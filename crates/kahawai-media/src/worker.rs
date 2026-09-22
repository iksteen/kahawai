//! Supervised pipeline worker (§1.1, §6): the remux/transcode pipeline
//! runs in a child process so a GStreamer crash (hostile input, plugin
//! bug — both observed on a real library) kills one session, not the
//! hub. The parent serves source bytes over a Unix socket.
//!
//! Wire format, child → parent per read: 16 LE bytes (offset u64,
//! len u64); parent replies 8 LE bytes n, then n bytes. n = 0 → EOF.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result};

use crate::remux::{self, RemuxPlan, RemuxSource, StreamMode};

/// Cap on a single socket read request, both sides. Larger GStreamer
/// demands are assembled from bounded reads by the pull feeder.
pub const MAX_READ: u64 = 8 * 1024 * 1024;

struct SocketSource {
    stream: UnixStream,
    path: std::path::PathBuf,
    interrupted: std::sync::Arc<std::sync::atomic::AtomicBool>,
    size: u64,
}

impl SocketSource {
    fn prepare(&mut self) -> std::io::Result<()> {
        if self
            .interrupted
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            self.stream = UnixStream::connect(&self.path)?;
        }
        Ok(())
    }
    fn request(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut req = [0u8; 16];
        req[..8].copy_from_slice(&offset.to_le_bytes());
        req[8..].copy_from_slice(&(buf.len() as u64).to_le_bytes());
        self.stream.write_all(&req)?;
        let mut hdr = [0u8; 8];
        self.stream.read_exact(&mut hdr)?;
        let n = u64::from_le_bytes(hdr) as usize;
        if n > buf.len() {
            return Err(std::io::Error::other("oversized read response"));
        }
        self.stream.read_exact(&mut buf[..n])?;
        Ok(n)
    }
}
impl RemuxSource for SocketSource {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        self.prepare()?;
        self.request(offset, buf)
    }
    fn read_at_cancelled(
        &mut self,
        offset: u64,
        buf: &mut [u8],
        cancel: &remux::ReadCancellation,
    ) -> std::io::Result<usize> {
        cancel.check()?;
        self.prepare()?;
        let interrupt = self.stream.try_clone()?;
        let interrupted = self.interrupted.clone();
        cancel.on_cancel(move || {
            interrupted.store(true, std::sync::atomic::Ordering::Release);
            let _ = interrupt.shutdown(std::net::Shutdown::Both);
        });
        cancel.check()?;
        // Do not reconnect between registration and I/O: cancellation owns
        // exactly this socket, even if it races the first write.
        let result = self.request(offset, buf);
        cancel.check()?;
        result
    }
}

pub fn parse_mode(s: &str) -> StreamMode {
    match s {
        "copy" => StreamMode::Copy,
        "encode" => StreamMode::Encode,
        _ => StreamMode::Off,
    }
}

pub fn mode_arg(mode: StreamMode) -> &'static str {
    match mode {
        StreamMode::Copy => "copy",
        StreamMode::Encode => "encode",
        StreamMode::Off => "off",
    }
}

/// Child entry point: connect to the parent's socket, run the pipeline
/// to EOS, exit. Errors (including pipeline errors) return Err — the
/// binary maps that to a non-zero exit the supervisor can see.
#[allow(clippy::too_many_arguments)] // CLI-shaped plumbing
pub fn run(
    socket: &Path,
    out_dir: &Path,
    size: u64,
    plan: RemuxPlan,
    start_ms: u64,
    sink: Option<&str>,
) -> Result<()> {
    run_parts(
        &[(socket.to_path_buf(), size)],
        out_dir,
        plan,
        start_ms,
        sink,
        None,
        None,
    )
}

/// Multi-part entry point: one socket per part, in timeline order, joined
/// into a single pipeline (see `remux::start_parts`). `start_ms` applies
/// to the first part; the rest play whole.
#[allow(clippy::too_many_arguments)] // CLI-shaped plumbing
pub fn run_parts(
    parts: &[(std::path::PathBuf, u64)],
    out_dir: &Path,
    plan: RemuxPlan,
    start_ms: u64,
    sink: Option<&str>,
    burn_sets: Option<&Path>,
    burn_ass_file: Option<&Path>,
) -> Result<()> {
    anyhow::ensure!(!parts.is_empty(), "no parts given");
    let mut sources: Vec<Box<dyn RemuxSource>> = Vec::with_capacity(parts.len());
    for (socket, size) in parts {
        let stream = UnixStream::connect(socket)
            .with_context(|| format!("connecting to {}", socket.display()))?;
        sources.push(Box::new(SocketSource {
            stream,
            path: socket.clone(),
            interrupted: Default::default(),
            size: *size,
        }));
    }
    // Pacing window (§4.6): transcode ahead of the viewer, but not the
    // whole film. The supervisor keeps `viewer.pos` fresh (absolute ms,
    // from the client's progress pings); muxer-bound buffers beyond
    // viewer+window block in-band until the viewer catches up.
    // Default to 15 minutes ahead; the environment can override the window.
    let pace = remux::PaceConfig {
        window_ms: std::env::var("KAHAWAI_PACE_WINDOW_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(900_000),
        floor_ms: start_ms,
        viewer_file: out_dir.join("viewer.pos"),
        out_dir: out_dir.to_path_buf(),
        // Unset: take the allowance from the playlist's own target
        // duration, which is the number the client's stuck check is
        // derived from. Set it (0 = off) only to pin the behaviour.
        stale_ms: std::env::var("KAHAWAI_PACE_STALE_MS")
            .ok()
            .and_then(|v| v.parse().ok()),
    };
    let job = remux::start_parts(
        out_dir,
        plan,
        sources,
        start_ms,
        sink,
        Some(pace),
        burn_sets,
        burn_ass_file,
    )?;
    while !job.finished() {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if let Some(e) = job.failed() {
        anyhow::bail!("{e}");
    }
    Ok(())
}

#[cfg(test)]
mod source_cancellation_tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn cancelled_socket_read_reconnects_without_accepting_old_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let source = SocketSource {
            stream: UnixStream::connect(&path).unwrap(),
            path,
            size: 100,
            interrupted: Default::default(),
        };
        let first = Arc::new(remux::ReadCancellation::default());
        let second = Arc::new(remux::ReadCancellation::default());
        let (entered, waiting) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut old, _) = listener.accept().unwrap();
            let mut req = [0; 16];
            old.read_exact(&mut req).unwrap();
            entered.send(()).unwrap();
            // The cancelled connection cannot contribute a response to its replacement.
            let mut byte = [0];
            assert_eq!(old.read(&mut byte).unwrap(), 0);
            for offset in [21u64, 42] {
                let (mut conn, _) = listener.accept().unwrap();
                conn.read_exact(&mut req).unwrap();
                assert_eq!(u64::from_le_bytes(req[..8].try_into().unwrap()), offset);
                conn.write_all(&8u64.to_le_bytes()).unwrap();
                conn.write_all(&[offset as u8; 8]).unwrap();
            }
        });
        let (cancel, done) = (first.clone(), second.clone());
        let client = std::thread::spawn(move || {
            let mut source = source;
            let mut buf = [0; 8];
            assert_eq!(
                source
                    .read_at_cancelled(0, &mut buf, &cancel)
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::Interrupted
            );
            assert_eq!(source.read_at_cancelled(21, &mut buf, &done).unwrap(), 8);
            assert_eq!(buf, [21; 8]);
            // A cancellation racing publication, after I/O has returned, must
            // still reconnect on the NEXT demand.
            done.cancel();
            assert_eq!(
                source
                    .read_at_cancelled(42, &mut buf, &remux::ReadCancellation::default())
                    .unwrap(),
                8
            );
            assert_eq!(buf, [42; 8]);
        });
        waiting
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        first.cancel();
        client.join().unwrap();
        server.join().unwrap();
    }
}
