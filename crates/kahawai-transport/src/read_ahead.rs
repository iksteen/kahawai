//! Two independently backpressured source generations, each with a reserved
//! half of the per-source memory budget. A hit costs a copy; a miss costs a
//! remote reposition/read. Audio/video alternation must not cancel the other
//! stream or release its unread bytes. Independent byte channels prevent a full
//! speculative window from blocking delivery to the demanded window.
//! BiReadAhead reuses resident bytes before assigning a producer. Two recent
//! demand positions guide release behind playback independently of the physical
//! supplier; nearby cursors can share one producer without pruning each other.
//! Hints never pin allocations: a miss can reclaim least-recently-read blocks.
//! A standalone ReadAhead releases blocks before its own read's start.
//! Full allocations (including consumed prefixes) count against capacity. A miss
//! reserves admission space, including when it reaches the prefetch boundary.
//! Thus large reads progress without needing space from the other window.
use kahawai_proto::v1::{ByteChunk, ReadRequest};
use std::{
    collections::VecDeque,
    future::Future,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, mpsc, watch};

pub const HUB_CAPACITY: usize = 32 * 1024 * 1024;
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
        self.read_with_retention(offset, len, offset, false).await
    }

    async fn read_with_retention(
        &self,
        offset: u64,
        len: u64,
        retain_from: u64,
        retain: bool,
    ) -> io::Result<Vec<u8>> {
        let _serial = self.shared.serial.lock().await;
        let len = len.min(self.size.saturating_sub(offset));
        let mut out = Vec::new();
        if len != 0 {
            let mut state = self.shared.state.lock().unwrap();
            let mut removed = 0;
            state.blocks.retain(|block| {
                let before = block.offset + block.data.len() as u64 <= retain_from;
                if before {
                    removed += block.data.len();
                }
                !before
            });
            state.bytes -= removed;
            if removed != 0 {
                self.shared.changed.notify_waiters();
            }
        }
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
                    let skip = (pos - block.offset) as usize;
                    let n = (len as usize - out.len()).min(block.data.len() - skip);
                    out.extend_from_slice(&block.data[skip..skip + n]);
                    state.blocks.push_back(block);
                    state.hits += 1;
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
                    // Bounded forward skips can wait for the active stream.
                    // Restarting here wastes in-flight bytes; admission
                    // above ensures skipped bytes cannot fill and pin the window.
                    let horizon = if retain {
                        self.capacity
                    } else {
                        super::source_stream::CHUNK
                    };
                    let approaching = pos >= state.next && pos - state.next < horizon as u64;
                    if state.generation == 0 || !approaching || state.eof {
                        state.generation += 1;
                        state.discarded += state.bytes as u64;
                        state.blocks.clear();
                        state.bytes = 0;
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

/// Two independently owned wires; neither can consume the other's reservation.
#[derive(Clone)]
pub struct BiReadAhead {
    windows: [ReadAhead; 2],
    selection: Arc<tokio::sync::Mutex<Selection>>,
}
#[derive(Default)]
struct Selection {
    clock: u64,
    used: [u64; 2],
    cursor: [u64; 2],
    heads: [Option<u64>; 2],
    head_used: [u64; 2],
}
impl BiReadAhead {
    pub fn new(windows: [ReadAhead; 2]) -> Self {
        assert_eq!(windows[0].size, windows[1].size);
        assert_eq!(windows[0].capacity, windows[1].capacity);
        Self {
            windows,
            selection: Arc::new(tokio::sync::Mutex::new(Selection::default())),
        }
    }
    pub fn diagnostics(&self) -> String {
        format!(
            "window 0: {}\nwindow 1: {}",
            self.windows[0].diagnostics(),
            self.windows[1].diagnostics()
        )
    }
    pub async fn read(&self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let mut select = self.selection.lock().await;
        if len == 0 || offset >= self.windows[0].size {
            return Ok(Vec::new());
        }
        // Logical demand positions are independent of physical suppliers. Track
        // the nearest position within one transport chunk; larger jumps replace
        // the least recently used position instead of dragging a cursor forward
        // and prematurely releasing bytes needed by a trailing reader.
        // This is a retention hint, not a track identity or a pinned allocation:
        // misses can always reclaim space if either hint becomes stale.
        let head = (0..2)
            .filter_map(|i| select.heads[i].map(|pos| (i, pos)))
            .filter(|&(_, pos)| pos.abs_diff(offset) < super::source_stream::CHUNK as u64)
            .min_by_key(|&(_, pos)| pos.abs_diff(offset))
            .map(|(i, _)| i)
            .unwrap_or_else(|| {
                if select.head_used[0] <= select.head_used[1] {
                    0
                } else {
                    1
                }
            });
        select.clock += 1;
        select.heads[head] = Some(offset);
        select.head_used[head] = select.clock;
        let retain_from = select
            .heads
            .iter()
            .flatten()
            .copied()
            .filter(|&pos| pos <= offset && offset - pos < self.windows[0].capacity as u64)
            .min()
            .unwrap_or(offset)
            // Positions within a chunk are deliberately coalesced above. Keep
            // that much history too, including across an allocation boundary.
            .saturating_sub(super::source_stream::CHUNK as u64);
        // A resident byte wins over a forward continuation, regardless of which
        // cursor previously used it. Retention below makes that sharing safe:
        // reading ahead must not prune a trailing cursor's blocks. Nearby cursors
        // can therefore share one producer instead of fetching the same advancing
        // region twice. Distant regions still reserve independent windows.
        let hit = (0..2)
            .filter_map(|i| {
                let state = self.windows[i].shared.state.lock().unwrap();
                let resident = state
                    .blocks
                    .iter()
                    .any(|b| b.offset <= offset && offset - b.offset < b.data.len() as u64);
                let approaching = state.generation != 0
                    && !state.eof
                    && offset >= state.next
                    && offset - state.next < self.windows[i].capacity as u64;
                (resident || approaching).then_some((i, resident))
            })
            .min_by_key(|&(i, resident)| {
                (
                    !resident,
                    // If streams have converged, stick with the most recently
                    // used resident supplier. Cursor affinity here would keep
                    // both producers traversing the same bytes indefinitely.
                    if resident {
                        0
                    } else {
                        select.cursor[i].abs_diff(offset)
                    },
                    std::cmp::Reverse(select.used[i]),
                )
            })
            .map(|(i, _)| i);
        let window = hit.unwrap_or_else(|| {
            if select.used[0] <= select.used[1] {
                0
            } else {
                1
            }
        });
        select.clock += 1;
        select.used[window] = select.clock;
        select.cursor[window] = offset;
        tracing::trace!(window, offset, len, "bi-generational demand");
        self.windows[window]
            .read_with_retention(offset, len, retain_from, true)
            .await
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
    async fn repeated_boundary_and_multiblock_reads_remain_resident() {
        let (buffer, _) = source(4 * CHUNK);
        bounded(&buffer, 0, 1).await;
        let offset = CHUNK as u64 - 4096;
        for len in [4096, CHUNK as u64 + 4096] {
            assert_eq!(
                bounded(&buffer, offset, len).await,
                data(offset, len as usize)
            );
            let generation = buffer.shared.state.lock().unwrap().generation;
            assert_eq!(
                bounded(&buffer, offset, len).await,
                data(offset, len as usize)
            );
            assert_eq!(buffer.shared.state.lock().unwrap().generation, generation);
        }
        // Within a window, forward demand releases earlier blocks.
        bounded(&buffer, CHUNK as u64, CHUNK as u64).await;
        let state = buffer.shared.state.lock().unwrap();
        assert!(!state.blocks.iter().any(|b| b.offset == 0));
        assert!(state.blocks.iter().any(|b| b.offset == CHUNK as u64));
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
    async fn two_generations_advance_without_cancelling_or_starving_each_other() {
        let (a, _) = source(2 * CHUNK);
        let (b, _) = source(2 * CHUNK);
        let buffer = BiReadAhead::new([a, b]);
        for i in 0..8 {
            for offset in [i * CHUNK as u64, (16 + i) * CHUNK as u64] {
                let bytes =
                    tokio::time::timeout(Duration::from_secs(2), buffer.read(offset, CHUNK as u64))
                        .await
                        .expect("one full window starved the other")
                        .unwrap();
                assert_eq!(bytes, data(offset, CHUNK));
            }
        }
        for window in &buffer.windows {
            let state = window.shared.state.lock().unwrap();
            assert_eq!(
                state.generation, 1,
                "alternation restarted an active stream"
            );
            assert!(state.peak <= 2 * CHUNK);
        }
        // A third region replaces one window, not both.
        assert_eq!(
            buffer.read(30 * CHUNK as u64, 19).await.unwrap(),
            data(30 * CHUNK as u64, 19)
        );
        assert_eq!(
            buffer.read(23 * CHUNK as u64, 19).await.unwrap(),
            data(23 * CHUNK as u64, 19)
        );
        assert_eq!(buffer.windows[1].shared.state.lock().unwrap().generation, 1);
    }

    #[tokio::test]
    async fn consecutive_forward_skip_keeps_its_window_and_the_other_stream() {
        let (a, _) = source(2 * CHUNK);
        let (b, _) = source(2 * CHUNK);
        let buffer = BiReadAhead::new([a, b]);
        buffer.read(0, 32).await.unwrap();
        buffer.read(16 * CHUNK as u64, 32).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while buffer
                .windows
                .iter()
                .any(|w| w.shared.state.lock().unwrap().bytes != 2 * CHUNK)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Audio makes two consecutive reads, the second just beyond its current
        // prefetch frontier. Replacing the LRU window here cancels VIDEO, swaps
        // the streams, and makes the next video demand cancel audio in turn.
        buffer.read(64, 32).await.unwrap();
        let target = 2 * CHUNK as u64 + 17;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), buffer.read(target, 32))
                .await
                .unwrap()
                .unwrap(),
            data(target, 32)
        );
        assert_eq!(
            buffer.read(16 * CHUNK as u64 + 64, 32).await.unwrap(),
            data(16 * CHUNK as u64 + 64, 32)
        );
        for window in &buffer.windows {
            assert_eq!(window.shared.state.lock().unwrap().generation, 1);
        }
    }

    #[tokio::test]
    async fn nearby_cursors_share_fetches_without_pruning_each_others_bytes() {
        let intervals = Arc::new(Mutex::new(Vec::new()));
        let size = 64 * CHUNK as u64;
        let windows = std::array::from_fn(|_| {
            let intervals = intervals.clone();
            ReadAhead::new(size, 8 * CHUNK, move |rx, tx| async move {
                serve(rx, tx, size, |offset, len| {
                    intervals.lock().unwrap().push((offset, len));
                    async move { Ok(data(offset, len)) }
                })
                .await;
            })
        });
        let buffer = BiReadAhead::new(windows);
        // The leading demand arrives before prefetch necessarily reaches it.
        // Both cursors move much farther than the entire retention allowance.
        for i in 0..48 {
            for offset in [i * CHUNK as u64, (i + 3) * CHUNK as u64] {
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(2), buffer.read(offset, 4096))
                        .await
                        .expect("nearby cursor stalled")
                        .unwrap(),
                    data(offset, 4096)
                );
            }
        }
        let mut intervals = intervals.lock().unwrap().clone();
        intervals.sort_unstable();
        assert!(
            intervals.len() >= 51,
            "fixture did not advance through the file"
        );
        for pair in intervals.windows(2) {
            assert!(
                pair[0].0 + pair[0].1 as u64 <= pair[1].0,
                "duplicate upstream file reads: {pair:?}"
            );
        }
        assert_eq!(buffer.windows[0].shared.state.lock().unwrap().generation, 1);
        assert_eq!(buffer.windows[1].shared.state.lock().unwrap().generation, 0);
        assert!(
            buffer
                .windows
                .iter()
                .all(|w| w.shared.state.lock().unwrap().peak <= w.capacity)
        );
    }

    #[tokio::test]
    async fn shared_cursor_hint_preserves_reads_across_block_boundaries() {
        let (a, _) = source(4 * CHUNK);
        let (b, _) = source(4 * CHUNK);
        let buffer = BiReadAhead::new([a, b]);
        buffer.read(0, 32).await.unwrap();
        for i in 1..20 {
            let boundary = i * CHUNK as u64;
            for offset in [boundary - 4096, boundary + 32, boundary - 4096] {
                assert_eq!(buffer.read(offset, 4096).await.unwrap(), data(offset, 4096));
            }
        }
        assert_eq!(buffer.windows[0].shared.state.lock().unwrap().generation, 1);
        assert_eq!(buffer.windows[1].shared.state.lock().unwrap().generation, 0);
    }

    #[tokio::test]
    async fn converging_producers_stop_fetching_the_same_advancing_region() {
        let size = 128 * CHUNK as u64;
        let counts: [Arc<AtomicUsize>; 2] = std::array::from_fn(|_| Arc::new(AtomicUsize::new(0)));
        let windows = std::array::from_fn(|i| {
            let count = counts[i].clone();
            ReadAhead::new(size, 8 * CHUNK, move |rx, tx| async move {
                serve(rx, tx, size, |offset, len| {
                    count.fetch_add(1, Ordering::SeqCst);
                    async move { Ok(data(offset, len)) }
                })
                .await;
            })
        });
        let buffer = BiReadAhead::new(windows);
        buffer.read(0, 32).await.unwrap();
        buffer.read(32 * CHUNK as u64, 32).await.unwrap();
        // A formerly distant cursor catches up, then stays three chunks behind.
        // Run far past both original windows and any queued speculative data.
        let mut settled = [0; 2];
        for i in 29..100 {
            if i == 48 {
                settled = std::array::from_fn(|j| counts[j].load(Ordering::SeqCst));
            }
            for offset in [i * CHUNK as u64, (i + 3) * CHUNK as u64] {
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(2), buffer.read(offset, 4096))
                        .await
                        .expect("converging cursor stalled")
                        .unwrap(),
                    data(offset, 4096)
                );
            }
        }
        let further_reads: Vec<_> = counts
            .iter()
            .enumerate()
            .map(|(i, n)| n.load(Ordering::SeqCst) - settled[i])
            .collect();
        // After convergence, only bounded in-flight data can remain on the
        // retiring producer. It must not shadow the final fifty-two chunks.
        assert!(
            *further_reads.iter().min().unwrap() <= 6,
            "both producers kept advancing: {further_reads:?}"
        );
        assert!(
            further_reads.iter().sum::<usize>() <= 70,
            "sustained duplicate fetches: {further_reads:?}"
        );
        assert!(
            buffer
                .windows
                .iter()
                .all(|w| w.shared.state.lock().unwrap().peak <= w.capacity)
        );
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
