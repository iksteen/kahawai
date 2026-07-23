//! The supervised pipeline worker end-to-end: spawn the real binary's
//! hidden `remux-worker` subcommand, serve it source bytes over the Unix
//! socket protocol, and assert it produces a playable HLS tree (§1.1 —
//! this is the process boundary that keeps GStreamer crashes away from
//! the hub).

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::time::{Duration, Instant};

/// Minimal parent side of the worker read protocol, backed by a file.
fn serve_file(listener: UnixListener, path: &Path) {
    let data = std::fs::read(path).unwrap();
    let (mut conn, _) = listener.accept().unwrap();
    let mut req = [0u8; 16];
    while conn.read_exact(&mut req).is_ok() {
        let offset = u64::from_le_bytes(req[..8].try_into().unwrap()) as usize;
        let len = u64::from_le_bytes(req[8..].try_into().unwrap()) as usize;
        let end = (offset + len).min(data.len());
        let chunk = if offset < data.len() { &data[offset..end] } else { &[][..] };
        if conn.write_all(&(chunk.len() as u64).to_le_bytes()).is_err() {
            break;
        }
        if conn.write_all(chunk).is_err() {
            break;
        }
    }
}

#[test]
fn worker_remuxes_over_socket() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("in.mkv");
    kahawai_media::testutil::render_h264_aac_mkv(&src);
    let size = std::fs::metadata(&src).unwrap().len();

    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let sock = dir.path().join("worker.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let src2 = src.clone();
    let server = std::thread::spawn(move || serve_file(listener, &src2));

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_kahawai"))
        .arg("remux-worker")
        .arg(&sock)
        .arg(&out)
        .arg(size.to_string())
        .arg("--video")
        .args(["--audio", "copy"])
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "worker did not finish in time");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success(), "worker exited with {status}");
    server.join().unwrap();

    let playlist = std::fs::read_to_string(out.join("master.m3u8")).unwrap();
    assert!(playlist.contains("#EXT-X-ENDLIST"), "playlist not finalized: {playlist}");
    assert!(out.join("segment00000.ts").exists());
}
