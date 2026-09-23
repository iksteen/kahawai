//! Fresh file transports become either finite leases or sized pipeline sources.
//! Pipeline clones share a 16 MiB buffer and one wire; the choice is immutable.
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
/// Fresh, unshared wire. Its consumer is chosen once at construction.
pub(crate) struct LeaseTransport {
    req_tx: mpsc::Sender<Result<ReadRequest, tonic::Status>>,
    chunks: mpsc::Receiver<ByteChunk>,
}
struct RangeState {
    chunks: mpsc::Receiver<ByteChunk>,
    generation: u64,
}
struct LeaseInner {
    req_tx: mpsc::Sender<Result<ReadRequest, tonic::Status>>,
    state: tokio::sync::Mutex<RangeState>,
}
/// Finite reads, serialized on one transport and drained when abandoned.
#[derive(Clone)]
pub struct Lease(Arc<LeaseInner>);
/// Sized pipeline source. All owners share one continuously filled buffer.
#[derive(Clone)]
pub(crate) struct PipelineSource {
    size: u64,
    buffer: ReadAhead,
}
impl PipelineSource {
    pub fn diagnostics(&self) -> String {
        self.buffer.diagnostics()
    }
    pub async fn read(&self, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
        self.buffer.read(offset, len).await
    }
}
impl kahawai_playback::executor::ByteSource for PipelineSource {
    fn size(&self) -> u64 {
        self.size
    }
    fn diagnostics(&self) -> String {
        self.diagnostics()
    }
    fn read(
        &self,
        offset: u64,
        len: u64,
    ) -> kahawai_playback::executor::BoxFuture<'_, std::io::Result<Vec<u8>>> {
        Box::pin(self.read(offset, len))
    }
}
impl LeaseTransport {
    pub fn finite(self) -> Lease {
        Lease(Arc::new(LeaseInner {
            req_tx: self.req_tx,
            state: tokio::sync::Mutex::new(RangeState {
                chunks: self.chunks,
                generation: 0,
            }),
        }))
    }
    pub fn buffered(self, size: u64) -> PipelineSource {
        let buffer = ReadAhead::new(size, HUB_CAPACITY, move |mut requests, output| async move {
            let send = async {
                while let Some(req) = requests.recv().await {
                    if self.req_tx.send(Ok(req)).await.is_err() {
                        break;
                    }
                }
            };
            let receive = async {
                let mut chunks = self.chunks;
                while let Some(chunk) = chunks.recv().await {
                    if output.send(chunk).await.is_err() {
                        break;
                    }
                }
            };
            tokio::select! { _ = send => {}, _ = receive => {} }
        });
        PipelineSource { size, buffer }
    }
}
impl Lease {
    pub fn read_range(
        &self,
        offset: u64,
        len: u64,
    ) -> ReceiverStream<Result<bytes::Bytes, std::io::Error>> {
        let (tx, rx) = mpsc::channel(8);
        let lease = self.clone();
        tokio::spawn(async move {
            let mut state = lease.0.state.lock().await;
            let RangeState { chunks, generation } = &mut *state;
            let Some(end) = offset.checked_add(len) else {
                let _ = tx.send(Err(std::io::Error::other("range overflow"))).await;
                return;
            };
            let mut cur = offset;
            while cur < end {
                let want = (end - cur).min(BLOCK);
                *generation = generation
                    .checked_add(1)
                    .expect("source generation exhausted");
                if lease
                    .0
                    .req_tx
                    .send(Ok(ReadRequest {
                        offset: cur,
                        len: want,
                        generation: *generation,
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
                        Some(c) if c.generation != *generation => {
                            let _ = tx
                                .send(Err(std::io::Error::other("invalid range generation")))
                                .await;
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
        LeaseTransport::local_guarded(path, None, None).finite()
    }
    #[cfg(test)]
    fn local_guarded(
        path: std::path::PathBuf,
        admission: Option<LocalAdmission>,
        activity: Option<Box<dyn Send + Sync>>,
    ) -> Lease {
        LeaseTransport::local_guarded(path, admission, activity).finite()
    }
}
impl LeaseTransport {
    pub fn local_guarded(
        path: std::path::PathBuf,
        admission: Option<LocalAdmission>,
        activity: Option<Box<dyn Send + Sync>>,
    ) -> Self {
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
        Self {
            req_tx,
            chunks: chunk_rx,
        }
    }
}

/// Pending leases waiting for their ByteChannel to arrive.
#[derive(Default)]
pub struct Leases {
    pending: Mutex<HashMap<String, oneshot::Sender<LeaseTransport>>>,
}

impl Leases {
    /// Register a token, then wait (bounded) for the host's channel.
    pub(crate) async fn establish(
        &self,
        token: &str,
        announce: impl std::future::Future<Output = Result<()>>,
    ) -> Result<LeaseTransport> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(token.to_string(), tx);
        struct Pending<'a>(&'a Leases, &'a str);
        impl Drop for Pending<'_> {
            fn drop(&mut self) {
                self.0.pending.lock().unwrap().remove(self.1);
            }
        }
        let _pending = Pending(self, token);
        if let Err(e) = announce.await {
            return Err(e).context("announcing OpenRead");
        }
        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(lease)) => Ok(lease),
            Ok(Err(_)) | Err(_) => {
                bail!("mediahost did not open the byte channel in time");
            }
        }
    }

    /// Called by the ByteChannel service when a host connects with a token.
    /// Returns the wires the service should pump, or None for unknown tokens.
    pub fn fulfill(&self, token: &str) -> Option<LeaseWires> {
        let waiter = self.pending.lock().unwrap().remove(token)?;
        let (req_tx, req_rx) = mpsc::channel(4);
        let (chunk_tx, chunk_rx) = mpsc::channel::<ByteChunk>(8);
        let lease = LeaseTransport {
            req_tx,
            chunks: chunk_rx,
        };
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
    async fn finite_requests_use_increasing_generations_across_ranges() {
        use tokio_stream::StreamExt;
        let (req_tx, mut requests) = mpsc::channel(2);
        let (chunks, rx) = mpsc::channel(2);
        let lease = LeaseTransport { req_tx, chunks: rx }.finite();
        let server = tokio::spawn(async move {
            for generation in 1..=3 {
                let req = requests.recv().await.unwrap().unwrap();
                assert_eq!(req.generation, generation);
                chunks
                    .send(ByteChunk {
                        generation,
                        offset: req.offset,
                        data: vec![generation as u8],
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                chunks
                    .send(ByteChunk {
                        generation,
                        offset: req.offset + 1,
                        eof: true,
                        ..Default::default()
                    })
                    .await
                    .unwrap();
            }
        });
        for generation in 1..=3 {
            let mut stream = lease.read_range(10, 1);
            assert_eq!(
                stream.next().await.unwrap().unwrap().as_ref(),
                &[generation as u8]
            );
            assert!(stream.next().await.is_none());
        }
        server.await.unwrap();
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
    #[tokio::test]
    async fn local_lease_seeks_after_full_buffer_and_surfaces_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        let file = std::fs::File::create(&path).unwrap();
        let size = 40 * 1024 * 1024;
        file.set_len(size).unwrap();
        let lease = LeaseTransport::local_guarded(path, None, None).buffered(size);
        assert_eq!(lease.read(0, 1).await.unwrap(), [0]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            timeout(Duration::from_secs(2), lease.read(size - 17, 19))
                .await
                .unwrap()
                .unwrap(),
            vec![0; 17]
        );
        file.set_len(0).unwrap();
        assert!(
            timeout(Duration::from_secs(2), lease.read(25 * 1024 * 1024, 19))
                .await
                .unwrap()
                .is_err()
        );
    }
}
