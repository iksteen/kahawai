//! The 16-byte read protocol between a supervisor and its worker, served
//! from a `ByteSource` the way both the hub and the transcoder now do.

use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use kahawai_media::worker::MAX_READ;
use kahawai_playback::executor::{BoxFuture, ByteSource, serve_reads, short_socket_dir};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

struct Mem(Vec<u8>);

impl ByteSource for Mem {
    fn size(&self) -> u64 {
        self.0.len() as u64
    }

    fn read(&self, offset: u64, len: u64) -> BoxFuture<'_, std::io::Result<Vec<u8>>> {
        let end = (offset + len).min(self.0.len() as u64) as usize;
        let data = self.0[offset as usize..end].to_vec();
        Box::pin(async move { Ok(data) })
    }
}

struct Broken;

impl ByteSource for Broken {
    fn size(&self) -> u64 {
        1 << 20
    }

    fn read(&self, _offset: u64, _len: u64) -> BoxFuture<'_, std::io::Result<Vec<u8>>> {
        Box::pin(async { Err(std::io::Error::other("lease gone")) })
    }
}

/// The worker's side of one read.
async fn ask(conn: &mut UnixStream, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
    let mut req = [0u8; 16];
    req[..8].copy_from_slice(&offset.to_le_bytes());
    req[8..].copy_from_slice(&len.to_le_bytes());
    conn.write_all(&req).await?;
    let mut header = [0u8; 8];
    conn.read_exact(&mut header).await?;
    let n = u64::from_le_bytes(header) as usize;
    let mut data = vec![0u8; n];
    conn.read_exact(&mut data).await?;
    Ok(data)
}

fn serve(source: impl ByteSource) -> UnixStream {
    let (worker, supervisor) = UnixStream::pair().unwrap();
    let source: Arc<dyn ByteSource> = Arc::new(source);
    tokio::spawn(async move {
        let _ = serve_reads(supervisor, source).await;
    });
    worker
}

#[tokio::test]
async fn a_read_past_the_end_answers_zero_bytes() {
    let mut worker = serve(Mem((0..10).collect()));
    assert_eq!(ask(&mut worker, 20, 5).await.unwrap(), Vec::<u8>::new());
    assert_eq!(
        ask(&mut worker, 8, 5).await.unwrap(),
        vec![8, 9],
        "clamped to the end"
    );
    assert_eq!(ask(&mut worker, 0, 3).await.unwrap(), vec![0, 1, 2]);
}

#[tokio::test]
async fn an_oversized_request_is_clamped_to_max_read() {
    let size = MAX_READ as usize + 1024;
    let mut worker = serve(Mem(vec![7u8; size]));
    let data = ask(&mut worker, 0, MAX_READ * 2).await.unwrap();
    assert_eq!(data.len() as u64, MAX_READ);
}

#[tokio::test]
async fn a_source_error_closes_the_socket_instead_of_hanging() {
    let mut worker = serve(Broken);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), ask(&mut worker, 0, 16))
        .await
        .expect("the worker must not hang on a dead source");
    assert!(
        outcome.is_err(),
        "a closed socket is an error the pipeline can act on"
    );
}

#[test]
fn worker_socket_directory_leaves_sun_len_headroom() {
    let dir = short_socket_dir().unwrap();
    let socket = dir.path().join("worker999.sock");
    assert!(socket.as_os_str().as_bytes().len() <= 64, "{socket:?}");
}
