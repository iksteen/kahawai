//! Source-local EBU R128 measurement keyed by exact output channel layout.
//!
//! One decode feeds bounded meter branches for the untouched decoded layout
//! and every smaller canonical layout playback may choose. Static gains can
//! therefore be selected after the worker's real conversion caps are known,
//! without deriving correlation-sensitive loudness from lossy scalar facts.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_audio as gst_audio;

pub const ANALYZER: i64 = 6;

/// EBU R 128 s2 (2023) permits a -20 to -16 LUFS distribution level for
/// streaming devices with limited playback gain/headroom; -18 LUFS is the
/// centre of that documented range.
/// Source: https://tech.ebu.ch/docs/r/r128s2.pdf
pub const TARGET_LUFS: f64 = -18.0;

/// EBU R 128 (2023), recommendation (l): programme true peak shall not exceed
/// -1 dBTP for 20 kHz-bandlimited linear audio.
/// Source: https://tech.ebu.ch/docs/r/r128.pdf
pub const MAX_TRUE_PEAK_DBTP: f64 = -1.0;

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AudioLoudness {
    pub integrated_lufs: f64,
    pub true_peak_dbtp: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct AudioLayout {
    pub channels: u32,
    pub channel_mask: u64,
}

impl AudioLayout {
    pub fn new(channels: u32, channel_mask: u64) -> Self {
        let channel_mask = match (channels, channel_mask) {
            (1, 0) => 0x4,
            (2, 0) => 0x3,
            _ => channel_mask,
        };
        Self {
            channels,
            channel_mask,
        }
    }

    pub fn from_stream(channels: u32, channel_mask: Option<&str>) -> Self {
        let mask = channel_mask
            .and_then(|value| value.strip_prefix("0x"))
            .and_then(|value| u64::from_str_radix(value, 16).ok())
            .unwrap_or(0);
        Self::new(channels, mask)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AudioLayoutLoudness {
    pub layout: AudioLayout,
    pub loudness: AudioLoudness,
}
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AudioLayoutGain {
    pub layout: AudioLayout,
    pub gain_db: f64,
}
pub const MAX_LAYOUT_GAINS: usize = STANDARD_LAYOUTS.len() + 1;
pub type AudioLayoutGains = [Option<AudioLayoutGain>; MAX_LAYOUT_GAINS];

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AudioLoudnessMeasurement {
    pub source: AudioLayout,
    pub layouts: Vec<AudioLayoutLoudness>,
}

impl AudioLoudnessMeasurement {
    pub fn get(&self, layout: AudioLayout) -> Option<AudioLoudness> {
        self.layouts
            .iter()
            .find(|measurement| measurement.layout == layout)
            .map(|measurement| measurement.loudness)
    }
}

pub const STANDARD_LAYOUTS: &[AudioLayout] = &[
    AudioLayout {
        channels: 8,
        channel_mask: 0xc3f,
    },
    AudioLayout {
        channels: 8,
        channel_mask: 0xff,
    },
    AudioLayout {
        channels: 6,
        channel_mask: 0x3f,
    },
    AudioLayout {
        channels: 2,
        channel_mask: 0x3,
    },
    AudioLayout {
        channels: 1,
        channel_mask: 0x4,
    },
];

pub fn measured_layouts(source: AudioLayout) -> Vec<AudioLayout> {
    let mut layouts = vec![source];
    layouts.extend(STANDARD_LAYOUTS.iter().copied().filter(|layout| {
        layout.channels < source.channels
            || (source.channel_mask == 0 && layout.channels == source.channels)
    }));
    layouts
}
/// Replace the unconstrained native probe's declared layout with what the
/// decoder actually produced, retaining every explicit matrix the analyzer
/// built from discovery. An unpositioned 7.1 declaration, for example, probes
/// both canonical 7.1 masks even when native decode resolves to one of them.
pub fn resolved_measured_layouts(declared: AudioLayout, decoded: AudioLayout) -> Vec<AudioLayout> {
    let mut layouts = if decoded.channel_mask == 0 {
        Vec::new()
    } else {
        vec![decoded]
    };
    layouts.extend(measured_layouts(declared).into_iter().skip(1));
    layouts.sort_by_key(|layout| (std::cmp::Reverse(layout.channels), layout.channel_mask));
    layouts.dedup();
    layouts
}

pub fn layout_from_caps(caps: &gst::CapsRef) -> Option<AudioLayout> {
    let structure = caps.structure(0)?;
    let channels = structure.get::<i32>("channels").ok()?.max(0) as u32;
    if channels == 0 {
        return None;
    }
    let mask = structure
        .get::<gst::Bitmask>("channel-mask")
        .map(|mask| *mask)
        .unwrap_or(0);
    Some(AudioLayout::new(channels, mask))
}

/// Static programme gain: hit the loudness target unless true-peak headroom
/// requires less. No limiter or continuously varying gain is involved.
pub fn gain_db(measured: AudioLoudness) -> f64 {
    let loudness_gain = TARGET_LUFS - measured.integrated_lufs;
    let peak_gain = MAX_TRUE_PEAK_DBTP - measured.true_peak_dbtp;
    loudness_gain.min(peak_gain)
}

pub fn gain_multiplier(gain_db: f64) -> f64 {
    10.0f64.powf(gain_db / 20.0)
}

struct MeterState {
    name: &'static str,
    meter: Option<ebur128::EbuR128>,
    channels: u32,
    layout: Option<AudioLayout>,
    error: Option<String>,
    scratch: Vec<f32>,
}

impl MeterState {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            meter: None,
            scratch: Vec::new(),
            channels: 0,
            layout: None,
            error: None,
        }
    }

    fn add_sample(&mut self, sample: &gst::Sample) -> Result<()> {
        let caps = sample.caps().context("loudness sample has no caps")?;
        let info = gst_audio::AudioInfo::from_caps(caps)?;
        let layout = layout_from_caps(caps).context("loudness sample has no channel layout")?;
        if self.meter.is_none() {
            let mode = ebur128::Mode::I | ebur128::Mode::TRUE_PEAK | ebur128::Mode::HISTOGRAM;
            let mut meter = ebur128::EbuR128::new(info.channels(), info.rate(), mode)?;
            let channel_map = info
                .positions()
                .context("loudness layout has no channel positions")?
                .iter()
                .copied()
                .map(channel_for_position)
                .collect::<Vec<_>>();
            meter.set_channel_map(&channel_map)?;
            self.channels = info.channels();
            self.layout = Some(layout);
            self.meter = Some(meter);
        }

        anyhow::ensure!(
            self.channels == info.channels(),
            "{} channel count changed from {} to {}",
            self.name,
            self.channels,
            info.channels()
        );
        anyhow::ensure!(
            self.layout == Some(layout),
            "{} channel layout changed during measurement",
            self.name
        );
        let buffer = sample.buffer().context("loudness sample has no buffer")?;
        let map = buffer
            .map_readable()
            .context("mapping loudness audio buffer")?;
        // The caps force native-endian F32, matching playback's unclipped
        // conversion matrix before its encoder-format conversion. GStreamer
        // buffers are normally aligned; retain a custom-allocator fallback.
        let (prefix, aligned, suffix) = unsafe { map.as_slice().align_to::<f32>() };
        if prefix.is_empty() && suffix.is_empty() {
            self.meter
                .as_mut()
                .expect("meter initialized above")
                .add_frames_f32(aligned)?;
        } else {
            // Reuse one conversion buffer: allocating a Vec for every audio
            // buffer fills native streaming-thread arenas over a long file.
            self.scratch.clear();
            self.scratch.extend(
                map.chunks_exact(4)
                    .map(|bytes| f32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
            );
            self.meter
                .as_mut()
                .expect("meter initialized above")
                .add_frames_f32(&self.scratch)?;
        }
        Ok(())
    }

    fn finish(&self) -> Result<AudioLayoutLoudness> {
        if let Some(error) = &self.error {
            anyhow::bail!("{} meter failed: {error}", self.name);
        }
        let meter = self
            .meter
            .as_ref()
            .with_context(|| format!("{} programme was too short or silent to meter", self.name))?;
        let integrated_lufs = meter.loudness_global()?;
        let true_peak = (0..self.channels)
            .map(|channel| meter.true_peak(channel))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .fold(0.0f64, f64::max);
        anyhow::ensure!(
            integrated_lufs.is_finite(),
            "{} integrated loudness is not finite",
            self.name
        );
        anyhow::ensure!(
            true_peak > 0.0 && true_peak.is_finite(),
            "no {} true-peak measurement",
            self.name
        );
        Ok(AudioLayoutLoudness {
            layout: self.layout.context("meter has no channel layout")?,
            loudness: AudioLoudness {
                integrated_lufs,
                true_peak_dbtp: 20.0 * true_peak.log10(),
            },
        })
    }
}

fn channel_for_position(position: gst_audio::AudioChannelPosition) -> ebur128::Channel {
    use gst_audio::AudioChannelPosition as Position;
    match position {
        // A mono programme is one centre channel. `DualMono` tells libebur128
        // to count it twice and is intentionally about 3 LU louder.
        Position::Mono => ebur128::Channel::Center,
        Position::FrontLeft => ebur128::Channel::Left,
        Position::FrontRight => ebur128::Channel::Right,
        Position::FrontCenter => ebur128::Channel::Center,
        Position::Lfe1 | Position::Lfe2 => ebur128::Channel::Unused,
        // BS.1770 applies the 1.41 surround energy weight to the standard
        // 5.1 rear pair. The angular Mp135/Mm135 extensions are unweighted in
        // libebur128 and therefore are not equivalent here.
        Position::RearLeft => ebur128::Channel::LeftSurround,
        Position::RearRight => ebur128::Channel::RightSurround,
        Position::FrontLeftOfCenter => ebur128::Channel::MpSC,
        Position::FrontRightOfCenter => ebur128::Channel::MmSC,
        Position::RearCenter => ebur128::Channel::Mp180,
        Position::SideLeft => ebur128::Channel::Mp090,
        Position::SideRight => ebur128::Channel::Mm090,
        Position::TopFrontLeft => ebur128::Channel::Up030,
        Position::TopFrontRight => ebur128::Channel::Um030,
        Position::TopFrontCenter => ebur128::Channel::Up000,
        Position::TopCenter => ebur128::Channel::Tp000,
        Position::TopRearLeft => ebur128::Channel::Up135,
        Position::TopRearRight => ebur128::Channel::Um135,
        Position::TopSideLeft => ebur128::Channel::Up090,
        Position::TopSideRight => ebur128::Channel::Um090,
        Position::TopSurroundLeft => ebur128::Channel::Up110,
        Position::TopSurroundRight => ebur128::Channel::Um110,
        Position::TopRearCenter => ebur128::Channel::Up180,
        Position::BottomFrontCenter => ebur128::Channel::Bp000,
        Position::BottomFrontLeft => ebur128::Channel::Bp045,
        Position::BottomFrontRight => ebur128::Channel::Bm045,
        Position::WideLeft => ebur128::Channel::Mp060,
        Position::WideRight => ebur128::Channel::Mm060,
        Position::SurroundLeft => ebur128::Channel::LeftSurround,
        Position::SurroundRight => ebur128::Channel::RightSurround,
        Position::Invalid | Position::None => ebur128::Channel::Unused,
        _ => ebur128::Channel::Unused,
    }
}

const NO_PROGRESS_TIMEOUT: Duration = Duration::from_secs(60);

struct MeterProgress {
    last_buffer: Mutex<Instant>,
    active_callbacks: AtomicUsize,
}

impl MeterProgress {
    fn new() -> Self {
        Self {
            last_buffer: Mutex::new(Instant::now()),
            active_callbacks: AtomicUsize::new(0),
        }
    }

    fn stalled(&self, timeout: Duration) -> bool {
        self.active_callbacks.load(Ordering::Acquire) == 0
            && self.last_buffer.lock().unwrap().elapsed() >= timeout
    }
}

fn install_meter_callback(
    sink: &gst_app::AppSink,
    state: Arc<Mutex<MeterState>>,
    recording: Arc<AtomicBool>,
    between: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    progress: Arc<MeterProgress>,
) {
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if !recording.load(Ordering::Acquire) {
                    return Ok(gst::FlowSuccess::Ok);
                }
                progress.active_callbacks.fetch_add(1, Ordering::AcqRel);
                let checkpoint = between();
                let mut state = state.lock().unwrap();
                let result = match checkpoint.and_then(|()| state.add_sample(&sample)) {
                    Ok(()) => {
                        *progress.last_buffer.lock().unwrap() = Instant::now();
                        Ok(gst::FlowSuccess::Ok)
                    }
                    Err(error) => {
                        state.error = Some(format!("{error:#}"));
                        Err(gst::FlowError::Error)
                    }
                };
                progress.active_callbacks.fetch_sub(1, Ordering::AcqRel);
                result
            })
            .build(),
    );
}

struct MeterBranch {
    queue: gst::Element,
    convert: gst::Element,
    caps: gst::Element,
    sink: gst_app::AppSink,
    state: Arc<Mutex<MeterState>>,
}

impl MeterBranch {
    fn new(target: Option<AudioLayout>, raw_format: &str) -> Result<Self> {
        let queue = gst::ElementFactory::make("queue")
            .property("max-size-buffers", 4u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            .build()?;
        let convert = gst::ElementFactory::make("audioconvert").build()?;
        let mut caps = gst::Caps::builder("audio/x-raw")
            .field("format", raw_format)
            .field("layout", "interleaved");
        if let Some(target) = target {
            caps = caps
                .field("channels", target.channels as i32)
                .field("channel-mask", gst::Bitmask::new(target.channel_mask));
        }
        let caps = gst::ElementFactory::make("capsfilter")
            .property("caps", caps.build())
            .build()?;
        let sink = gst_app::AppSink::builder()
            .sync(false)
            .max_buffers(4)
            .build();
        sink.set_property("async", false);
        Ok(Self {
            queue,
            convert,
            caps,
            sink,
            state: Arc::new(Mutex::new(MeterState::new("layout"))),
        })
    }
}

/// Decode one audio stream at disk speed and meter every bounded output layout
/// that playback may choose. `between` runs on every output buffer; blocking it
/// backpressures every branch so a mediahost can yield to playback and scans,
/// while returning an error cancels the measurement at that boundary.
pub fn measure_file(
    path: &Path,
    audio_index: usize,
    source_layout: AudioLayout,
    between: impl Fn() -> Result<()> + Send + Sync + 'static,
) -> Result<AudioLoudnessMeasurement> {
    crate::init()?;
    between()?;
    anyhow::ensure!(source_layout.channels > 0, "source audio has no channels");
    let raw_format = if cfg!(target_endian = "little") {
        "F32LE"
    } else {
        "F32BE"
    };

    let tee = gst::ElementFactory::make("tee").build()?;
    let targets = measured_layouts(source_layout);
    let mut branches = Vec::with_capacity(targets.len());
    // Native is unconstrained only when discovery supplied real positions.
    // Positionless multichannel has no honest exact gain key; start with the
    // canonical full-layout conversion instead of inventing centre channels.
    if source_layout.channel_mask != 0 {
        branches.push(MeterBranch::new(None, raw_format)?);
    }
    for target in targets.into_iter().skip(1) {
        branches.push(MeterBranch::new(Some(target), raw_format)?);
    }
    anyhow::ensure!(!branches.is_empty(), "no measurable audio layout");

    let recording = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(MeterProgress::new());
    let between: Arc<dyn Fn() -> Result<()> + Send + Sync> = Arc::new(between);
    for branch in &branches {
        install_meter_callback(
            &branch.sink,
            branch.state.clone(),
            recording.clone(),
            between.clone(),
            progress.clone(),
        );
    }

    let audio_sink = gst::Bin::new();
    audio_sink.add(&tee)?;
    for branch in &branches {
        audio_sink.add_many([
            &branch.queue,
            &branch.convert,
            &branch.caps,
            branch.sink.upcast_ref(),
        ])?;
        gst::Element::link_many([
            &branch.queue,
            &branch.convert,
            &branch.caps,
            branch.sink.upcast_ref(),
        ])?;
        tee.link(&branch.queue)?;
    }
    let ghost = gst::GhostPad::with_target(&tee.static_pad("sink").unwrap())?;
    ghost.set_active(true)?;
    audio_sink.add_pad(&ghost)?;

    let source = gst::ElementFactory::make("filesrc")
        .property("location", path)
        .build()?;
    let decode = crate::selected_decode::SelectedDecode::new(
        crate::selected_decode::StreamKind::Audio,
        audio_index,
        Some(gst::Caps::builder("audio/x-raw").build()),
    )?;
    let pipeline = gst::Pipeline::new();
    pipeline.add(&audio_sink)?;
    decode.install(&pipeline, &source, &audio_sink.static_pad("sink").unwrap())?;
    let bus = pipeline.bus().context("loudness pipeline has no bus")?;
    recording.store(true, Ordering::Release);
    pipeline.set_state(gst::State::Playing)?;

    let result = (|| -> Result<()> {
        loop {
            let Some(message) = bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[
                    gst::MessageType::Eos,
                    gst::MessageType::Error,
                    gst::MessageType::Application,
                ],
            ) else {
                // Cancellation must not depend on a decoder producing another
                // sample—the no-buffer stall is exactly when teardown needs
                // this independent poll.
                between()?;
                if progress.stalled(NO_PROGRESS_TIMEOUT) {
                    anyhow::bail!(
                        "loudness pipeline produced no decoded audio for {} s",
                        NO_PROGRESS_TIMEOUT.as_secs()
                    );
                }
                continue;
            };
            match message.view() {
                gst::MessageView::Eos(_) => break,
                gst::MessageView::Error(error) => {
                    anyhow::bail!("loudness pipeline: {} ({:?})", error.error(), error.debug());
                }
                gst::MessageView::Application(application)
                    if application.structure().is_some_and(|structure| {
                        structure.name() == "kahawai-selected-decode-missing"
                    }) =>
                {
                    anyhow::bail!("audio stream {audio_index} is not decodable");
                }
                _ => {}
            }
        }
        Ok(())
    })();
    let _ = pipeline.set_state(gst::State::Null);
    result?;

    let mut layouts = Vec::with_capacity(branches.len());
    for branch in &branches {
        layouts.push(branch.state.lock().unwrap().finish()?);
    }
    let source = if source_layout.channel_mask == 0 {
        // Identity stays positionless while only honest canonical conversion
        // keys are published. There is deliberately no fabricated native key.
        source_layout
    } else {
        let decoded = layouts[0].layout;
        anyhow::ensure!(
            decoded.channels == source_layout.channels,
            "discovered {} source channels but decoded {}",
            source_layout.channels,
            decoded.channels
        );
        anyhow::ensure!(
            decoded == source_layout,
            "discovered source layout {:?} but decoded {:?}",
            source_layout,
            decoded
        );
        decoded
    };
    layouts.sort_by_key(|measurement| {
        (
            std::cmp::Reverse(measurement.layout.channels),
            measurement.layout.channel_mask,
        )
    });
    layouts.dedup_by_key(|measurement| measurement.layout);
    Ok(AudioLoudnessMeasurement { source, layouts })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_and_standard_surround_positions_use_bs1770_weights() {
        use gst_audio::AudioChannelPosition as Position;

        assert_eq!(
            channel_for_position(Position::Mono),
            ebur128::Channel::Center
        );
        assert_eq!(
            channel_for_position(Position::RearLeft),
            ebur128::Channel::LeftSurround
        );
        assert_eq!(
            channel_for_position(Position::RearRight),
            ebur128::Channel::RightSurround
        );
    }

    #[test]
    fn no_progress_watchdog_ignores_an_active_callback() {
        let progress = MeterProgress::new();
        *progress.last_buffer.lock().unwrap() = Instant::now() - Duration::from_secs(2);
        assert!(progress.stalled(Duration::from_secs(1)));
        progress.active_callbacks.store(1, Ordering::Release);
        assert!(!progress.stalled(Duration::from_secs(1)));
        progress.active_callbacks.store(0, Ordering::Release);
        *progress.last_buffer.lock().unwrap() = Instant::now();
        assert!(!progress.stalled(Duration::from_secs(1)));
    }

    #[test]
    fn gain_hits_loudness_without_crossing_true_peak() {
        let quiet = AudioLoudness {
            integrated_lufs: -28.0,
            true_peak_dbtp: -8.0,
        };
        assert_eq!(gain_db(quiet), 7.0, "true peak limits the requested 10 LU");

        let roomy = AudioLoudness {
            integrated_lufs: -24.0,
            true_peak_dbtp: -10.0,
        };
        assert_eq!(gain_db(roomy), 6.0);
        assert!((gain_multiplier(6.0) - 1.995_262).abs() < 0.000_01);

        let very_quiet = AudioLoudness {
            integrated_lufs: -60.0,
            true_peak_dbtp: -70.0,
        };
        assert_eq!(gain_db(very_quiet), 42.0);

        let very_loud = AudioLoudness {
            integrated_lufs: 8.0,
            true_peak_dbtp: 9.0,
        };
        assert_eq!(gain_db(very_loud), -26.0);
    }

    #[test]
    fn unpositioned_multichannel_also_measures_canonical_layouts() {
        assert_eq!(
            measured_layouts(AudioLayout::new(6, 0)),
            [
                AudioLayout::new(6, 0),
                AudioLayout::new(6, 0x3f),
                AudioLayout::new(2, 0x3),
                AudioLayout::new(1, 0x4),
            ]
        );
        assert_eq!(
            resolved_measured_layouts(AudioLayout::new(3, 0), AudioLayout::new(3, 0)),
            [AudioLayout::new(2, 0x3), AudioLayout::new(1, 0x4)]
        );
        assert_eq!(
            resolved_measured_layouts(AudioLayout::new(8, 0), AudioLayout::new(8, 0xc3f),),
            [
                AudioLayout::new(8, 0xff),
                AudioLayout::new(8, 0xc3f),
                AudioLayout::new(6, 0x3f),
                AudioLayout::new(2, 0x3),
                AudioLayout::new(1, 0x4),
            ]
        );
    }
    #[test]
    fn a_tenth_scale_mono_sine_measures_as_one_channel() {
        crate::init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mono-reference.wav");
        let pipeline = gst::parse::launch(&format!(
            "audiotestsrc num-buffers=30 samplesperbuffer=4800 wave=sine freq=1000 volume=0.1 ! \
             audio/x-raw,rate=48000,channels=1,channel-mask=(bitmask)0x4 ! \
             audioconvert ! wavenc ! filesink location={}",
            path.display()
        ))
        .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline.bus().unwrap().timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(message.is_some_and(|message| message.type_() == gst::MessageType::Eos));

        let source = AudioLayout::new(1, 0x4);
        let measured = measure_file(&path, 0, source, || Ok(())).unwrap();
        let lufs = measured.get(source).unwrap().integrated_lufs;
        assert!(
            (-23.2..=-22.8).contains(&lufs),
            "0.1-amplitude 1 kHz mono reference measured {lufs:.2} LUFS"
        );
    }

    #[test]
    fn measures_each_supported_output_layout_from_one_decode() {
        crate::init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        let pipeline = gst::parse::launch(&format!(
            "audiotestsrc num-buffers=200 wave=sine volume=0.1 ! \
             audio/x-raw,rate=48000,channels=6,channel-mask=(bitmask)0x3f ! \
             audioconvert ! wavenc ! filesink location={}",
            path.display()
        ))
        .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline.bus().unwrap().timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(message.is_some_and(|message| message.type_() == gst::MessageType::Eos));

        let source = AudioLayout::new(6, 0x3f);
        let measured = measure_file(&path, 0, source, || Ok(())).unwrap();
        assert_eq!(measured.source, source);
        assert_eq!(
            measured
                .layouts
                .iter()
                .map(|measurement| measurement.layout)
                .collect::<Vec<_>>(),
            [
                AudioLayout::new(6, 0x3f),
                AudioLayout::new(2, 0x3),
                AudioLayout::new(1, 0x4),
            ]
        );
        assert!(measured.layouts.iter().all(|measurement| {
            measurement.loudness.integrated_lufs.is_finite()
                && measurement.loudness.true_peak_dbtp.is_finite()
        }));
    }

    #[test]
    fn selects_each_laced_opus_track_without_cross_stream_blocking() {
        crate::init().unwrap();
        if ["opusenc", "matroskamux"]
            .into_iter()
            .any(|name| gst::ElementFactory::find(name).is_none())
        {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("three-opus-tracks.mkv");
        let pipeline = gst::parse::launch(&format!(
            "audiotestsrc num-buffers=200 wave=sine volume=0.1 ! \
             audio/x-raw,rate=48000,channels=2 ! audioconvert ! opusenc ! queue ! mux. \
             audiotestsrc num-buffers=200 wave=sine volume=0.01 freq=880 ! \
             audio/x-raw,rate=48000,channels=1 ! audioconvert ! opusenc ! queue ! mux. \
             audiotestsrc num-buffers=200 wave=sine volume=0.0005 freq=1760 ! \
             audio/x-raw,rate=48000,channels=6,channel-mask=(bitmask)0x3f ! \
             audioconvert ! opusenc ! queue ! mux. \
             matroskamux name=mux ! filesink location={}",
            path.display()
        ))
        .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline.bus().unwrap().timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(message.is_some_and(|message| message.type_() == gst::MessageType::Eos));

        let first = measure_file(&path, 0, AudioLayout::new(2, 0x3), || Ok(())).unwrap();
        let second = measure_file(&path, 1, AudioLayout::new(1, 0x4), || Ok(())).unwrap();
        let third = measure_file(&path, 2, AudioLayout::new(6, 0x3f), || Ok(())).unwrap();
        assert_eq!(first.source.channels, 2);
        assert_eq!(second.source.channels, 1);
        assert_eq!(third.source.channels, 6);
        let native =
            |measurement: &AudioLoudnessMeasurement| measurement.get(measurement.source).unwrap();
        assert!(
            native(&first).integrated_lufs - native(&second).integrated_lufs > 15.0
                && native(&second).integrated_lufs - native(&third).integrated_lufs > 15.0,
            "tracks were not selected independently: first={first:?}, \
             second={second:?}, third={third:?}"
        );
    }
}
