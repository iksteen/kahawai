//! One pipeline run, fully specified by the hub (TC-3), and its spellings.
//!
//! A [`Job`] is what a supervisor hands the pipeline: the `RemuxPlan` plus
//! everything the plan does not carry — the sizes of the parts, where to
//! start, which HLS sink to force, the display sets and sidecar script a
//! burn needs, and how much runway the playlist must show before the run
//! counts as ready. It crosses three boundaries and is spelled ONCE for
//! each: as `remux-worker` argv towards the child process, as
//! `StartSession` towards a transcoder, and as a direct call for the
//! in-process runs tests use.
//!
//! The hub, the transcoder and the runtime each used to keep their own copy
//! of these spellings. They drifted the way copies do: one side never
//! learned the stdout-capture fix, the other never learned the readiness
//! runway. The round-trip tests in `tests/job_codecs.rs` are what keep the
//! two directions of each codec agreeing now.
//!
//! Wire conventions worth knowing, because they are not the obvious ones:
//!
//! * Scalar loudness gains are `optional` on the wire and their PRESENCE is
//!   authoritative — an old hub's absent field must not read as unity gain.
//!   A present-but-unmeasured gain is sent as NaN so the worker's
//!   distinction between "absent" and "exactly 0 dB" survives.
//! * Burn indexes are 1-based on the wire, 0 = burn nothing; argv and the
//!   plan are 0-based.
//! * Bitrate, height and channel ceilings are 0 = unset on the wire.
//! * `target_duration_secs` is 0 = unknown on the wire; an old hub sends
//!   nothing and the transcoder falls back to the historical runway.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kahawai_media::loudness::{AudioLayout, AudioLayoutGain, MAX_LAYOUT_GAINS};
use kahawai_media::remux::{AudioTarget, RemuxPlan, SegmentFormat, VideoTarget};
use kahawai_media::worker::{mode_arg, parse_mode};
use kahawai_proto::v1::StartSession;

use crate::worker::WorkerArgs;

/// Bytes a burn needs that the worker cannot fetch itself: the display
/// sets the mediahost walked (HUB-32b) or a sidecar `.ass` script
/// (HUB-32a). The hub has them as files it already cached; a transcoder
/// receives them as bytes on the wire. The executor writes bytes into the
/// run directory before the worker starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    Bytes(Vec<u8>),
    Path(PathBuf),
}

impl Payload {
    /// The bytes, whichever way they are held.
    pub fn read(&self) -> std::io::Result<Vec<u8>> {
        match self {
            Payload::Bytes(bytes) => Ok(bytes.clone()),
            Payload::Path(path) => std::fs::read(path),
        }
    }
}

/// One pipeline run.
#[derive(Debug, Clone)]
pub struct Job {
    pub plan: RemuxPlan,
    /// Part sizes in timeline order. `[0]` is the positional size of the
    /// worker argv and `StartSession.size`; the rest are `--part` and
    /// `tail_sizes`. Never empty.
    pub part_sizes: Vec<u64>,
    /// Offset within the FIRST part; later parts play whole.
    pub start_ms: u64,
    /// HLS sink override. `None` follows the preference order; the TC-6
    /// retry sets `Some("hlssink2")`.
    pub sink: Option<String>,
    pub burn_sets: Option<Payload>,
    pub burn_ass: Option<Payload>,
    /// What the playlist will declare as `EXT-X-TARGETDURATION`. Feeds the
    /// readiness runway (see [`crate::playlist::runway_secs`]). `None` =
    /// unknown, which is what an old hub over the wire looks like.
    pub target_duration_secs: Option<u32>,
}

/// What `remux-worker` argv resolves to on the child side.
#[derive(Debug)]
pub struct WorkerInvocation {
    pub job: Job,
    /// One socket per part, in the same order as `job.part_sizes`.
    pub sockets: Vec<PathBuf>,
    pub out_dir: PathBuf,
    pub supervisor_pid: Option<u32>,
}

/// Where the supervisor put the things argv points at.
#[derive(Debug, Clone, Copy)]
pub struct ArgvLayout<'a> {
    /// One socket per part, in `part_sizes` order.
    pub sockets: &'a [PathBuf],
    pub out_dir: &'a Path,
    /// The display-set file, once materialised.
    pub burn_sets: Option<&'a Path>,
    /// The sidecar script file, once materialised.
    pub burn_ass: Option<&'a Path>,
    pub supervisor_pid: Option<u32>,
}

impl Job {
    pub fn size(&self) -> u64 {
        self.part_sizes[0]
    }

    pub fn tail_sizes(&self) -> &[u64] {
        &self.part_sizes[1..]
    }

    /// The exact gains the plan carries, in slot order.
    pub fn exact_gains(&self) -> Vec<AudioLayoutGain> {
        self.plan.loudness_gains.iter().flatten().copied().collect()
    }

    /// The `remux-worker` argv, subcommand token first.
    pub fn to_argv(&self, layout: &ArgvLayout<'_>) -> Result<Vec<OsString>> {
        anyhow::ensure!(!self.part_sizes.is_empty(), "a job needs at least one part");
        anyhow::ensure!(
            layout.sockets.len() == self.part_sizes.len(),
            "{} sockets for {} parts",
            layout.sockets.len(),
            self.part_sizes.len()
        );
        let plan = &self.plan;
        let mut argv: Vec<OsString> = vec![
            "remux-worker".into(),
            layout.sockets[0].clone().into(),
            layout.out_dir.to_path_buf().into(),
            self.part_sizes[0].to_string().into(),
        ];
        let mut push = |flag: &str, value: Option<String>| {
            argv.push(flag.into());
            if let Some(value) = value {
                argv.push(value.into());
            }
        };
        // Who to die with. The worker compares this against its own
        // getppid(); see the guard in the runtime's `run_remux_worker`.
        if let Some(pid) = layout.supervisor_pid {
            push("--supervisor-pid", Some(pid.to_string()));
        }
        // One socket per part: the worker joins them with concat into a
        // single pipeline, so a CD1->CD2 boundary is not a restart. Part
        // one keeps the historical positional spelling.
        for (sock, size) in layout.sockets[1..].iter().zip(&self.part_sizes[1..]) {
            push("--part", Some(format!("{}:{size}", sock.display())));
        }
        for (flag, value) in [
            ("--video-kbps", plan.video_kbps),
            ("--max-height", plan.max_height),
            ("--max-channels", plan.max_channels),
        ] {
            if let Some(value) = value {
                push(flag, Some(value.to_string()));
            }
        }
        if let Some(gain) = plan.stereo_gain_db {
            push("--stereo-gain-db", Some(gain.to_string()));
        }
        if let Some(gain) = plan.native_gain_db {
            push("--native-gain-db", Some(gain.to_string()));
        }
        if let Some(channels) = plan.loudness_source_channels {
            push("--loudness-source-channels", Some(channels.to_string()));
        }
        let gains = self.exact_gains();
        if !gains.is_empty() {
            push(
                "--loudness-gains",
                Some(serde_json::to_string(&gains).context("encoding loudness gains")?),
            );
        }
        if plan.deinterlace {
            push("--deinterlace", None);
        }
        if plan.tone_map {
            push("--tone-map", None);
        }
        if let Some(n) = plan.burn_subtitle {
            push("--burn-sub", Some(n.to_string()));
        }
        if let Some(path) = layout.burn_sets {
            push("--burn-sets", Some(path.to_string_lossy().into_owned()));
        }
        if let Some(n) = plan.burn_ass {
            push("--burn-ass", Some(n.to_string()));
        }
        if let Some(path) = layout.burn_ass {
            push("--burn-ass-file", Some(path.to_string_lossy().into_owned()));
        }
        push("--video", Some(mode_arg(plan.video).into()));
        push("--audio", Some(mode_arg(plan.audio).into()));
        push("--video-codec", Some(plan.video_codec.as_str().into()));
        push("--audio-codec", Some(plan.audio_codec.as_str().into()));
        push("--container", Some(plan.segment_format.as_str().into()));
        push("--audio-track", Some(plan.audio_track.to_string()));
        push("--video-track", Some(plan.video_track.to_string()));
        push("--start-ms", Some(self.start_ms.to_string()));
        if let Some(sink) = &self.sink {
            push("--sink", Some(sink.clone()));
        }
        Ok(argv)
    }

    /// The child side of [`Self::to_argv`].
    pub fn from_args(args: WorkerArgs) -> Result<WorkerInvocation> {
        let mut sockets = vec![args.socket];
        let mut part_sizes = vec![args.size];
        for part in &args.parts {
            let (sock, size) = part
                .rsplit_once(':')
                .context("--part wants <socket>:<size>")?;
            sockets.push(PathBuf::from(sock));
            part_sizes.push(size.parse().context("--part size")?);
        }
        let parsed: Vec<AudioLayoutGain> = args
            .loudness_gains
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .context("parsing --loudness-gains")?
            .unwrap_or_default();
        let loudness_gains = layout_gains(parsed)?;
        let plan = RemuxPlan {
            video: parse_mode(&args.video),
            audio: parse_mode(&args.audio),
            audio_track: args.audio_track,
            video_track: args.video_track,
            video_kbps: args.video_kbps,
            max_height: args.max_height,
            max_channels: args.max_channels,
            stereo_gain_db: args.stereo_gain_db,
            native_gain_db: args.native_gain_db,
            loudness_source_channels: args.loudness_source_channels,
            loudness_gains,
            tone_map: args.tone_map,
            deinterlace: args.deinterlace,
            burn_subtitle: args.burn_sub,
            burn_ass: args.burn_ass,
            video_codec: VideoTarget::from_str(&args.video_codec),
            audio_codec: AudioTarget::from_str(&args.audio_codec),
            segment_format: SegmentFormat::from_str(&args.container),
        };
        Ok(WorkerInvocation {
            job: Job {
                plan,
                part_sizes,
                start_ms: args.start_ms,
                sink: args.sink,
                burn_sets: args.burn_sets.map(Payload::Path),
                burn_ass: args.burn_ass_file.map(Payload::Path),
                // The worker never needs it: readiness is the
                // supervisor's business.
                target_duration_secs: None,
            },
            sockets,
            out_dir: args.out_dir,
            supervisor_pid: args.supervisor_pid,
        })
    }

    /// The wire spelling, hub → transcoder. Reads file-held payloads.
    pub fn to_start_session(&self, session_id: &str) -> Result<StartSession> {
        anyhow::ensure!(!self.part_sizes.is_empty(), "a job needs at least one part");
        let plan = &self.plan;
        let payload = |p: &Option<Payload>| -> Result<Vec<u8>> {
            match p {
                Some(p) => p.read().context("reading a burn payload"),
                None => Ok(Vec::new()),
            }
        };
        Ok(StartSession {
            session_id: session_id.to_string(),
            size: self.size(),
            video: mode_arg(plan.video).into(),
            audio: mode_arg(plan.audio).into(),
            audio_track: plan.audio_track as u32,
            video_track: plan.video_track as u32,
            start_ms: self.start_ms,
            sink: self.sink.clone().unwrap_or_default(),
            tail_sizes: self.tail_sizes().to_vec(),
            video_kbps: plan.video_kbps.unwrap_or(0),
            max_height: plan.max_height.unwrap_or(0),
            max_channels: plan.max_channels.unwrap_or(0),
            // Presence is authoritative in the protocol-4 baseline. The
            // sentinels keep argv's distinction between absent and an
            // exact 0 dB value.
            stereo_gain_db: Some(plan.stereo_gain_db.unwrap_or(f64::NAN)),
            native_gain_db: Some(plan.native_gain_db.unwrap_or(f64::NAN)),
            loudness_source_channels: Some(plan.loudness_source_channels.unwrap_or(0)),
            loudness_gains: self
                .exact_gains()
                .into_iter()
                .map(|gain| kahawai_proto::v1::AudioLayoutGain {
                    channels: gain.layout.channels,
                    channel_mask: gain.layout.channel_mask,
                    gain_db: gain.gain_db,
                })
                .collect(),
            tone_map: plan.tone_map,
            deinterlace: plan.deinterlace,
            // 1-based on the wire: 0 means "burn nothing".
            burn_subtitle: plan.burn_subtitle.map_or(0, |n| n as u32 + 1),
            burn_sets: payload(&self.burn_sets)?,
            burn_ass: plan.burn_ass.map_or(0, |n| n as u32 + 1),
            burn_ass_file: payload(&self.burn_ass)?,
            video_codec: plan.video_codec.as_str().into(),
            audio_codec: plan.audio_codec.as_str().into(),
            container: plan.segment_format.as_str().into(),
            target_duration_secs: self.target_duration_secs.unwrap_or(0),
        })
    }

    /// The transcoder side of [`Self::to_start_session`].
    pub fn from_start_session(msg: &StartSession) -> Result<Job> {
        let positive = |v: u32| (v > 0).then_some(v);
        let exact: Vec<AudioLayoutGain> = msg
            .loudness_gains
            .iter()
            .map(|gain| AudioLayoutGain {
                layout: AudioLayout::new(gain.channels, gain.channel_mask),
                gain_db: gain.gain_db,
            })
            .collect();
        let loudness_gains = layout_gains(exact)?;
        let plan = RemuxPlan {
            video: parse_mode(&msg.video),
            audio: parse_mode(&msg.audio),
            audio_track: msg.audio_track as usize,
            video_track: msg.video_track as usize,
            video_kbps: positive(msg.video_kbps),
            max_height: positive(msg.max_height),
            max_channels: positive(msg.max_channels),
            stereo_gain_db: msg.stereo_gain_db.filter(|gain| gain.is_finite()),
            native_gain_db: msg.native_gain_db.filter(|gain| gain.is_finite()),
            loudness_source_channels: msg.loudness_source_channels.and_then(positive),
            loudness_gains,
            tone_map: msg.tone_map,
            deinterlace: msg.deinterlace,
            burn_subtitle: positive(msg.burn_subtitle).map(|n| (n - 1) as usize),
            burn_ass: positive(msg.burn_ass).map(|n| (n - 1) as usize),
            video_codec: VideoTarget::from_str(&msg.video_codec),
            audio_codec: AudioTarget::from_str(&msg.audio_codec),
            segment_format: SegmentFormat::from_str(&msg.container),
        };
        let mut part_sizes = vec![msg.size];
        part_sizes.extend_from_slice(&msg.tail_sizes);
        Ok(Job {
            plan,
            part_sizes,
            start_ms: msg.start_ms,
            sink: (!msg.sink.is_empty()).then(|| msg.sink.clone()),
            burn_sets: (!msg.burn_sets.is_empty()).then(|| Payload::Bytes(msg.burn_sets.clone())),
            burn_ass: (!msg.burn_ass_file.is_empty())
                .then(|| Payload::Bytes(msg.burn_ass_file.clone())),
            target_duration_secs: positive(msg.target_duration_secs),
        })
    }
}

/// Pack exact gains into the plan's fixed slots, refusing more than fit.
fn layout_gains(
    gains: Vec<AudioLayoutGain>,
) -> Result<[Option<AudioLayoutGain>; MAX_LAYOUT_GAINS]> {
    anyhow::ensure!(
        gains.len() <= MAX_LAYOUT_GAINS,
        "too many layout gains ({} > {MAX_LAYOUT_GAINS})",
        gains.len()
    );
    let mut slots = [None; MAX_LAYOUT_GAINS];
    for (slot, gain) in slots.iter_mut().zip(gains) {
        *slot = Some(gain);
    }
    Ok(slots)
}
