//! The three spellings of a job agree with each other, field for field.
//!
//! These are the tests that stop the hub's argv, the transcoder's argv
//! and the runtime's parser drifting apart again: one `Job` goes out
//! through each codec and comes back the same.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use kahawai_media::loudness::{AudioLayout, AudioLayoutGain, MAX_LAYOUT_GAINS};
use kahawai_media::remux::{AudioTarget, RemuxPlan, SegmentFormat, StreamMode, VideoTarget};
use kahawai_playback::job::{ArgvLayout, Job, Payload};
use kahawai_playback::worker::WorkerArgs;
use kahawai_proto::v1::StartSession;

/// The command line as the binaries declare it: a hidden subcommand
/// carrying `WorkerArgs`.
#[derive(clap::Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    RemuxWorker(WorkerArgs),
}

fn parse(argv: Vec<OsString>) -> WorkerArgs {
    use clap::Parser as _;
    let cli = Cli::try_parse_from(std::iter::once(OsString::from("kahawai")).chain(argv))
        .expect("the worker parses what the supervisor spells");
    let Cmd::RemuxWorker(args) = cli.cmd;
    args
}

fn full_plan() -> RemuxPlan {
    let mut gains = [None; MAX_LAYOUT_GAINS];
    gains[0] = Some(AudioLayoutGain {
        layout: AudioLayout::new(6, 0x3f),
        gain_db: -3.5,
    });
    gains[1] = Some(AudioLayoutGain {
        layout: AudioLayout::new(2, 0x3),
        gain_db: 1.25,
    });
    RemuxPlan {
        video: StreamMode::Encode,
        audio: StreamMode::Encode,
        audio_track: 2,
        video_track: 1,
        video_kbps: Some(4500),
        max_height: Some(1080),
        max_channels: Some(6),
        stereo_gain_db: Some(-2.5),
        native_gain_db: Some(-1.0),
        loudness_source_channels: Some(8),
        loudness_gains: gains,
        tone_map: true,
        deinterlace: true,
        burn_subtitle: Some(3),
        burn_ass: Some(0),
        video_codec: VideoTarget::Hevc,
        audio_codec: AudioTarget::Opus,
        segment_format: SegmentFormat::Fmp4,
    }
}

fn full_job() -> Job {
    Job {
        plan: full_plan(),
        part_sizes: vec![100, 200],
        start_ms: 1234,
        sink: Some("hlssink2".into()),
        burn_sets: Some(Payload::Bytes(vec![1, 2, 3])),
        burn_ass: Some(Payload::Bytes(b"[Script Info]".to_vec())),
        target_duration_secs: Some(6),
    }
}

fn same_plan(a: &RemuxPlan, b: &RemuxPlan) {
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
}

#[test]
fn argv_round_trips_every_field_including_negative_gains() {
    let job = full_job();
    let sockets = [PathBuf::from("/tmp/a.sock"), PathBuf::from("/tmp/b.sock")];
    let argv = job
        .to_argv(&ArgvLayout {
            sockets: &sockets,
            out_dir: Path::new("/tmp/out"),
            burn_sets: Some(Path::new("/tmp/out/burn-sets.bin")),
            burn_ass: Some(Path::new("/tmp/out/burn.ass")),
            supervisor_pid: Some(42),
        })
        .unwrap();
    let back = Job::from_args(parse(argv)).unwrap();
    same_plan(&back.job.plan, &job.plan);
    assert_eq!(back.job.part_sizes, job.part_sizes);
    assert_eq!(back.job.start_ms, job.start_ms);
    assert_eq!(back.job.sink, job.sink);
    assert_eq!(
        back.job.burn_sets,
        Some(Payload::Path("/tmp/out/burn-sets.bin".into()))
    );
    assert_eq!(
        back.job.burn_ass,
        Some(Payload::Path("/tmp/out/burn.ass".into()))
    );
    assert_eq!(
        back.job.target_duration_secs, None,
        "argv never carries the runway"
    );
    assert_eq!(back.sockets, sockets);
    assert_eq!(back.out_dir, PathBuf::from("/tmp/out"));
    assert_eq!(back.supervisor_pid, Some(42));
}

#[test]
fn argv_matches_the_hub_and_transcoder_spelling() {
    // A golden token list: a renamed or reordered flag is a visible diff
    // here before it is a worker that refuses to start.
    let job = full_job();
    let sockets = [PathBuf::from("/tmp/a.sock"), PathBuf::from("/tmp/b.sock")];
    let argv = job
        .to_argv(&ArgvLayout {
            sockets: &sockets,
            out_dir: Path::new("/tmp/out"),
            burn_sets: Some(Path::new("/tmp/out/burn-sets.bin")),
            burn_ass: Some(Path::new("/tmp/out/burn.ass")),
            supervisor_pid: Some(42),
        })
        .unwrap();
    let gains = serde_json::to_string(&job.exact_gains()).unwrap();
    let expected: Vec<String> = [
        "remux-worker",
        "/tmp/a.sock",
        "/tmp/out",
        "100",
        "--supervisor-pid",
        "42",
        "--part",
        "/tmp/b.sock:200",
        "--video-kbps",
        "4500",
        "--max-height",
        "1080",
        "--max-channels",
        "6",
        "--stereo-gain-db",
        "-2.5",
        "--native-gain-db",
        "-1",
        "--loudness-source-channels",
        "8",
        "--loudness-gains",
        gains.as_str(),
        "--deinterlace",
        "--tone-map",
        "--burn-sub",
        "3",
        "--burn-sets",
        "/tmp/out/burn-sets.bin",
        "--burn-ass",
        "0",
        "--burn-ass-file",
        "/tmp/out/burn.ass",
        "--video",
        "encode",
        "--audio",
        "encode",
        "--video-codec",
        "hevc",
        "--audio-codec",
        "opus",
        "--container",
        "fmp4",
        "--audio-track",
        "2",
        "--video-track",
        "1",
        "--start-ms",
        "1234",
        "--sink",
        "hlssink2",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let got: Vec<String> = argv
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    assert_eq!(got, expected);
}

#[test]
fn a_bare_plan_spells_only_what_it_has() {
    let job = Job {
        plan: RemuxPlan {
            video: StreamMode::Copy,
            audio: StreamMode::Copy,
            ..RemuxPlan::default()
        },
        part_sizes: vec![10],
        start_ms: 0,
        sink: None,
        burn_sets: None,
        burn_ass: None,
        target_duration_secs: None,
    };
    let sockets = [PathBuf::from("/tmp/a.sock")];
    let argv = job
        .to_argv(&ArgvLayout {
            sockets: &sockets,
            out_dir: Path::new("/tmp/out"),
            burn_sets: None,
            burn_ass: None,
            supervisor_pid: None,
        })
        .unwrap();
    let got: Vec<String> = argv
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        got,
        [
            "remux-worker",
            "/tmp/a.sock",
            "/tmp/out",
            "10",
            "--video",
            "copy",
            "--audio",
            "copy",
            "--video-codec",
            "h264",
            "--audio-codec",
            "aac",
            "--container",
            "ts",
            "--audio-track",
            "0",
            "--video-track",
            "0",
            "--start-ms",
            "0",
        ]
    );
    let back = Job::from_args(parse(argv)).unwrap();
    same_plan(&back.job.plan, &job.plan);
    assert_eq!(back.job.burn_sets, None);
    assert_eq!(back.supervisor_pid, None);
}

#[test]
fn sockets_and_parts_must_line_up() {
    let job = full_job();
    let one = [PathBuf::from("/tmp/a.sock")];
    let err = job
        .to_argv(&ArgvLayout {
            sockets: &one,
            out_dir: Path::new("/tmp/out"),
            burn_sets: None,
            burn_ass: None,
            supervisor_pid: None,
        })
        .unwrap_err();
    assert!(err.to_string().contains("1 sockets for 2 parts"), "{err}");
}

#[test]
fn wire_round_trips_a_full_plan() {
    let job = full_job();
    let msg = job.to_start_session("s1").unwrap();
    assert_eq!(msg.session_id, "s1");
    assert_eq!(msg.size, 100);
    assert_eq!(msg.tail_sizes, vec![200]);
    assert_eq!(msg.burn_subtitle, 4, "1-based on the wire");
    assert_eq!(msg.burn_ass, 1, "1-based on the wire");
    assert_eq!(msg.stereo_gain_db, Some(-2.5));
    assert_eq!(msg.loudness_gains.len(), 2);
    assert_eq!(msg.burn_sets, vec![1, 2, 3]);
    assert_eq!(msg.burn_ass_file, b"[Script Info]".to_vec());
    assert_eq!(msg.target_duration_secs, 6);
    assert_eq!(msg.sink, "hlssink2");
    assert_eq!(
        (
            msg.video_codec.as_str(),
            msg.audio_codec.as_str(),
            msg.container.as_str()
        ),
        ("hevc", "opus", "fmp4")
    );

    let back = Job::from_start_session(&msg).unwrap();
    same_plan(&back.plan, &job.plan);
    assert_eq!(back.part_sizes, job.part_sizes);
    assert_eq!(back.start_ms, job.start_ms);
    assert_eq!(back.sink, job.sink);
    assert_eq!(back.burn_sets, job.burn_sets);
    assert_eq!(back.burn_ass, job.burn_ass);
    assert_eq!(back.target_duration_secs, Some(6));
}

#[test]
fn absent_optional_gains_stay_absent_after_decode() {
    // Mirrors the proto crate's own presence test: an old hub sends no
    // gain fields at all, and that must not read as unity gain.
    let job = Job::from_start_session(&StartSession::default()).unwrap();
    assert_eq!(job.plan.stereo_gain_db, None);
    assert_eq!(job.plan.native_gain_db, None);
    assert_eq!(job.plan.loudness_source_channels, None);
    assert!(job.plan.loudness_gains.iter().all(Option::is_none));
    assert_eq!(job.plan.burn_subtitle, None);
    assert_eq!(job.plan.burn_ass, None);
    assert_eq!(job.sink, None);
    assert_eq!(job.burn_sets, None);
    assert_eq!(job.part_sizes, vec![0]);
}

#[test]
fn nan_scalar_gain_means_absent_not_zero() {
    let mut job = full_job();
    job.plan.stereo_gain_db = None;
    job.plan.native_gain_db = None;
    job.plan.loudness_source_channels = None;
    let msg = job.to_start_session("s").unwrap();
    // Present on the wire, so a protocol-4 peer sees the field, but NaN so
    // it cannot be mistaken for an exact 0 dB.
    assert!(msg.stereo_gain_db.unwrap().is_nan());
    assert!(msg.native_gain_db.unwrap().is_nan());
    assert_eq!(msg.loudness_source_channels, Some(0));
    let back = Job::from_start_session(&msg).unwrap();
    assert_eq!(back.plan.stereo_gain_db, None);
    assert_eq!(back.plan.native_gain_db, None);
    assert_eq!(back.plan.loudness_source_channels, None);
}

#[test]
fn zero_target_duration_means_unknown() {
    let mut job = full_job();
    job.target_duration_secs = None;
    let msg = job.to_start_session("s").unwrap();
    assert_eq!(msg.target_duration_secs, 0);
    assert_eq!(
        Job::from_start_session(&msg).unwrap().target_duration_secs,
        None
    );
}

#[test]
fn an_exact_zero_db_gain_survives_both_codecs() {
    let mut job = full_job();
    job.plan.stereo_gain_db = Some(0.0);
    let msg = job.to_start_session("s").unwrap();
    assert_eq!(
        Job::from_start_session(&msg).unwrap().plan.stereo_gain_db,
        Some(0.0)
    );
    let sockets = [PathBuf::from("/a"), PathBuf::from("/b")];
    let argv = job
        .to_argv(&ArgvLayout {
            sockets: &sockets,
            out_dir: Path::new("/o"),
            burn_sets: None,
            burn_ass: None,
            supervisor_pid: None,
        })
        .unwrap();
    assert_eq!(
        Job::from_args(parse(argv)).unwrap().job.plan.stereo_gain_db,
        Some(0.0)
    );
}

#[test]
fn a_payload_held_as_a_file_is_read_onto_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let sets = dir.path().join("sets.bin");
    std::fs::write(&sets, [9, 8, 7]).unwrap();
    let mut job = full_job();
    job.burn_sets = Some(Payload::Path(sets));
    let msg = job.to_start_session("s").unwrap();
    assert_eq!(msg.burn_sets, vec![9, 8, 7]);
}
