//! Read leases (AR-10): the hub-side handle to one open file on a mediahost.
//!
//! A lease is created by the session manager, announced to the host via
//! OpenRead, and fulfilled when the host's ByteChannel arrives with the
//! matching token. Dropping the lease closes the channel, which is how the
//! host learns the lease ended — no explicit close message.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use kahawai_proto::v1::{ByteChunk, ReadRequest};
use rand_core::{OsRng, RngCore};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

/// Read granularity: big enough to amortize round trips, small enough that
/// an abandoned HTTP request wastes at most one block.
const BLOCK: u64 = 4 * 1024 * 1024;

/// What the ByteChannel service pumps: the request stream to return to the
/// host, and the sender for its inbound chunks.
pub type LeaseWires = (
    ReceiverStream<Result<ReadRequest, tonic::Status>>,
    mpsc::Sender<ByteChunk>,
);

pub fn new_lease_token() -> String {
    let mut buf = [0u8; 16];
    OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

struct LeaseInner {
    req_tx: mpsc::Sender<Result<ReadRequest, tonic::Status>>,
    chunk_rx: tokio::sync::Mutex<mpsc::Receiver<ByteChunk>>,
}

/// Cloneable handle; all clones share one sequential byte channel.
#[derive(Clone)]
pub struct Lease(Arc<LeaseInner>);

impl Lease {
    /// Stream `len` bytes starting at `offset`. Reads are serialized per
    /// lease; blocks are requested one at a time so an abandoned consumer
    /// stops the transfer at the next block boundary.
    pub fn read_range(&self, offset: u64, len: u64) -> ReceiverStream<Result<bytes::Bytes, std::io::Error>> {
        let (out_tx, out_rx) = mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
        let inner = self.0.clone();
        tokio::spawn(async move {
            let mut chunk_rx = inner.chunk_rx.lock().await;
            let mut cur = offset;
            let end = offset + len;
            let mut consumer_gone = false;
            'blocks: while cur < end && !consumer_gone {
                let block = BLOCK.min(end - cur);
                if inner
                    .req_tx
                    .send(Ok(ReadRequest { offset: cur, len: block }))
                    .await
                    .is_err()
                {
                    let _ = out_tx
                        .send(Err(std::io::Error::other("byte channel closed")))
                        .await;
                    return;
                }
                let mut served: u64 = 0;
                loop {
                    match chunk_rx.recv().await {
                        Some(c) if !c.error.is_empty() => {
                            let _ = out_tx.send(Err(std::io::Error::other(c.error))).await;
                            return;
                        }
                        Some(c) if c.eof => {
                            cur += served;
                            if served < block {
                                // File ended short of the request.
                                break 'blocks;
                            }
                            break;
                        }
                        Some(c) => {
                            served += c.data.len() as u64;
                            if !consumer_gone
                                && out_tx.send(Ok(bytes::Bytes::from(c.data))).await.is_err()
                            {
                                // Consumer went away (seek/abort): finish
                                // draining this block, then stop requesting.
                                consumer_gone = true;
                            }
                        }
                        None => {
                            let _ = out_tx
                                .send(Err(std::io::Error::other("mediahost closed byte channel")))
                                .await;
                            return;
                        }
                    }
                }
            }
        });
        ReceiverStream::new(out_rx)
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
    pub fn fulfill(&self, token: &str) -> Option<LeaseWires> {
        let waiter = self.pending.lock().unwrap().remove(token)?;
        let (req_tx, req_rx) = mpsc::channel(4);
        let (chunk_tx, chunk_rx) = mpsc::channel::<ByteChunk>(8);
        let lease = Lease(Arc::new(LeaseInner {
            req_tx,
            chunk_rx: tokio::sync::Mutex::new(chunk_rx),
        }));
        waiter.send(lease).ok()?;
        Some((ReceiverStream::new(req_rx), chunk_tx))
    }
}
