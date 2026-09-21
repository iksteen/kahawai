use super::*;

/// Byte source for a remux: sized and random-access, because real-world
/// containers demand seeks (MP4 with the moov atom at the end cannot be
/// demuxed as a forward-only stream — the demuxer must jump to the tail
/// for its index before streaming the data). The hub backs this with a
/// mediahost read lease; tools back it with a local file.
pub trait RemuxSource: Send + 'static {
    fn size(&self) -> u64;
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize>;
}

/// Local-file source (sweep tool, tests).
pub struct FileSource {
    file: std::fs::File,
    size: u64,
}

impl FileSource {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self { file, size })
    }
}

impl RemuxSource for FileSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::{Read, Seek, SeekFrom};
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read(buf)
    }
}

pub(super) enum FeedCmd {
    /// Feed generation at request time — a Need stamped before a seek
    /// must never be served after it: with slow sources (lease/socket)
    /// a stale block can land after flush-stop and push old-position
    /// bytes into the new segment, which the demuxer then parses as
    /// garbage ("large block, file might be corrupt"). The byte count
    /// appsrc asks for is irrelevant since the prefetch ring sized its
    /// blocks already.
    Need(u64),
}

/// A seekable appsrc fed from a `RemuxSource` through a PREFETCH RING:
/// a reader thread streams ahead of the pipeline into a bounded buffer,
/// and stalls when it is full — "stream until pushback", expressed
/// locally instead of as flow-control games on the shared control link
/// (AR-12: never head-of-line-block the heartbeats).
///
/// Why a ring at all: the old feeder read one 256 KB block per appsrc
/// Need, serially — and for a dispatched worker every block is a full
/// worker→transcoder→hub→lease round trip. Measured on a 4K HDR title:
/// the byte plane delivered ~2 MB/s (≈ the file's own bitrate), capping
/// EVERY session near 1.0× realtime while the same pipeline ran 4–6.6×
/// against a local file — a video COPY session crawled identically,
/// which is what convicted transport over compute. Large blocks
/// amortize the round trip; the ring overlaps fetch with the pipeline.
///
/// Seek correctness is generation-based, as before: a block read for
/// generation N is dropped once a seek bumps to N+1, so a slow in-
/// flight read can never land pre-seek bytes after flush-stop.
/// A source element that reads a `RemuxSource` on demand, seeks included.
/// Public because intro detection runs the same way the remuxer does: in the
/// hub, over a mediahost lease, for a file it cannot open by name.
pub fn seekable_appsrc(mut source: Box<dyn RemuxSource>) -> AppSrc {
    /// One fetch, sized to amortize the byte-plane round trip.
    const READ_BLOCK: usize = 2 * 1024 * 1024;
    /// Ring capacity — the pushback point. 16 MB ≈ 9 s of a 4K HDR
    /// film ahead of the pipeline, bounded per part-source.
    const RING_BYTES: usize = 16 * 1024 * 1024;

    struct Ring {
        blocks: std::collections::VecDeque<(u64, Vec<u8>)>,
        bytes: usize,
        /// Bumped by seek_data; blocks and Needs from an older
        /// generation are stale and dropped.
        generation: u64,
        /// Where the reader resumes after a seek.
        seek_to: Option<u64>,
        /// Reader reached EOF (for the current generation).
        eos: bool,
        /// Reader hit a fatal read error.
        failed: bool,
        /// The appsrc is gone: its callbacks were dropped with the
        /// element, and the ring must let the reader thread — and the
        /// SOURCE it owns — go. Without this the thread parked on the
        /// condvar for ever, and over the byte plane the source is a
        /// LEASE: the mediahost held one open file per leaked reader
        /// until the box ran out of file descriptors (os error 24,
        /// measured at exactly the 1024 cap during a sweep).
        closed: bool,
    }
    let ring = Arc::new((
        Mutex::new(Ring {
            blocks: std::collections::VecDeque::new(),
            bytes: 0,
            generation: 0,
            seek_to: None,
            eos: false,
            failed: false,
            closed: false,
        }),
        std::sync::Condvar::new(),
    ));

    let appsrc = AppSrc::builder()
        .stream_type(gstreamer_app::AppStreamType::Seekable)
        .block(true)
        .max_bytes(8 * 1024 * 1024)
        .build();
    appsrc.set_size(source.size() as i64);

    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<FeedCmd>();
    // Held by the feeder for the duration of each Need. seek_data takes
    // it after bumping the generation: any in-flight feed then finishes
    // inside the flush (its push fails Flushing — appsrc unblocks
    // producers before invoking seek_data), so a stale block can never
    // land after flush-stop.
    let busy: Arc<Mutex<()>> = Arc::new(Mutex::new(()));

    // Owned by the need_data callback, so it lives exactly as long as the
    // element: when the pipeline is torn down and the appsrc finalized,
    // the drop hangs up the ring and the reader thread returns, releasing
    // the source (and any lease inside it) with it.
    struct Hangup(Arc<(Mutex<Ring>, std::sync::Condvar)>);
    impl Drop for Hangup {
        fn drop(&mut self) {
            let (lock, cv) = &*self.0;
            lock.lock().unwrap().closed = true;
            cv.notify_all();
        }
    }
    let hangup = Hangup(ring.clone());

    let ring_need = ring.clone();
    let ring_seek = ring.clone();
    let busy_seek = busy.clone();
    appsrc.set_callbacks(
        gstreamer_app::AppSrcCallbacks::builder()
            .need_data(move |_, _length| {
                let _ = &hangup;
                let stamp = ring_need.0.lock().unwrap().generation;
                let _ = cmd_tx.send(FeedCmd::Need(stamp));
            })
            .seek_data(move |_, offset| {
                {
                    let (lock, cv) = &*ring_seek;
                    let mut r = lock.lock().unwrap();
                    r.generation += 1;
                    r.seek_to = Some(offset);
                    r.blocks.clear();
                    r.bytes = 0;
                    r.eos = false;
                    cv.notify_all();
                }
                drop(busy_seek.lock().unwrap());
                true
            })
            .build(),
    );

    // Reader: streams ahead until the ring pushes back. Owns the source.
    let ring_rd = ring.clone();
    std::thread::spawn(move || {
        let mut pos: u64 = 0;
        let mut my_gen: u64 = 0;
        loop {
            // Wait for room (or a reason to reposition/stop).
            {
                let (lock, cv) = &*ring_rd;
                let mut r = lock.lock().unwrap();
                loop {
                    if r.closed || r.failed {
                        return;
                    }
                    if r.generation != my_gen {
                        my_gen = r.generation;
                        if let Some(t) = r.seek_to.take() {
                            pos = t;
                        }
                        break;
                    }
                    if !r.eos && r.bytes < RING_BYTES {
                        break;
                    }
                    r = cv.wait(r).unwrap();
                }
                if r.eos {
                    continue; // parked until a seek revives us
                }
            }
            let mut buf = vec![0u8; READ_BLOCK];
            let result = source.read_at(pos, &mut buf);
            let (lock, cv) = &*ring_rd;
            let mut r = lock.lock().unwrap();
            if r.closed {
                return; // torn down while we were reading
            }
            if r.generation != my_gen {
                continue; // seek raced the read: bytes are stale
            }
            match result {
                Ok(0) => {
                    r.eos = true;
                    cv.notify_all();
                }
                Ok(n) => {
                    buf.truncate(n);
                    r.bytes += n;
                    r.blocks.push_back((pos, buf));
                    pos += n as u64;
                    cv.notify_all();
                }
                Err(e) => {
                    tracing::warn!(error = %e, "remux source read failed; ending stream");
                    r.failed = true;
                    cv.notify_all();
                }
            }
        }
    });

    // Feeder: serves appsrc Needs from the ring. No I/O of its own.
    //
    // A WEAK element ref, or nothing here ever dies: a strong clone kept
    // the element alive, the element owned the callbacks, the callbacks
    // owned this thread's channel (and the ring's hangup), and the thread
    // waited on that channel — a cycle in which the reader's source, and
    // the lease inside it, leaked with every torn-down pipeline.
    let feeder_src = appsrc.downgrade();
    let ring_fd = ring;
    std::thread::spawn(move || {
        while let Ok(FeedCmd::Need(stamp)) = cmd_rx.recv() {
            let _busy = busy.lock().unwrap();
            let block = {
                let (lock, cv) = &*ring_fd;
                let mut r = lock.lock().unwrap();
                loop {
                    if r.closed {
                        return; // the element is gone; nobody wants this Need
                    }
                    if r.generation != stamp {
                        break None; // stamped before a seek: stale, drop
                    }
                    if let Some((off, bytes)) = r.blocks.pop_front() {
                        r.bytes -= bytes.len();
                        cv.notify_all(); // room: wake the reader
                        break Some(Ok((off, bytes)));
                    }
                    if r.eos {
                        break Some(Err(true));
                    }
                    if r.failed {
                        break Some(Err(false));
                    }
                    r = cv.wait(r).unwrap();
                }
            };
            let Some(feeder_src) = feeder_src.upgrade() else {
                return; // element finalized between Needs
            };
            match block {
                None => continue,
                Some(Err(_eos_or_fail)) => {
                    let _ = feeder_src.end_of_stream();
                }
                Some(Ok((offset, bytes))) => {
                    // Push in slices, never one buffer per read.
                    //
                    // gst_avi_demux_chain() advances exactly ONE state
                    // per buffer — START parses the RIFF header and
                    // returns, HEADER needs the next buffer — so a file
                    // that arrives whole in a single buffer never gets
                    // past the header and dies at EOS with "got eos and
                    // didn't receive a complete header object".
                    // Measured on a 1.5 MiB AVI, same bytes: pushed as
                    // one buffer it fails, as two it demuxes. Only
                    // sources smaller than READ_BLOCK could hit it,
                    // which is why it took a test fixture to find.
                    //
                    // The slices share the read's memory (copy_region
                    // takes a reference, not a copy), so this costs
                    // buffer headers and nothing else.
                    const MAX_PUSH: usize = 256 * 1024;
                    let len = bytes.len();
                    let whole = gst::Buffer::from_mut_slice(bytes);
                    let chunk = MAX_PUSH.min(len.div_ceil(4)).max(1);
                    let mut at = 0usize;
                    while at < len {
                        let n = chunk.min(len - at);
                        let Ok(mut b) = whole.copy_region(gst::BufferCopyFlags::MEMORY, at..at + n)
                        else {
                            break;
                        };
                        b.get_mut().unwrap().set_offset(offset + at as u64);
                        // Err = Flushing (seek in progress) or shutdown;
                        // either a new Need follows or recv fails.
                        if feeder_src.push_buffer(b).is_err() {
                            break;
                        }
                        at += n;
                    }
                }
            }
        }
    });
    appsrc
}

#[cfg(test)]
mod appsrc_teardown {
    //! The reader thread OWNS the source, and over the byte plane the
    //! source is a lease holding a file open on the mediahost. This pins
    //! that tearing the appsrc down actually releases it: before the
    //! hangup guard, the reader parked on the ring's condvar for ever and
    //! the NAS ran out of file descriptors mid-sweep (os error 24 at
    //! exactly the 1024 cap, one leaked file per probe).
    use super::*;

    struct Witnessed(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl RemuxSource for Witnessed {
        fn size(&self) -> u64 {
            64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = (64u64.saturating_sub(offset) as usize).min(buf.len());
            buf[..n].fill(0);
            Ok(n)
        }
    }
    impl Drop for Witnessed {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn dropping_the_appsrc_releases_the_source() {
        crate::init().unwrap();
        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let src = seekable_appsrc(Box::new(Witnessed(released.clone())));
        // Let the reader hit EOF and park — the state it leaked in.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!released.load(std::sync::atomic::Ordering::SeqCst));
        drop(src);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !released.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "the reader thread still holds the source after teardown"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
