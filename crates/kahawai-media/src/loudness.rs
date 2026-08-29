//! Source-local EBU R128 measurement for both native and stereo output.
//!
//! One decode feeds two meter branches: the untouched decoded layout, and the
//! exact default `audioconvert` stereo fold Kahawai may encode. The paired facts
//! let playback normalize stereo-to-stereo/downmix work and multichannel encodes
//! that preserve the source layout without decoding every programme twice.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_audio as gst_audio;

pub const ANALYZER: i64 = 3;

/// EBU R 128 s2 (2023) permits a -20 to -16 LUFS distribution level for
/// streaming devices with limited playback gain/headroom; -18 LUFS is the
/// centre of that documented range.
/// Source: https://tech.ebu.ch/docs/r/r128s2.pdf
pub const TARGET_LUFS: f64 = -18.0;

/// EBU R 128 (2023), recommendation (l): programme true peak shall not exceed
/// -1 dBTP for 20 kHz-bandlimited linear audio.
/// Source: https://tech.ebu.ch/docs/r/r128.pdf
pub const MAX_TRUE_PEAK_DBTP: f64 = -1.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioLoudness {
    pub integrated_lufs: f64,
    pub true_peak_dbtp: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioLoudnessMeasurement {
    pub source_channels: u32,
    pub native: AudioLoudness,
    pub stereo: AudioLoudness,
}

/// Static programme gain: hit the loudness target unless true-peak headroom
/// requires less. No limiter or continuously varying gain is involved.
pub fn gain_db(measured: AudioLoudness) -> f64 {
    let loudness_gain = TARGET_LUFS - measured.integrated_lufs;
    let peak_gain = MAX_TRUE_PEAK_DBTP - measured.true_peak_dbtp;
    loudness_gain.min(peak_gain).clamp(-24.0, 24.0)
}

pub fn gain_multiplier(gain_db: f64) -> f64 {
    10.0f64.powf(gain_db / 20.0)
}

struct MeterState {
    name: &'static str,
    meter: Option<ebur128::EbuR128>,
    channels: u32,
    error: Option<String>,
    scratch: Vec<i16>,
}

impl MeterState {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            meter: None,
            scratch: Vec::new(),
            channels: 0,
            error: None,
        }
    }

    fn add_sample(&mut self, sample: &gst::Sample) -> Result<()> {
        let caps = sample.caps().context("loudness sample has no caps")?;
        let info = gst_audio::AudioInfo::from_caps(caps)?;
        if self.meter.is_none() {
            let mode = ebur128::Mode::I | ebur128::Mode::TRUE_PEAK | ebur128::Mode::HISTOGRAM;
            let mut meter = ebur128::EbuR128::new(info.channels(), info.rate(), mode)?;
            let channel_map = info.positions().map_or_else(
                || vec![ebur128::Channel::Center; info.channels() as usize],
                |positions| {
                    positions
                        .iter()
                        .copied()
                        .map(channel_for_position)
                        .collect()
                },
            );
            meter.set_channel_map(&channel_map)?;
            self.channels = info.channels();
            self.meter = Some(meter);
        }

        anyhow::ensure!(
            self.channels == info.channels(),
            "{} channel count changed from {} to {}",
            self.name,
            self.channels,
            info.channels()
        );
        let buffer = sample.buffer().context("loudness sample has no buffer")?;
        let map = buffer
            .map_readable()
            .context("mapping loudness audio buffer")?;
        // The caps force native-endian S16. GStreamer buffers are normally
        // aligned; retain a correct fallback for custom allocators.
        let (prefix, aligned, suffix) = unsafe { map.as_slice().align_to::<i16>() };
        if prefix.is_empty() && suffix.is_empty() {
            self.meter
                .as_mut()
                .expect("meter initialized above")
                .add_frames_i16(aligned)?;
        } else {
            // Reuse one conversion buffer: allocating a Vec for every audio
            // buffer fills native streaming-thread arenas over a long file.
            self.scratch.clear();
            self.scratch.extend(
                map.chunks_exact(2)
                    .map(|bytes| i16::from_ne_bytes([bytes[0], bytes[1]])),
            );
            self.meter
                .as_mut()
                .expect("meter initialized above")
                .add_frames_i16(&self.scratch)?;
        }
        Ok(())
    }

    fn finish(&self) -> Result<AudioLoudness> {
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
        Ok(AudioLoudness {
            integrated_lufs,
            true_peak_dbtp: 20.0 * true_peak.log10(),
        })
    }
}

fn channel_for_position(position: gst_audio::AudioChannelPosition) -> ebur128::Channel {
    use gst_audio::AudioChannelPosition as Position;
    match position {
        Position::Mono => ebur128::Channel::DualMono,
        Position::FrontLeft => ebur128::Channel::Left,
        Position::FrontRight => ebur128::Channel::Right,
        Position::FrontCenter => ebur128::Channel::Center,
        Position::Lfe1 | Position::Lfe2 => ebur128::Channel::Unused,
        Position::RearLeft => ebur128::Channel::Mp135,
        Position::RearRight => ebur128::Channel::Mm135,
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
        Position::TopRearCenter => ebur128::Channel::Up180,
        Position::BottomFrontCenter => ebur128::Channel::Bp000,
        Position::BottomFrontLeft => ebur128::Channel::Bp045,
        Position::BottomFrontRight => ebur128::Channel::Bm045,
        Position::WideLeft | Position::SurroundLeft => ebur128::Channel::Mp135,
        Position::WideRight | Position::SurroundRight => ebur128::Channel::Mm135,
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
    between: Arc<dyn Fn() + Send + Sync>,
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
                between();
                let mut state = state.lock().unwrap();
                let result = match state.add_sample(&sample) {
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

/// Decode one audio stream at disk speed. `between` runs on every output
/// buffer; blocking it backpressures every meter branch so a mediahost can
/// yield to playback and scans. `source_channels` comes from discovery and
/// decides whether the stereo fold is a distinct signal worth metering.
pub fn measure_file(
    path: &Path,
    audio_index: usize,
    source_channels: u32,
    between: impl Fn() + Send + Sync + 'static,
) -> Result<AudioLoudnessMeasurement> {
    measure_file_impl(path, audio_index, source_channels, false, between)
}

fn measure_file_impl(
    path: &Path,
    audio_index: usize,
    source_channels: u32,
    force_stereo_meter: bool,
    between: impl Fn() + Send + Sync + 'static,
) -> Result<AudioLoudnessMeasurement> {
    crate::init()?;
    anyhow::ensure!(source_channels > 0, "source audio has no channels");
    let raw_format = if cfg!(target_endian = "little") {
        "S16LE"
    } else {
        "S16BE"
    };

    let tee = gst::ElementFactory::make("tee").build()?;
    let queue = || {
        gst::ElementFactory::make("queue")
            .property("max-size-buffers", 4u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            .build()
    };
    let native_queue = queue()?;
    let native_convert = gst::ElementFactory::make("audioconvert").build()?;
    let native_caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("format", raw_format)
                .field("layout", "interleaved")
                .build(),
        )
        .build()?;
    let native_sink = gst_app::AppSink::builder()
        .sync(false)
        .max_buffers(4)
        .build();
    native_sink.set_property("async", false);
    let native = Arc::new(Mutex::new(MeterState::new("native")));

    // Mono's DualMono weighting is the duplicated stereo fold, and a two
    // channel source already is the target layout. In both cases integrated
    // loudness and max per-channel true peak are identical, so a second
    // audioconvert+EBU pass is pure duplicate CPU.
    let stereo_branch = if source_channels > 2 || force_stereo_meter {
        let queue = queue()?;
        let convert = gst::ElementFactory::make("audioconvert").build()?;
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("audio/x-raw")
                    .field("format", raw_format)
                    .field("layout", "interleaved")
                    .field("channels", 2i32)
                    .field("channel-mask", gst::Bitmask::new(0x3))
                    .build(),
            )
            .build()?;
        let sink = gst_app::AppSink::builder()
            .sync(false)
            .max_buffers(4)
            .build();
        sink.set_property("async", false);
        let state = Arc::new(Mutex::new(MeterState::new("stereo")));
        Some((queue, convert, caps, sink, state))
    } else {
        None
    };

    let recording = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(MeterProgress::new());
    let between: Arc<dyn Fn() + Send + Sync> = Arc::new(between);
    install_meter_callback(
        &native_sink,
        native.clone(),
        recording.clone(),
        between.clone(),
        progress.clone(),
    );
    if let Some((_, _, _, sink, state)) = &stereo_branch {
        install_meter_callback(
            sink,
            state.clone(),
            recording.clone(),
            between,
            progress.clone(),
        );
    }

    let audio_sink = gst::Bin::new();
    audio_sink.add_many([
        &tee,
        &native_queue,
        &native_convert,
        &native_caps,
        native_sink.upcast_ref(),
    ])?;
    gst::Element::link_many([
        &native_queue,
        &native_convert,
        &native_caps,
        native_sink.upcast_ref(),
    ])?;
    tee.link(&native_queue)?;
    if let Some((queue, convert, caps, sink, _)) = &stereo_branch {
        audio_sink.add_many([queue, convert, caps, sink.upcast_ref()])?;
        gst::Element::link_many([queue, convert, caps, sink.upcast_ref()])?;
        tee.link(queue)?;
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

    let native = native.lock().unwrap();
    anyhow::ensure!(
        native.channels == source_channels,
        "discovered {source_channels} source channels but decoded {}",
        native.channels
    );
    let native_loudness = native.finish()?;
    let stereo_loudness = if let Some((_, _, _, _, state)) = &stereo_branch {
        state.lock().unwrap().finish()?
    } else {
        native_loudness
    };
    Ok(AudioLoudnessMeasurement {
        source_channels,
        native: native_loudness,
        stereo: stereo_loudness,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn measures_native_and_stereo_from_one_decode() {
        crate::init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        let pipeline = gst::parse::launch(&format!(
            "audiotestsrc num-buffers=200 wave=sine volume=0.1 ! \
             audio/x-raw,rate=48000 ! audioconvert ! wavenc ! filesink location={}",
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

        let measured = measure_file(&path, 0, 1, || {}).unwrap();
        let separate = measure_file_impl(&path, 0, 1, true, || {}).unwrap();
        let close = |left: f64, right: f64| {
            assert!(
                (left - right).abs() < 1e-9,
                "deduplicated={left}, separate={right}"
            );
        };
        close(
            separate.native.integrated_lufs,
            separate.stereo.integrated_lufs,
        );
        close(
            separate.native.true_peak_dbtp,
            separate.stereo.true_peak_dbtp,
        );
        close(
            measured.stereo.integrated_lufs,
            separate.stereo.integrated_lufs,
        );
        close(
            measured.stereo.true_peak_dbtp,
            separate.stereo.true_peak_dbtp,
        );
        assert_eq!(measured.source_channels, 1);
        assert!((-21.5..=-19.5).contains(&measured.native.integrated_lufs));
        assert!((-21.5..=-19.5).contains(&measured.stereo.integrated_lufs));
        assert!((-20.5..=-19.5).contains(&measured.native.true_peak_dbtp));
        assert!((-20.5..=-19.5).contains(&measured.stereo.true_peak_dbtp));
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

        let first = measure_file(&path, 0, 2, || {}).unwrap();
        let second = measure_file(&path, 1, 1, || {}).unwrap();
        let third = measure_file(&path, 2, 6, || {}).unwrap();
        assert_eq!(first.source_channels, 2);
        assert_eq!(second.source_channels, 1);
        assert_eq!(third.source_channels, 6);
        assert!(
            first.native.integrated_lufs - second.native.integrated_lufs > 15.0
                && second.native.integrated_lufs - third.native.integrated_lufs > 15.0,
            "tracks were not selected independently: first={first:?}, \
             second={second:?}, third={third:?}"
        );
    }
}
