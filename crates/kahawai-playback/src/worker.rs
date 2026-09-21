//! The `remux-worker` command line, as the child parses it.
//!
//! The per-session pipeline worker (§1.1 crash isolation) is spawned by
//! the hub and the transcoder as `<current_exe> remux-worker ...`, so every
//! binary that supervises sessions carries this arm. The supervisor side of
//! the same spelling is [`crate::job::Job::to_argv`]; the two are held
//! together by the round-trip tests in `tests/job_codecs.rs`.

use std::path::PathBuf;

#[derive(clap::Args, Debug)]
pub struct WorkerArgs {
    pub socket: PathBuf,
    pub out_dir: PathBuf,
    pub size: u64,
    #[arg(long, default_value = "off")]
    pub video: String,
    #[arg(long, default_value = "off")]
    pub audio: String,
    #[arg(long, default_value_t = 0)]
    pub audio_track: usize,
    #[arg(long, default_value_t = 0)]
    pub video_track: usize,
    #[arg(long, default_value_t = 0)]
    pub start_ms: u64,
    #[arg(long)]
    pub sink: Option<String>,
    /// Additional parts of a split source, in timeline order, as
    /// `<socket>:<size>`. The positional socket/size is part one.
    #[arg(long = "part")]
    pub parts: Vec<String>,
    /// HUB-15 encode parameters; absent = historical fixed values.
    #[arg(long)]
    pub video_kbps: Option<u32>,
    #[arg(long)]
    pub max_height: Option<u32>,
    #[arg(long)]
    pub max_channels: Option<u32>,
    // Supervisors pass the value as a separate argv token; attenuation
    // therefore starts with `-` and must remain a value, not a new flag.
    #[arg(long, allow_negative_numbers = true)]
    pub stereo_gain_db: Option<f64>,
    #[arg(long, allow_negative_numbers = true)]
    pub native_gain_db: Option<f64>,
    #[arg(long)]
    pub loudness_source_channels: Option<u32>,
    /// JSON `AudioLayoutGain[]`; part of the protocol-4 baseline.
    #[arg(long)]
    pub loudness_gains: Option<String>,
    /// HUB-15a: tone-map HDR to SDR in the video encode chain.
    #[arg(long)]
    pub tone_map: bool,
    /// The source is interlaced: deinterlace before the encoder.
    #[arg(long)]
    pub deinterlace: bool,
    /// The pid this worker belongs to. See the PDEATHSIG guard in the
    /// runtime's `run_remux_worker`; absent means the guard cannot check
    /// and leaves the kernel's signal as the only tie.
    #[arg(long)]
    pub supervisor_pid: Option<u32>,
    /// HUB-32b: burn this image subtitle track (e{n}) into the picture.
    #[arg(long)]
    pub burn_sub: Option<usize>,
    /// Display sets to burn, extracted by the mediahost. Present for
    /// every dispatched session — a worker cannot walk the source
    /// index itself (every read crosses the byte plane).
    #[arg(long)]
    pub burn_sets: Option<PathBuf>,
    /// HUB-32a: burn this EMBEDDED text subtitle track (e{n}) into the
    /// picture with libass. A user's SIDECAR .ass burns from
    /// `--burn-ass-file` instead — the demuxer pad is used for embedded
    /// tracks because it is what carries the file's attached fonts.
    #[arg(long)]
    pub burn_ass: Option<usize>,
    #[arg(long)]
    pub burn_ass_file: Option<PathBuf>,
    /// HUB-15b encode targets + segment container; unknown values fall
    /// back to the legacy h264/aac/ts.
    #[arg(long, default_value = "h264")]
    pub video_codec: String,
    #[arg(long, default_value = "aac")]
    pub audio_codec: String,
    #[arg(long, default_value = "ts")]
    pub container: String,
}
