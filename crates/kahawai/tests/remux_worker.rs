//! The supervised pipeline worker end-to-end: spawn the real binary's
//! hidden `remux-worker` subcommand, serve it source bytes over the Unix
//! socket protocol, and assert it produces a playable HLS tree (§1.1 —
//! this is the process boundary that keeps GStreamer crashes away from
//! the hub).

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// A worker still loads global configuration before dispatching its hidden
/// subcommand. Tests must therefore make both config lookup mechanisms
/// explicit: an isolated file, and an environment with inherited Kahawai
/// overrides removed. GStreamer-related environment remains intact.
fn worker_command(dir: &Path) -> Command {
    let config = dir.join("kahawai.toml");
    let hub = dir.join("hub");
    let mediahost = dir.join("mediahost");
    let transcoder = dir.join("transcoder");
    let xdg_config = dir.join("xdg-config");
    let xdg_data = dir.join("xdg-data");
    let xdg_cache = dir.join("xdg-cache");
    let xdg_runtime = dir.join("xdg-runtime");
    for path in [
        &hub,
        &mediahost,
        &transcoder,
        &xdg_config,
        &xdg_data,
        &xdg_cache,
        &xdg_runtime,
    ] {
        std::fs::create_dir_all(path).unwrap();
    }
    let toml_string = |path: &Path| {
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    };
    std::fs::write(
        &config,
        format!(
            "[hub]\ndata_dir = \"{}\"\n\
             [mediahost]\nstate_dir = \"{}\"\n\
             [transcoder]\nstate_dir = \"{}\"\n",
            toml_string(&hub),
            toml_string(&mediahost),
            toml_string(&transcoder),
        ),
    )
    .unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_kahawai"));
    command
        .arg("--config")
        .arg(config)
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", xdg_config)
        .env("XDG_DATA_HOME", xdg_data)
        .env("XDG_CACHE_HOME", xdg_cache)
        .env("XDG_RUNTIME_DIR", xdg_runtime);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KAHAWAI_") {
            command.env_remove(key);
        }
    }
    command
}

/// Minimal parent side of the worker read protocol, backed by a file.
fn serve_file(listener: UnixListener, path: &Path) {
    let data = std::fs::read(path).unwrap();
    let (mut conn, _) = listener.accept().unwrap();
    let mut req = [0u8; 16];
    while conn.read_exact(&mut req).is_ok() {
        let offset = u64::from_le_bytes(req[..8].try_into().unwrap()) as usize;
        let len = u64::from_le_bytes(req[8..].try_into().unwrap()) as usize;
        let end = (offset + len).min(data.len());
        let chunk = if offset < data.len() {
            &data[offset..end]
        } else {
            &[][..]
        };
        if conn.write_all(&(chunk.len() as u64).to_le_bytes()).is_err() {
            break;
        }
        if conn.write_all(chunk).is_err() {
            break;
        }
    }
}

/// Pacing (§4.6): with a tiny window and a viewer parked at zero, the
/// worker must stall; once the viewer's position advances it finishes.
#[test]
fn worker_paces_against_viewer_position() {
    if !kahawai_media::testutil::require_h264_aac_fixture() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("in.mkv");
    kahawai_media::testutil::render_h264_aac_mkv(&src); // ~10 s
    let size = std::fs::metadata(&src).unwrap().len();

    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let sock = dir.path().join("worker.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let src2 = src.clone();
    let server = std::thread::spawn(move || serve_file(listener, &src2));

    let mut child = worker_command(dir.path());
    child
        .arg("remux-worker")
        .arg(&sock)
        .arg(&out)
        .arg(size.to_string())
        .args(["--video", "copy"])
        .args(["--audio", "copy"])
        .env("KAHAWAI_PACE_WINDOW_MS", "2000");
    let mut child = child.spawn().unwrap();

    // The viewer never moves: the worker must pause within the window
    // and NOT finish the 10 s file.
    std::thread::sleep(Duration::from_secs(6));
    assert!(
        child.try_wait().unwrap().is_none(),
        "worker finished a 10s file with a 2s window and a parked viewer"
    );
    let stalled = std::fs::read_to_string(out.join("master.m3u8")).unwrap_or_default();
    assert!(
        !stalled.contains("#EXT-X-ENDLIST"),
        "playlist finalized while paced:
{stalled}"
    );

    // Viewer catches up: the worker resumes and completes.
    std::fs::write(out.join("viewer.pos"), "60000").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not resume after viewer advanced"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success(), "worker exited with {status}");
    server.join().unwrap();
    let playlist = std::fs::read_to_string(out.join("master.m3u8")).unwrap();
    assert!(
        playlist.contains("#EXT-X-ENDLIST"),
        "not finalized:
{playlist}"
    );
}

#[test]
fn worker_remuxes_over_socket() {
    if !kahawai_media::testutil::require_h264_aac_fixture() {
        return;
    }
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

    let mut child = worker_command(dir.path());
    child
        .arg("remux-worker")
        .arg(&sock)
        .arg(&out)
        .arg(size.to_string())
        .args(["--video", "copy"])
        .args(["--audio", "copy"]);
    let mut child = child.spawn().unwrap();

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
    assert!(
        playlist.contains("#EXT-X-ENDLIST"),
        "playlist not finalized: {playlist}"
    );
    assert!(out.join("segment00000.ts").exists());
}

/// The supervisor guard checks the pid it was given, not the number 1.
///
/// "My parent is init" is what a dead supervisor looks like on a normal
/// box, and it is what a HEALTHY one looks like in a container: the hub
/// is the ENTRYPOINT, so it is pid 1 and every worker it spawns has
/// getppid() == 1. Read literally, the guard refused all of them and the
/// image could not play anything (reported 2026-08-06).
///
/// Both halves matter, so both are here: a worker told its real
/// supervisor starts, and one told a pid that is not its parent does not.
#[test]
fn the_supervisor_guard_compares_pids_not_init() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    // No socket is ever served: whatever happens, this worker cannot get
    // past the source. The guard fires before that, so the two runs are
    // told apart by their message, not by their exit status.
    let sock = dir.path().join("worker.sock");

    let run = |pid: u32| {
        let mut child = worker_command(dir.path());
        let child = child
            .arg("remux-worker")
            .arg(&sock)
            .arg(&out)
            .arg("1024")
            .args(["--video", "copy"])
            .args(["--supervisor-pid", &pid.to_string()])
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap();
        String::from_utf8_lossy(&child.stderr).to_string()
    };

    // This test process IS the worker's parent, whatever pid it has.
    let ours = run(std::process::id());
    assert!(
        !ours.contains("supervisor already gone"),
        "a worker spawned by its own supervisor must start: {ours}"
    );

    // A pid that is not its parent: the race the guard exists for.
    let stranger = run(u32::from(u16::MAX) + 1);
    assert!(
        stranger.contains("supervisor already gone"),
        "a worker whose supervisor is gone must not start: {stranger}"
    );
}
