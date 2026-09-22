//! One file lease, either finite range reads or a lazily activated 16 MiB
//! pipeline read-ahead buffer. All clones share that buffer and one wire.
use anyhow::{Context, Result, bail};
use kahawai_proto::v1::{ByteChunk, ReadRequest};
use kahawai_transport::read_ahead::{HUB_CAPACITY, ReadAhead};
use kahawai_transport::source_stream::{self, FileReader};
use rand_core::{OsRng, RngCore};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
pub type LocalAdmission = source_stream::Admission;
const BLOCK: u64 = 4 * 1024 * 1024;
pub type LeaseWires = (
    ReceiverStream<Result<ReadRequest, tonic::Status>>,
    mpsc::Sender<ByteChunk>,
);
pub fn new_lease_token() -> String {
    let mut buf = [0u8; 16];
    OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}
enum LeaseMode {
    Direct(Option<mpsc::Receiver<ByteChunk>>),
    Buffered(ReadAhead),
}
struct LeaseInner {
    req_tx: mpsc::Sender<Result<ReadRequest, tonic::Status>>,
    mode: tokio::sync::Mutex<LeaseMode>,
    streaming: bool,
}
#[derive(Clone)]
pub struct Lease(Arc<LeaseInner>);
impl Lease {
    pub fn diagnostics(&self) -> String {
        let transport = if self.0.streaming {
            "continuous"
        } else {
            "legacy-ranges"
        };
        let buffer = match self.0.mode.try_lock().as_deref() {
            Ok(LeaseMode::Buffered(buffer)) => buffer.diagnostics(),
            _ => "finite range read or not yet activated".into(),
        };
        format!("transport={transport} {buffer}")
    }

    pub async fn read_buffered(
        &self,
        offset: u64,
        len: u64,
        size: u64,
    ) -> std::io::Result<Vec<u8>> {
        let buffer = {
            let mut mode = self.0.mode.lock().await;
            match &mut *mode {
                LeaseMode::Buffered(buffer) => buffer.clone(),
                LeaseMode::Direct(receiver) => {
                    let chunks = receiver.take().expect("lease receiver already transferred");
                    let wire = self.0.req_tx.clone();
                    let streaming = self.0.streaming;
                    tracing::info!(
                        streaming,
                        capacity = HUB_CAPACITY,
                        "hub source buffer opened"
                    );
                    let buffer =
                        ReadAhead::new(size, HUB_CAPACITY, move |requests, output| async move {
                            if streaming {
                                let send = async {
                                    let mut requests = requests;
                                    while let Some(req) = requests.recv().await {
                                        if wire.send(Ok(req)).await.is_err() {
                                            break;
                                        }
                                    }
                                };
                                let receive = async {
                                    let mut chunks = chunks;
                                    while let Some(chunk) = chunks.recv().await {
                                        if output.send(chunk).await.is_err() {
                                            break;
                                        }
                                    }
                                };
                                tokio::select! { _ = send => {}, _ = receive => {} }
                            } else {
                                legacy_stream(wire, chunks, requests, output).await;
                            }
                        });
                    *mode = LeaseMode::Buffered(buffer.clone());
                    buffer
                }
            }
        };
        buffer.read(offset, len).await
    }
    pub fn read_range(
        &self,
        offset: u64,
        len: u64,
    ) -> ReceiverStream<Result<bytes::Bytes, std::io::Error>> {
        let (tx, rx) = mpsc::channel(8);
        let lease = self.clone();
        tokio::spawn(async move {
            let mut mode = lease.0.mode.lock().await;
            if let LeaseMode::Buffered(buffer) = &*mode {
                let buffer = buffer.clone();
                drop(mode);
                let mut cur = offset;
                let Some(end) = offset.checked_add(len) else {
                    let _ = tx.send(Err(std::io::Error::other("range overflow"))).await;
                    return;
                };
                while cur < end {
                    match buffer
                        .read(cur, (end - cur).min(source_stream::CHUNK as u64))
                        .await
                    {
                        Ok(data) if data.is_empty() => return,
                        Ok(data) => {
                            cur += data.len() as u64;
                            if tx.send(Ok(data.into())).await.is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    }
                }
                return;
            }
            let LeaseMode::Direct(Some(chunks)) = &mut *mode else {
                unreachable!()
            };
            let Some(end) = offset.checked_add(len) else {
                let _ = tx.send(Err(std::io::Error::other("range overflow"))).await;
                return;
            };
            let mut cur = offset;
            while cur < end {
                let want = (end - cur).min(BLOCK);
                if lease
                    .0
                    .req_tx
                    .send(Ok(ReadRequest {
                        offset: cur,
                        len: want,
                        generation: 0,
                    }))
                    .await
                    .is_err()
                {
                    let _ = tx
                        .send(Err(std::io::Error::other("byte channel closed")))
                        .await;
                    return;
                }
                let mut served = 0;
                let mut abandoned = false;
                loop {
                    match chunks.recv().await {
                        Some(c) if !c.error.is_empty() => {
                            let _ = tx.send(Err(std::io::Error::other(c.error))).await;
                            return;
                        }
                        Some(c) if c.eof => {
                            if abandoned || served < want {
                                return;
                            }
                            cur += served;
                            break;
                        }
                        Some(c) => {
                            if c.offset != cur + served || served + c.data.len() as u64 > want {
                                let _ = tx
                                    .send(Err(std::io::Error::other("invalid range chunk")))
                                    .await;
                                return;
                            }
                            served += c.data.len() as u64;
                            if !abandoned && tx.send(Ok(c.data.into())).await.is_err() {
                                abandoned = true;
                            }
                        }
                        None => {
                            let _ = tx
                                .send(Err(std::io::Error::other("mediahost closed byte channel")))
                                .await;
                            return;
                        }
                    }
                }
            }
        });
        ReceiverStream::new(rx)
    }
    pub fn local(path: std::path::PathBuf) -> Lease {
        Self::local_guarded(path, None, None)
    }
    pub fn local_guarded(
        path: std::path::PathBuf,
        admission: Option<LocalAdmission>,
        activity: Option<Box<dyn Send + Sync>>,
    ) -> Lease {
        let (req_tx, mut req_rx) = mpsc::channel::<Result<ReadRequest, tonic::Status>>(4);
        let (chunk_tx, chunk_rx) = mpsc::channel(8);
        tokio::spawn(async move {
            let _activity = activity;
            let file = match FileReader::open(path, admission).await {
                Ok(file) => file,
                Err(e) => {
                    if let Some(Ok(req)) = req_rx.recv().await {
                        let _ = chunk_tx
                            .send(ByteChunk {
                                generation: req.generation,
                                error: format!("{e:#}"),
                                ..Default::default()
                            })
                            .await;
                    }
                    return;
                }
            };
            let (tx, rx) = mpsc::channel(2);
            let commands = async {
                while let Some(Ok(req)) = req_rx.recv().await {
                    if tx.send(req).await.is_err() {
                        return;
                    }
                }
            };
            tokio::select! {
                _ = commands => {},
                _ = source_stream::serve(rx, chunk_tx, file.size, |offset, len| file.read(offset, len)) => {},
            }
        });
        Lease(Arc::new(LeaseInner {
            req_tx,
            mode: tokio::sync::Mutex::new(LeaseMode::Direct(Some(chunk_rx))),
            streaming: true,
        }))
    }
}

/// Older mediahosts cannot interrupt a send. Drain at most their current
/// 4 MiB request, remembering the latest seek, then resume at its generation.
async fn legacy_stream(
    wire: mpsc::Sender<Result<ReadRequest, tonic::Status>>,
    mut chunks: mpsc::Receiver<ByteChunk>,
    mut commands: mpsc::Receiver<ReadRequest>,
    output: mpsc::Sender<ByteChunk>,
) {
    let mut pending = commands.recv().await;
    while let Some(req) = pending.take() {
        let mut cur = req.offset;
        let end = req.offset.saturating_add(req.len);
        while cur < end {
            let want = (end - cur).min(BLOCK);
            if wire
                .send(Ok(ReadRequest {
                    offset: cur,
                    len: want,
                    generation: 0,
                }))
                .await
                .is_err()
            {
                return;
            }
            let mut received = 0;
            loop {
                let mut chunk = tokio::select! {
                    biased;
                    command = commands.recv() => { match command { Some(c) => pending = Some(c), None => return }; continue; }
                    chunk = chunks.recv() => match chunk { Some(c) => c, None => return },
                };
                let eof = chunk.eof;
                let error = !chunk.error.is_empty();
                if !eof {
                    received += chunk.data.len() as u64;
                }
                chunk.generation = req.generation;
                // Intermediate legacy EOFs delimit blocks, not the stream.
                if pending.is_none() && (!eof || cur + received >= end || received < want) {
                    tokio::select! {
                        biased;
                        command = commands.recv() => match command { Some(c) => pending = Some(c), None => return },
                        result = output.send(chunk) => if result.is_err() { return; },
                    }
                }
                if error {
                    return;
                }
                if eof {
                    break;
                }
            }
            if pending.is_some() || received < want {
                break;
            }
            cur += received;
        }
        if pending.is_none() {
            pending = commands.recv().await;
        }
    }
}

/// Pending leases waiting for their ByteChannel to arrive.
#[derive(Default)]
pub struct Leases {
    pending: Mutex<HashMap<String, oneshot::Sender<Lease>>>,
}

impl Leases {
    /// Register a token, then wait (bounded) for the host's channel.
    pub async fn establish(
        &self,
        token: &str,
        announce: impl std::future::Future<Output = Result<()>>,
    ) -> Result<Lease> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(token.to_string(), tx);
        let cleanup = || self.pending.lock().unwrap().remove(token);
        if let Err(e) = announce.await {
            cleanup();
            return Err(e).context("announcing OpenRead");
        }
        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(lease)) => Ok(lease),
            Ok(Err(_)) | Err(_) => {
                cleanup();
                bail!("mediahost did not open the byte channel in time");
            }
        }
    }

    /// Called by the ByteChannel service when a host connects with a token.
    /// Returns the wires the service should pump, or None for unknown tokens.
    pub fn fulfill(&self, token: &str, streaming: bool) -> Option<LeaseWires> {
        let waiter = self.pending.lock().unwrap().remove(token)?;
        let (req_tx, req_rx) = mpsc::channel(4);
        let (chunk_tx, chunk_rx) = mpsc::channel::<ByteChunk>(8);
        let lease = Lease(Arc::new(LeaseInner {
            req_tx,
            mode: tokio::sync::Mutex::new(LeaseMode::Direct(Some(chunk_rx))),
            streaming,
        }));
        waiter.send(lease).ok()?;
        Some((ReceiverStream::new(req_rx), chunk_tx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountedOperation(Arc<AtomicUsize>);

    impl Drop for CountedOperation {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn a_stalled_consumer_does_not_hold_local_storage_admission() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.mkv");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(16 * 1024 * 1024).unwrap();

        let active = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(AtomicUsize::new(0));
        let admission: LocalAdmission = {
            let active = active.clone();
            let entered = entered.clone();
            Arc::new(move || {
                active.fetch_add(1, Ordering::SeqCst);
                entered.fetch_add(1, Ordering::SeqCst);
                let guard = CountedOperation(active.clone());
                Box::pin(async move { Ok(Box::new(guard) as Box<dyn Send + Sync>) })
            })
        };

        let lease = Lease::local_guarded(path, Some(admission), None);
        // Keep the stream alive without consuming it. Both bounded channels
        // fill, leaving the producer blocked on delivery rather than I/O.
        let _stream = lease.read_range(0, 16 * 1024 * 1024);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if entered.load(Ordering::SeqCst) >= 12 && active.load(Ordering::SeqCst) == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("local producer did not reach downstream backpressure");

        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(entered.load(Ordering::SeqCst) >= 12);
        drop(lease);
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use tokio::time::timeout;
    fn expected(offset: u64, n: usize) -> Vec<u8> {
        (offset..offset + n as u64)
            .map(|v| (v % 251) as u8)
            .collect()
    }

    #[tokio::test]
    async fn legacy_host_drains_only_current_block_before_serving_new_generation() {
        let size = 40 * 1024 * 1024;
        let (wire, mut requests) = mpsc::channel::<Result<ReadRequest, tonic::Status>>(2);
        let (tx, chunks) = mpsc::channel(2);
        let old_host = tokio::spawn(async move {
            while let Some(Ok(req)) = requests.recv().await {
                assert_eq!(req.generation, 0);
                assert!(req.len <= BLOCK);
                let mut offset = req.offset;
                let end = req.offset.saturating_add(req.len).min(size);
                while offset < end {
                    let len = (end - offset).min(source_stream::CHUNK as u64) as usize;
                    if tx
                        .send(ByteChunk {
                            offset,
                            data: expected(offset, len),
                            ..Default::default()
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    offset += len as u64;
                }
                if tx
                    .send(ByteChunk {
                        offset,
                        eof: true,
                        ..Default::default()
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        let lease = Lease(Arc::new(LeaseInner {
            req_tx: wire,
            mode: tokio::sync::Mutex::new(LeaseMode::Direct(Some(chunks))),
            streaming: false,
        }));
        assert_eq!(
            lease.read_buffered(0, 1, size).await.unwrap(),
            expected(0, 1)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        for offset in [30 * 1024 * 1024 + 17, 33, size - 5] {
            let data = timeout(
                Duration::from_secs(3),
                lease.read_buffered(offset, 4096, size),
            )
            .await
            .expect("legacy seek stalled")
            .unwrap();
            assert_eq!(data, expected(offset, (size - offset).min(4096) as usize));
        }
        drop(lease);
        timeout(Duration::from_secs(1), old_host)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn local_lease_seeks_after_full_buffer_and_surfaces_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        let file = std::fs::File::create(&path).unwrap();
        let size = 40 * 1024 * 1024;
        file.set_len(size).unwrap();
        let lease = Lease::local(path);
        assert_eq!(lease.read_buffered(0, 1, size).await.unwrap(), [0]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            timeout(
                Duration::from_secs(2),
                lease.read_buffered(size - 17, 19, size)
            )
            .await
            .unwrap()
            .unwrap(),
            vec![0; 17]
        );
        file.set_len(0).unwrap();
        assert!(
            timeout(
                Duration::from_secs(2),
                lease.read_buffered(25 * 1024 * 1024, 19, size)
            )
            .await
            .unwrap()
            .is_err()
        );
    }
}
