//! Bounded, disposable transport read-ahead, not a persistent media cache.
//! Demand hits cost a memory copy; misses cost a remote seek/read. Retain
//! disjoint ranges for demuxers alternating audio/video offsets. Speculation
//! stops at capacity; only a demand miss may reclaim retained ranges.
//! Capacity and diagnostics count full retained allocations, including consumed
//! prefixes of partial blocks. Those prefixes remain readable: demuxers repeat
//! and overlap requests. This avoids copying on every small demand and
//! bounds RAM rather than just readable bytes. Reclamation is FIFO by insertion,
//! not cursor-relative: a backward miss can discard useful bytes ahead and pay
//! to fetch them again. No guarantee of retaining the nearest ranges is implied.
use kahawai_proto::v1::{ByteChunk, ReadRequest};
use std::{
    collections::VecDeque,
    future::Future,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, mpsc, watch};

pub const HUB_CAPACITY: usize = 16 * 1024 * 1024;
pub const TRANSCODER_CAPACITY: usize = 2 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
struct Block {
    offset: u64,
    data: Vec<u8>,
}
#[derive(Default)]
struct State {
    blocks: VecDeque<Block>,
    bytes: usize,
    generation: u64,
    next: u64,
    error: Option<String>,
    eof: bool,
    hits: u64,
    misses: u64,
    discarded: u64,
    received: u64,
    peak: usize,
    demand_wait_us: u128,
}
struct Shared {
    state: Mutex<State>,
    changed: Notify,
    serial: tokio::sync::Mutex<()>,
}
struct Lifetime(Vec<tokio::task::AbortHandle>);
impl Drop for Lifetime {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}
#[derive(Clone)]
pub struct ReadAhead {
    shared: Arc<Shared>,
    commands: watch::Sender<ReadRequest>,
    size: u64,
    capacity: usize,
    _lifetime: Arc<Lifetime>,
}
impl ReadAhead {
    pub fn new<F, Fut>(size: u64, capacity: usize, wire: F) -> Self
    where
        F: FnOnce(mpsc::Receiver<ReadRequest>, mpsc::Sender<ByteChunk>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        assert!(capacity >= super::source_stream::CHUNK);
        let (request_tx, request_rx) = mpsc::channel(2);
        let (chunk_tx, mut chunk_rx) = mpsc::channel::<ByteChunk>(2);
        let transport = tokio::spawn(wire(request_rx, chunk_tx));
        let (commands, mut changes) = watch::channel(ReadRequest::default());
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Notify::new(),
            serial: tokio::sync::Mutex::new(()),
        });
        let incoming = shared.clone();
        let receive = tokio::spawn(async move {
            let mut pending: Option<ByteChunk> = None;
            loop {
                let notified = incoming.changed.notified();
                tokio::pin!(notified);
                // A consumer may free capacity while we inspect the buffer.
                // Register now: notify_waiters does not retain a permit.
                notified.as_mut().enable();
                let can_receive = {
                    let mut state = incoming.state.lock().unwrap();
                    if let Some(chunk) = pending.take() {
                        if chunk.generation == 0 && !chunk.error.is_empty() {
                            state.error = Some(chunk.error);
                            incoming.changed.notify_waiters();
                        } else if chunk.generation != state.generation {
                            state.discarded += chunk.data.len() as u64;
                        } else if !chunk.error.is_empty() {
                            state.error = Some(chunk.error);
                            incoming.changed.notify_waiters();
                        } else if chunk.offset != state.next
                            || chunk.data.len() > super::source_stream::CHUNK
                            || chunk.data.len() as u64 > size.saturating_sub(chunk.offset)
                            || (!chunk.eof && chunk.data.is_empty())
                            || (chunk.eof && !chunk.data.is_empty())
                        {
                            state.error = Some("invalid source chunk offset or length".into());
                            incoming.changed.notify_waiters();
                        } else {
                            // A repositioned stream may cross retained ranges.
                            // Replace overlaps instead of pinning duplicate, already
                            // consumed blocks behind the demand cursor.
                            let end = chunk.offset.saturating_add(chunk.data.len() as u64);
                            let mut removed = 0;
                            state.blocks.retain(|block| {
                                let overlaps = block.offset < end
                                    && chunk.offset < block.offset + block.data.len() as u64;
                                if overlaps {
                                    removed += block.data.len();
                                }
                                !overlaps
                            });
                            state.bytes -= removed;
                            if state.bytes + chunk.data.len() <= capacity {
                                if chunk.eof {
                                    state.eof = true;
                                    if state.next != size {
                                        state.error =
                                            Some("source ended before its declared size".into());
                                    }
                                } else if !chunk.data.is_empty() {
                                    state.next += chunk.data.len() as u64;
                                    state.bytes += chunk.data.len();
                                    state.peak = state.peak.max(state.bytes);
                                    state.received += chunk.data.len() as u64;
                                    state.blocks.push_back(Block {
                                        offset: chunk.offset,
                                        data: chunk.data,
                                    });
                                }
                                incoming.changed.notify_waiters();
                            } else {
                                pending = Some(chunk);
                            }
                        }
                    }
                    pending.is_none()
                };
                tokio::select! {
                    biased;
                    changed = changes.changed() => {
                        if changed.is_err() { return; }
                        let req = *changes.borrow_and_update();
                        if request_tx.send(req).await.is_err() { break; }
                    }
                    _ = notified => {},
                    chunk = chunk_rx.recv(), if can_receive => match chunk { Some(c) => pending = Some(c), None => break },
                }
            }
            incoming.state.lock().unwrap().error = Some("source channel closed".into());
            incoming.changed.notify_waiters();
        });
        Self {
            shared,
            commands,
            size,
            capacity,
            _lifetime: Arc::new(Lifetime(vec![
                transport.abort_handle(),
                receive.abort_handle(),
            ])),
        }
    }

    pub fn diagnostics(&self) -> String {
        let s = self.shared.state.lock().unwrap();
        format!(
            "capacity={} buffered={} peak={} generation={} hits={} misses={} received={} discarded={} demand_wait_us={} error={:?}",
            self.capacity,
            s.bytes,
            s.peak,
            s.generation,
            s.hits,
            s.misses,
            s.received,
            s.discarded,
            s.demand_wait_us,
            s.error
        )
    }

    pub async fn read(&self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let _serial = self.shared.serial.lock().await;
        let len = len.min(self.size.saturating_sub(offset));
        let mut out = Vec::new();
        let started = std::time::Instant::now();
        while (out.len() as u64) < len {
            let pos = offset + out.len() as u64;
            let changed = self.shared.changed.notified();
            // Register before inspecting state: notify_waiters does not retain permits.
            tokio::pin!(changed);
            changed.as_mut().enable();
            let got = {
                let mut state = self.shared.state.lock().unwrap();
                if let Some(error) = &state.error {
                    return Err(io::Error::other(error.clone()));
                }
                if let Some(index) = state
                    .blocks
                    .iter()
                    .position(|b| b.offset <= pos && pos - b.offset < b.data.len() as u64)
                {
                    let block = state.blocks.remove(index).unwrap();
                    state.bytes -= block.data.len();
                    let skip = (pos - block.offset) as usize;
                    let n = (len as usize - out.len()).min(block.data.len() - skip);
                    out.extend_from_slice(&block.data[skip..skip + n]);
                    if skip + n < block.data.len() {
                        state.bytes += block.data.len();
                        state.blocks.insert(index, block);
                    }
                    state.hits += 1;
                    self.shared.changed.notify_waiters();
                    true
                } else {
                    // Demand must be able to admit the next chunk even when
                    // the producer is already at the requested position.
                    let reserve = (self.capacity / 8).max(super::source_stream::CHUNK);
                    let before = state.bytes;
                    while state.bytes > self.capacity - reserve {
                        let block = state.blocks.pop_front().unwrap();
                        state.bytes -= block.data.len();
                        state.discarded += block.data.len() as u64;
                    }
                    if state.bytes != before {
                        self.shared.changed.notify_waiters();
                    }
                    if state.generation == 0 || pos != state.next || state.eof {
                        state.generation += 1;
                        state.next = pos;
                        state.eof = false;
                        state.misses += 1;
                        tracing::debug!(
                            generation = state.generation,
                            offset = pos,
                            buffered = state.bytes,
                            hits = state.hits,
                            misses = state.misses,
                            discarded = state.discarded,
                            "source read-ahead seek"
                        );
                        self.commands.send_replace(ReadRequest {
                            offset: pos,
                            len: self.size - pos,
                            generation: state.generation,
                        });
                    }
                    false
                }
            };
            if !got {
                tokio::time::timeout(TIMEOUT, changed).await.map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "source demand timed out")
                })?;
            }
        }
        self.shared.state.lock().unwrap().demand_wait_us += started.elapsed().as_micros();
        tracing::trace!(
            offset,
            len,
            wait_us = started.elapsed().as_micros(),
            "source demand answered"
        );
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_stream::{CHUNK, serve};
    use std::sync::atomic::{AtomicUsize, Ordering};
    fn data(offset: u64, len: usize) -> Vec<u8> {
        (offset..offset + len as u64)
            .map(|n| (n % 251) as u8)
            .collect()
    }
    fn source(capacity: usize) -> (ReadAhead, Arc<AtomicUsize>) {
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        let buffer = ReadAhead::new(32 * CHUNK as u64, capacity, move |rx, tx| async move {
            serve(rx, tx, 32 * CHUNK as u64, |offset, len| {
                count.fetch_add(1, Ordering::SeqCst);
                async move { Ok(data(offset, len)) }
            })
            .await;
        });
        (buffer, reads)
    }
    async fn bounded(buffer: &ReadAhead, offset: u64, len: u64) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(2), buffer.read(offset, len))
            .await
            .expect("demand hung")
            .unwrap()
    }
    #[tokio::test]
    async fn reads_larger_than_capacity_and_eof_are_exact() {
        let (buffer, _) = source(2 * CHUNK);
        assert_eq!(
            bounded(&buffer, 13, 7 * CHUNK as u64).await,
            data(13, 7 * CHUNK)
        );
        assert_eq!(
            bounded(&buffer, 32 * CHUNK as u64 - 9, 30).await,
            data(32 * CHUNK as u64 - 9, 9)
        );
        assert!(bounded(&buffer, u64::MAX, 42).await.is_empty());
    }
    #[tokio::test]
    async fn full_buffer_stops_reads_and_seek_preempts_blocked_delivery() {
        let capacity = 2 * CHUNK;
        let (buffer, reads) = source(capacity);
        assert_eq!(bounded(&buffer, 0, 1).await, data(0, 1));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let before = reads.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            before,
            reads.load(Ordering::SeqCst),
            "producer kept reading under backpressure"
        );
        assert!(buffer.shared.state.lock().unwrap().bytes <= capacity);
        let target = 20 * CHUNK as u64 + 17;
        assert_eq!(bounded(&buffer, target, 99).await, data(target, 99));
        assert!(buffer.shared.state.lock().unwrap().bytes <= capacity);
        assert_eq!(bounded(&buffer, 1, 17).await, data(1, 17));
    }
    #[tokio::test]
    async fn buffered_demand_does_not_issue_a_seek() {
        let (buffer, _) = source(4 * CHUNK);
        bounded(&buffer, 0, 1).await;
        let generation = buffer.shared.state.lock().unwrap().generation;
        assert_eq!(bounded(&buffer, 10, 33).await, data(10, 33));
        assert_eq!(buffer.shared.state.lock().unwrap().generation, generation);
    }
    #[tokio::test]
    async fn overlapping_demux_reads_reuse_retained_prefixes() {
        let (buffer, _) = source(4 * CHUNK);
        bounded(&buffer, 0, 4096).await;
        let generation = buffer.shared.state.lock().unwrap().generation;
        // Typefinding rereads the header, then scans with overlapping windows.
        // Matroska also expands a short header read into a larger block read.
        for (offset, len) in [
            (0, 4096),
            (4032, 4096),
            (8064, 4096),
            (8982, 65536),
            (8982, 100000),
        ] {
            assert_eq!(
                bounded(&buffer, offset, len).await,
                data(offset, len as usize)
            );
            assert_eq!(
                buffer.shared.state.lock().unwrap().generation,
                generation,
                "resident bytes at {offset}+{len} caused a network seek"
            );
        }
    }
    #[tokio::test]
    async fn demand_at_full_prefetch_boundary_reclaims_space_without_seeking() {
        let capacity = 2 * CHUNK;
        let (buffer, _) = source(capacity);
        bounded(&buffer, 0, 1).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while buffer.shared.state.lock().unwrap().bytes != capacity {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let generation = buffer.shared.state.lock().unwrap().generation;
        // A skip within the window remains a hit; keep both allocations full.
        assert_eq!(bounded(&buffer, 10, 33).await, data(10, 33));
        assert_eq!(buffer.shared.state.lock().unwrap().bytes, capacity);
        // The demand crosses the full window's edge and spans multiple refills.
        let offset = capacity as u64 - 17;
        assert_eq!(
            bounded(&buffer, offset, 3 * CHUNK as u64).await,
            data(offset, 3 * CHUNK)
        );
        assert_eq!(buffer.shared.state.lock().unwrap().generation, generation);
        assert!(buffer.shared.state.lock().unwrap().peak <= capacity);
    }

    #[tokio::test]
    async fn demand_skipping_to_full_prefetch_boundary_does_not_wait_for_eviction() {
        let capacity = 2 * CHUNK;
        let (buffer, _) = source(capacity);
        bounded(&buffer, 0, 1).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while buffer.shared.state.lock().unwrap().bytes != capacity {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let generation = buffer.shared.state.lock().unwrap().generation;
        assert_eq!(
            bounded(&buffer, capacity as u64, 1).await,
            data(capacity as u64, 1)
        );
        assert_eq!(buffer.shared.state.lock().unwrap().generation, generation);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_consumer_keeps_waking_a_full_buffer() {
        let (buffer, _) = source(CHUNK);
        for offset in (0..32 * CHUNK).step_by(4096) {
            assert_eq!(
                bounded(&buffer, offset as u64, 4096).await,
                data(offset as u64, 4096)
            );
        }
        assert!(buffer.shared.state.lock().unwrap().peak <= CHUNK);
    }
    #[tokio::test]
    async fn error_is_not_eof_and_drop_releases_transport() {
        struct Witness(Arc<AtomicUsize>);
        impl Drop for Witness {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let witness = Witness(dropped.clone());
        let buffer = ReadAhead::new(99, CHUNK, move |mut requests, chunks| async move {
            let _witness = witness;
            let req = requests.recv().await.unwrap();
            chunks
                .send(ByteChunk {
                    generation: req.generation,
                    error: "disk failed".into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        assert!(
            buffer
                .read(0, 10)
                .await
                .unwrap_err()
                .to_string()
                .contains("disk failed")
        );
        drop(buffer);
        tokio::time::timeout(Duration::from_secs(1), async {
            while dropped.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
