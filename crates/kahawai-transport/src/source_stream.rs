//! Interruptible, bounded source streams. Commands are polled independently
//! of disk admission and blocked sends. File reads are positional, so cancelling
//! a future cannot leave the next request with a half-completed seek.
use anyhow::Result;
use kahawai_proto::v1::{ByteChunk, ReadRequest};
use std::{future::Future, path::PathBuf, sync::Arc};
use tokio::sync::mpsc;

pub const CHUNK: usize = 256 * 1024;
pub type Admission = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn Future<Output = Result<Box<dyn Send + Sync>>> + Send>>
        + Send
        + Sync,
>;

pub struct FileReader {
    file: Arc<std::fs::File>,
    pub size: u64,
    admission: Option<Admission>,
    io_slot: Arc<tokio::sync::Semaphore>,
}
impl FileReader {
    pub async fn open(path: PathBuf, admission: Option<Admission>) -> Result<Self> {
        let permit = match &admission {
            Some(a) => Some(a().await?),
            None => None,
        };
        let (file, size) = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let file = std::fs::File::open(path)?;
            let size = file.metadata()?.len();
            Ok::<_, std::io::Error>((file, size))
        })
        .await??;
        Ok(Self {
            file: Arc::new(file),
            size,
            admission,
            io_slot: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }
    pub async fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let io_permit = self.io_slot.clone().acquire_owned().await?;
        let len = (self.size.saturating_sub(offset)).min(len as u64) as usize;
        let permit = match &self.admission {
            Some(a) => Some(a().await?),
            None => None,
        };
        let file = self.file.clone();
        Ok(tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::FileExt;
            let _io_permit = io_permit;
            let _permit = permit;
            let mut data = vec![0; len];
            let mut filled = 0;
            while filled < len {
                let n = file.read_at(&mut data[filled..], offset + filled as u64)?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "source shrank during read",
                    ));
                }
                filled += n;
            }
            tracing::trace!(offset, len, "source positional file read");
            Ok::<_, std::io::Error>(data)
        })
        .await??)
    }
}

pub async fn serve<F, Fut>(
    mut requests: mpsc::Receiver<ReadRequest>,
    chunks: mpsc::Sender<ByteChunk>,
    size: u64,
    read: F,
) where
    F: Fn(u64, usize) -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    let mut newest = 0;
    let mut request = requests.recv().await;
    while let Some(req) = request.take() {
        if req.generation <= newest {
            let _ = chunks
                .send(ByteChunk {
                    error: "zero or non-increasing source generation".into(),
                    generation: req.generation,
                    ..Default::default()
                })
                .await;
            return;
        }
        newest = req.generation;
        let transfer = async {
            let mut offset = req.offset.min(size);
            let end = req.offset.saturating_add(req.len).min(size);
            while offset < end {
                let want = (end - offset).min(CHUNK as u64) as usize;
                let result = read(offset, want).await;
                let data = match result {
                    Ok(data) if data.len() == want => data,
                    result => {
                        let error = match result {
                            Err(e) => format!("{e:#}"),
                            _ => "source ended before its declared size".into(),
                        };
                        let _ = chunks
                            .send(ByteChunk {
                                offset,
                                generation: req.generation,
                                error,
                                ..Default::default()
                            })
                            .await;
                        return;
                    }
                };
                for part in data.chunks(CHUNK) {
                    let n = part.len();
                    if chunks
                        .send(ByteChunk {
                            offset,
                            data: part.to_vec(),
                            generation: req.generation,
                            ..Default::default()
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    offset += n as u64;
                }
            }
            let _ = chunks
                .send(ByteChunk {
                    offset,
                    eof: true,
                    generation: req.generation,
                    ..Default::default()
                })
                .await;
        };
        tokio::select! {
            biased;
            _ = chunks.closed() => return,
            next = requests.recv() => request = next,
            _ = transfer => request = requests.recv().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn zero_and_repeated_generations_are_rejected_before_io() {
        for generations in [vec![0], vec![1, 1], vec![2, 1]] {
            let (tx, rx) = mpsc::channel(2);
            let (chunks, mut received) = mpsc::channel(2);
            let server = tokio::spawn(serve(rx, chunks, 0, |_, _| async {
                panic!("an empty source must not perform IO");
                #[allow(unreachable_code)]
                Ok(Vec::new())
            }));
            for (n, generation) in generations.iter().copied().enumerate() {
                tx.send(ReadRequest {
                    generation,
                    offset: 0,
                    len: 1,
                })
                .await
                .unwrap();
                let chunk = received.recv().await.unwrap();
                if n + 1 == generations.len() {
                    assert!(chunk.error.contains("generation"));
                } else {
                    assert!(chunk.eof);
                    assert_eq!(chunk.generation, generation);
                }
            }
            server.await.unwrap();
        }
    }
}
