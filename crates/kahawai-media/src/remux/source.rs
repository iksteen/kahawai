use super::*;

/// Byte source for a remux: sized and random-access, because real-world
/// containers demand seeks (MP4 with the moov atom at the end cannot be
/// demuxed as a forward-only stream — the demuxer must jump to the tail
/// for its index before streaming the data). The hub backs this with a
/// mediahost read lease; tools back it with a local file.
pub trait RemuxSource: Send + 'static {
    fn size(&self) -> u64;
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize>;
    fn read_at_cancelled(
        &mut self,
        offset: u64,
        buf: &mut [u8],
        cancel: &ReadCancellation,
    ) -> std::io::Result<usize> {
        cancel.check()?;
        let n = self.read_at(offset, buf)?;
        cancel.check()?;
        Ok(n)
    }
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

/// One demand's cancellation, independent of the source's blocking I/O lock.
#[derive(Default)]
pub struct ReadCancellation {
    cancelled: std::sync::atomic::AtomicBool,
    wake: Mutex<Option<Box<dyn Fn() + Send>>>,
}
impl ReadCancellation {
    pub fn check(&self) -> std::io::Result<()> {
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "source read cancelled",
            ))
        } else {
            Ok(())
        }
    }
    pub fn on_cancel(&self, wake: impl Fn() + Send + 'static) {
        let mut slot = self.wake.lock().unwrap();
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            wake();
        } else {
            *slot = Some(Box::new(wake));
        }
    }
    pub(crate) fn cancel(&self) {
        let mut wake = self.wake.lock().unwrap();
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(wake) = wake.take() {
            wake();
        }
    }
}
struct Demand {
    generation: u64,
    offset: u64,
    len: usize,
    cancel: Arc<ReadCancellation>,
}
#[derive(Default)]
struct FeedState {
    reported: bool,
    generation: u64,
    position: u64,
    pending: bool,
    closed: bool,
    active: Option<Arc<ReadCancellation>>,
}
/// Stop the source BEFORE setting the pipeline to NULL. This also releases a
/// blocked socket/lease read when GStreamer has no further demand to emit.
pub struct SourceGuard {
    state: Arc<Mutex<FeedState>>,
    commands: std::sync::mpsc::Sender<Option<Demand>>,
}
impl SourceGuard {
    pub fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        if let Some(cancel) = state.active.take() {
            cancel.cancel();
        }
        let _ = self.commands.send(None);
    }
}
impl Drop for SourceGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Random-access appsrc: demuxers pull exact byte ranges; network read-ahead
/// lives in the parent, not in a second worker-side ring.
pub fn seekable_appsrc(mut source: Box<dyn RemuxSource>) -> (AppSrc, SourceGuard) {
    // GStreamer: "a buffer of exactly the amount of bytes given by the need-data signal".
    // https://gstreamer.freedesktop.org/documentation/application-development/advanced/pipeline-manipulation.html
    let size = source.size();
    let appsrc = AppSrc::builder()
        .stream_type(gstreamer_app::AppStreamType::RandomAccess)
        .format(gst::Format::Bytes)
        .block(false)
        .max_bytes(0)
        .build();
    // In pull mode the demuxer creates stream IDs before a STREAM_START
    // arrives. GstPad uses the upstream URI for their common prefix; without
    // one it invents a different random prefix per track, and parsebin's
    // stream-ID sort swaps track indices. A source-local URI also separates parts.
    appsrc
        .set_uri(&format!("appsrc://kahawai/{}", appsrc.name()))
        .unwrap();
    appsrc.set_size(i64::try_from(size).expect("source exceeds GStreamer's signed size range"));
    let state = Arc::new(Mutex::new(FeedState::default()));
    let (tx, rx) = std::sync::mpsc::channel::<Option<Demand>>();
    let guard = SourceGuard {
        state: state.clone(),
        commands: tx.clone(),
    };
    let hangup = SourceGuard {
        state: state.clone(),
        commands: tx.clone(),
    };
    let need_state = state.clone();
    let seek_state = state.clone();
    appsrc.set_callbacks(gstreamer_app::AppSrcCallbacks::builder()
        .need_data(move |src, length| {
            let _ = &hangup;
            let mut s = need_state.lock().unwrap();
            if s.closed || s.pending { return; }
            if !s.reported {
                tracing::info!(mode = ?src.static_pad("src").map(|pad| pad.mode()), "appsrc scheduling");
                s.reported = true;
            }
            if length > 64 * 1024 * 1024 {
                gst::element_error!(src, gst::ResourceError::Read, ("source demand exceeds 64 MiB"));
                return;
            }
            let cancel = Arc::new(ReadCancellation::default());
            s.active = Some(cancel.clone());
            s.pending = true;
            let _ = tx.send(Some(Demand { generation: s.generation, offset: s.position,
                len: (size.saturating_sub(s.position)).min(length as u64) as usize, cancel }));
        })
        .seek_data(move |_, offset| {
            let mut s = seek_state.lock().unwrap();
            if s.closed || offset > size { return false; }
            if let Some(cancel) = s.active.take() { cancel.cancel(); }
            s.generation += 1;
            s.position = offset;
            s.pending = false;
            true
        }).build());
    let weak = appsrc.downgrade();
    std::thread::spawn(move || {
        while let Ok(Some(demand)) = rx.recv() {
            if demand.cancel.check().is_err() {
                continue;
            }
            let mut data = vec![0; demand.len];
            let result = (|| {
                let mut filled = 0;
                while filled < data.len() {
                    // The Unix bridge bounds each request at 8 MiB.
                    let end = data.len().min(filled.saturating_add(8 * 1024 * 1024));
                    let n = source.read_at_cancelled(
                        demand.offset + filled as u64,
                        &mut data[filled..end],
                        &demand.cancel,
                    )?;
                    if n == 0 || n > end - filled {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "source ended before its declared size",
                        ));
                    }
                    filled += n;
                }
                Ok::<_, std::io::Error>(())
            })();
            let Some(src) = weak.upgrade() else {
                return;
            };
            // Only publication holds this lock. Seeking never waits for I/O.
            let mut s = state.lock().unwrap();
            if s.closed {
                return;
            }
            if s.generation != demand.generation || demand.cancel.check().is_err() {
                continue;
            }
            s.pending = false;
            s.active = None;
            match result {
                Err(error) => {
                    gst::element_error!(
                        src,
                        gst::ResourceError::Read,
                        ("source read failed: {}", error)
                    );
                }
                Ok(()) if data.is_empty() => {
                    let _ = src.end_of_stream();
                }
                Ok(()) => {
                    let end = demand.offset + data.len() as u64;
                    let mut buffer = gst::Buffer::from_mut_slice(data);
                    let b = buffer.get_mut().unwrap();
                    b.set_offset(demand.offset);
                    b.set_offset_end(end);
                    if src.push_buffer(buffer).is_ok() {
                        s.position = end;
                    }
                }
            }
        }
    });
    (appsrc, guard)
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
        let (src, _guard) = seekable_appsrc(Box::new(Witnessed(released.clone())));
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

#[cfg(test)]
mod pull_tests {
    use super::*;
    struct ShortReads;
    impl RemuxSource for ShortReads {
        fn size(&self) -> u64 {
            8211
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf
                .len()
                .min(7)
                .min(self.size().saturating_sub(offset) as usize);
            for (i, b) in buf[..n].iter_mut().enumerate() {
                *b = ((offset + i as u64) % 251) as u8;
            }
            Ok(n)
        }
    }
    #[test]
    fn pull_scheduling_fills_short_reads_and_preserves_offsets() {
        crate::init().unwrap();
        let (src, guard) = seekable_appsrc(Box::new(ShortReads));
        src.set_state(gst::State::Ready).unwrap();
        let upstream = src.static_pad("src").unwrap();
        let consumer = gst::Pad::builder(gst::PadDirection::Sink).build();
        upstream.link(&consumer).unwrap();
        upstream.activate_mode(gst::PadMode::Pull, true).unwrap();
        consumer.activate_mode(gst::PadMode::Pull, true).unwrap();
        assert_eq!(upstream.mode(), gst::PadMode::Pull);
        for (offset, requested) in [(0, 4096), (4096, 4096), (8192, 4096), (37, 19), (1, 1)] {
            let buffer = consumer.pull_range(offset, requested).unwrap();
            let data = buffer.map_readable().unwrap();
            assert_eq!(data.len(), (8211 - offset).min(requested as u64) as usize);
            assert_eq!(buffer.offset(), offset);
            assert_eq!(buffer.offset_end(), offset + data.len() as u64);
            for (i, b) in data.iter().enumerate() {
                assert_eq!(*b, ((offset + i as u64) % 251) as u8);
            }
        }
        assert_eq!(consumer.pull_range(8211, 100), Err(gst::FlowError::Eos));
        guard.stop();
        consumer.activate_mode(gst::PadMode::Pull, false).unwrap();
        upstream.activate_mode(gst::PadMode::Pull, false).unwrap();
        src.set_state(gst::State::Null).unwrap();
    }

    struct Stalled {
        entered: std::sync::mpsc::Sender<()>,
    }
    impl RemuxSource for Stalled {
        fn size(&self) -> u64 {
            8192
        }
        fn read_at(&mut self, _: u64, _: &mut [u8]) -> std::io::Result<usize> {
            unreachable!()
        }
        fn read_at_cancelled(
            &mut self,
            _: u64,
            _: &mut [u8],
            cancel: &ReadCancellation,
        ) -> std::io::Result<usize> {
            let (tx, rx) = std::sync::mpsc::channel();
            cancel.on_cancel(move || {
                let _ = tx.send(());
            });
            self.entered.send(()).unwrap();
            rx.recv_timeout(std::time::Duration::from_secs(5))
                .expect("read was not cancelled");
            cancel.check()?;
            unreachable!()
        }
    }
    #[test]
    fn stopping_cancels_a_stalled_demand_before_pipeline_null() {
        crate::init().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (src, guard) = seekable_appsrc(Box::new(Stalled { entered: tx }));
        let sink = gst::ElementFactory::make("fakesink")
            .property("can-activate-pull", true)
            .property("can-activate-push", false)
            .build()
            .unwrap();
        let pipe = gst::Pipeline::new();
        pipe.add_many([src.upcast_ref::<gst::Element>(), &sink])
            .unwrap();
        src.link(&sink).unwrap();
        pipe.set_state(gst::State::Playing).unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let started = std::time::Instant::now();
        guard.stop();
        pipe.set_state(gst::State::Null).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
}
