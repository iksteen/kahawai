//! Season-wide segment detection on source-local paths.
//!
//! The hub owns catalogue grouping, ordering and persistence. The mediahost
//! receives one exact-source season, performs the expensive decode work beside
//! the bytes, and returns only boundaries.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use kahawai_proto::v1::{
    DetectSegments, DetectedSegment, HostToHub, SegmentDetectionAccepted,
    SegmentDetectionRejection, SegmentDetectionResult, SegmentEpisode, SegmentEpisodeResult,
    host_to_hub,
};

use crate::Activity;
use crate::scan::CollectionConfig;

struct Prepared {
    request: SegmentEpisode,
    path: PathBuf,
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub async fn run(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<DetectSegments>,
    tx: tokio::sync::mpsc::Sender<HostToHub>,
    collections: Vec<CollectionConfig>,
    activity: Activity,
    segment_serial: Arc<tokio::sync::Mutex<()>>,
    hub_protocol_minor: u32,
) {
    while let Some(job) = rx.recv().await {
        if job.detector != kahawai_core::segments::DETECTOR_GENERATION {
            let error = format!(
                "unsupported detector generation {}; this mediahost implements {}",
                job.detector,
                kahawai_core::segments::DETECTOR_GENERATION
            );
            if tx
                .send(HostToHub {
                    msg: Some(host_to_hub::Msg::SegmentDetectionAccepted(
                        SegmentDetectionAccepted {
                            request_id: job.request_id,
                            state: "rejected".into(),
                            error,
                            rejection: SegmentDetectionRejection::UnsupportedDetector as i32,
                        },
                    )),
                })
                .await
                .is_err()
            {
                return;
            }
            continue;
        }
        if tx
            .send(HostToHub {
                msg: Some(host_to_hub::Msg::SegmentDetectionAccepted(
                    SegmentDetectionAccepted {
                        request_id: job.request_id.clone(),
                        state: "queued".into(),
                        error: String::new(),
                        rejection: SegmentDetectionRejection::Unspecified as i32,
                    },
                )),
            })
            .await
            .is_err()
        {
            return;
        }
        // Announcing this before waiting on the shared permit makes queued
        // watched-first work preempt an in-flight loudness decode at its next
        // buffer checkpoint. The guard follows the blocking analyzer so a
        // dropped link cannot resume loudness while the old job still runs.
        let priority_guard = activity.segment_priority();
        tracing::info!(
            request = %job.request_id,
            collection = %job.collection_id,
            episodes = job.episodes.len(),
            "segment job queued"
        );
        let serial_guard = segment_serial.clone().lock_owned().await;
        let background_guard = loop {
            while activity.busy() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if let Some(guard) = activity.try_background() {
                break guard;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        tracing::info!(
            request = %job.request_id,
            collection = %job.collection_id,
            episodes = job.episodes.len(),
            "segment job starting"
        );
        let request_id = job.request_id.clone();
        let detector = job.detector;
        let tx2 = tx.clone();
        let collections2 = collections.clone();
        let activity2 = activity.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(cancelled.clone());
        let result = tokio::task::spawn_blocking(move || {
            // Owned by the blocking task, not its async waiter: aborting an old
            // link cannot admit replacement work until this analysis exits.
            let _serial_guard = serial_guard;
            let _background_guard = background_guard;
            let _priority_guard = priority_guard;
            analyze(
                job,
                &collections2,
                &activity2,
                &cancelled,
                hub_protocol_minor,
            )
        })
        .await;

        let result = match result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => failed(&request_id, detector, format!("{error:#}")),
            Err(error) => failed(
                &request_id,
                detector,
                format!("segment analysis task failed: {error}"),
            ),
        };
        crate::release_background_memory("segment detection");
        if result.error.is_empty() {
            tracing::info!(
                request = %result.request_id,
                episodes = result.episodes.len(),
                elapsed_ms = result.elapsed_ms,
                "segment job finished"
            );
        } else {
            tracing::warn!(
                request = %result.request_id,
                error = %result.error,
                "segment job failed"
            );
        }
        if tx2
            .send(HostToHub {
                msg: Some(host_to_hub::Msg::SegmentDetectionResult(result)),
            })
            .await
            .is_err()
        {
            return;
        }
    }
}

fn failed(request_id: &str, detector: i64, error: String) -> SegmentDetectionResult {
    SegmentDetectionResult {
        request_id: request_id.to_string(),
        detector,
        elapsed_ms: 0,
        episodes: Vec::new(),
        error,
    }
}

fn analyze(
    job: DetectSegments,
    collections: &[CollectionConfig],
    activity: &Activity,
    cancelled: &AtomicBool,
    hub_protocol_minor: u32,
) -> Result<SegmentDetectionResult> {
    anyhow::ensure!(
        job.episodes.len() >= 2,
        "segment job needs at least two episodes"
    );
    let mut prepared = Vec::with_capacity(job.episodes.len());
    let mut preflight_failures = Vec::new();
    for episode in job.episodes {
        let source = episode
            .source
            .as_ref()
            .context("segment episode missing exact source")?;
        let path = match crate::serve::resolve_rel(
            collections,
            &job.collection_id,
            &source.root_token,
            &source.path_rel,
        ) {
            Ok(path) => path,
            Err(error) => {
                preflight_failures.push(preflight_failure(&episode, 0, 0, format!("{error:#}")));
                continue;
            }
        };
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                preflight_failures.push(preflight_failure(
                    &episode,
                    0,
                    0,
                    format!("source metadata unavailable: {error}"),
                ));
                continue;
            }
        };
        let observed_size = metadata.len();
        let observed_mtime_unix = mtime_unix(&metadata);
        if observed_size != episode.expected_size
            || observed_mtime_unix != episode.expected_mtime_unix
        {
            preflight_failures.push(preflight_failure(
                &episode,
                observed_size,
                observed_mtime_unix,
                "source revision changed before analysis".into(),
            ));
            continue;
        }
        prepared.push(Prepared {
            request: episode,
            path,
        });
    }
    if prepared.len() < 2 {
        if !kahawai_proto::ProtocolFeatures::new(hub_protocol_minor)
            .supports(kahawai_proto::ProtocolFeature::RetryableSegmentResults)
        {
            // Pre-minor-6 hubs ignore the structured retryable flag. A
            // job-level transient failure makes them persist nothing, which
            // preserves the old pending behavior under inverted version skew.
            return Ok(SegmentDetectionResult {
                request_id: job.request_id,
                detector: job.detector,
                elapsed_ms: 0,
                episodes: Vec::new(),
                error: kahawai_proto::SEGMENT_COMPARISON_INSUFFICIENT.into(),
            });
        }
        preflight_failures.extend(prepared.into_iter().map(|prepared| {
            let mut result = preflight_failure(
                &prepared.request,
                prepared.request.expected_size,
                prepared.request.expected_mtime_unix,
                kahawai_proto::SEGMENT_COMPARISON_INSUFFICIENT.into(),
            );
            // Comparison insufficiency says nothing bad about these bytes.
            // The hub must keep the source pending, not quarantine its exact
            // revision beside the sibling that actually failed preflight.
            result.retryable = true;
            result
        }));
        return Ok(SegmentDetectionResult {
            request_id: job.request_id,
            detector: job.detector,
            elapsed_ms: 0,
            episodes: preflight_failures,
            error: String::new(),
        });
    }

    let episodes = prepared
        .iter()
        .map(|prepared| {
            kahawai_intro::season::Episode::new(
                prepared.path.as_path().into(),
                prepared
                    .request
                    .source
                    .as_ref()
                    .map(|source| source.path_rel.clone())
                    .unwrap_or_default(),
                Milliseconds(prepared.request.duration_ms).as_seconds(),
            )
            .with_id(prepared.request.item_id.clone())
        })
        .collect::<Vec<_>>();
    let config = kahawai_intro::season::Config {
        anime: job.anime,
        ..Default::default()
    };
    let trimmer = crate::BackgroundMemoryTrimmer::every(Duration::from_secs(30));
    let between = || -> Result<()> {
        trimmer.checkpoint("segment checkpoint");
        while activity.busy() {
            anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "segment job cancelled");
            std::thread::sleep(Duration::from_secs(2));
        }
        anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "segment job cancelled");
        Ok(())
    };
    let report = kahawai_intro::season::analyze(&episodes, &config, &between)?;

    let mut results = Vec::with_capacity(prepared.len() + preflight_failures.len());
    for (prepared, answer) in prepared.into_iter().zip(report.episodes) {
        let source = prepared.request.source.clone();
        let metadata = std::fs::metadata(&prepared.path);
        let (observed_size, observed_mtime_unix, stale_error) = match metadata {
            Ok(metadata) => {
                let size = metadata.len();
                let mtime = mtime_unix(&metadata);
                let stale = (size != prepared.request.expected_size
                    || mtime != prepared.request.expected_mtime_unix)
                    .then(|| "source revision changed during analysis".to_string());
                (size, mtime, stale)
            }
            Err(error) => (
                0,
                0,
                Some(format!("source vanished after analysis: {error}")),
            ),
        };
        let stale = stale_error.is_some();
        let mut segments = Vec::new();
        if !stale {
            if let Some(range) = answer.recap {
                segments.push(segment(
                    "recap",
                    MillisecondRange::from_seconds(range)?,
                    "blackframe",
                ));
            }
            if let Some(range) = answer.intro {
                segments.push(segment(
                    "intro",
                    MillisecondRange::from_seconds(range)?,
                    "chromaprint",
                ));
            }
            if let Some(range) = answer.credits {
                let analyzer = answer.credits_source.unwrap_or("chromaprint");
                let range = if analyzer == "blackframe" {
                    MillisecondRange::ending_at(range, Milliseconds(prepared.request.duration_ms))?
                } else {
                    MillisecondRange::from_seconds(range)?
                };
                segments.push(segment("credits", range, analyzer));
            }
        }
        let unreadable = answer.unreadable || stale;
        let error = if let Some(error) = stale_error {
            error
        } else if answer.unreadable {
            "segment analyzer could not read source".to_string()
        } else {
            String::new()
        };
        results.push(SegmentEpisodeResult {
            item_id: prepared.request.item_id,
            source,
            observed_size,
            observed_mtime_unix,
            unreadable,
            error,
            segments,
            retryable: false,
        });
    }
    results.extend(preflight_failures);

    Ok(SegmentDetectionResult {
        request_id: job.request_id,
        detector: job.detector,
        elapsed_ms: Milliseconds::from_seconds(report.seconds)?.0,
        episodes: results,
        error: String::new(),
    })
}

fn preflight_failure(
    episode: &SegmentEpisode,
    observed_size: u64,
    observed_mtime_unix: i64,
    error: String,
) -> SegmentEpisodeResult {
    SegmentEpisodeResult {
        item_id: episode.item_id.clone(),
        source: episode.source.clone(),
        observed_size,
        observed_mtime_unix,
        unreadable: true,
        error,
        segments: Vec::new(),
        retryable: false,
    }
}

/// Integer timeline value. Seconds enter only through the checked, rounded
/// constructor; protocol boundaries never cast floating-point values directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Milliseconds(u64);

impl Milliseconds {
    fn from_seconds(seconds: f64) -> Result<Self> {
        let duration = Duration::try_from_secs_f64(seconds)
            .map_err(|error| anyhow::anyhow!("invalid segment timestamp {seconds}: {error}"))?;
        let rounded = (duration.as_nanos() + 500_000) / 1_000_000;
        Ok(Self(
            u64::try_from(rounded).context("segment timestamp exceeds protocol range")?,
        ))
    }

    fn as_seconds(self) -> f64 {
        self.0 as f64 / 1000.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MillisecondRange {
    start: Milliseconds,
    end: Milliseconds,
}

impl MillisecondRange {
    fn from_seconds(range: kahawai_intro::chroma::Range) -> Result<Self> {
        Self::new(
            Milliseconds::from_seconds(range.start)?,
            Milliseconds::from_seconds(range.end)?,
        )
    }

    /// Black-frame credits end at the file's declared integer duration. Keep
    /// that authoritative endpoint rather than round-tripping it through the
    /// analyzer's floating-point seconds.
    fn ending_at(range: kahawai_intro::chroma::Range, end: Milliseconds) -> Result<Self> {
        Self::new(Milliseconds::from_seconds(range.start)?, end)
    }

    fn new(start: Milliseconds, end: Milliseconds) -> Result<Self> {
        anyhow::ensure!(end.0 > start.0, "segment range is empty or inverted");
        Ok(Self { start, end })
    }
}

fn segment(kind: &str, range: MillisecondRange, analyzer: &str) -> DetectedSegment {
    DetectedSegment {
        kind: kind.to_string(),
        start_ms: range.start.0,
        end_ms: range.end.0,
        analyzer: analyzer.to_string(),
    }
}

fn mtime_unix(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gstreamer as gst;
    use gstreamer::prelude::*;

    fn render(path: &std::path::Path) {
        kahawai_media::init().unwrap();
        let pipeline = gst::parse::launch(&format!(
            "videotestsrc num-buffers=500 ! video/x-raw,width=320,height=180,framerate=25/1 \
             ! x264enc speed-preset=ultrafast tune=zerolatency ! h264parse ! matroskamux name=m \
             audiotestsrc num-buffers=900 wave=sine freq=440 ! audioconvert ! vorbisenc ! m. \
             m. ! filesink location={}",
            path.display()
        ))
        .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline.bus().unwrap().timed_pop_filtered(
            gst::ClockTime::from_seconds(30),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(
            message
                .as_ref()
                .is_some_and(|message| message.type_() == gst::MessageType::Eos),
            "fixture pipeline failed: {message:?}"
        );
    }

    #[test]
    fn millisecond_boundaries_are_rounded_and_exact_endpoints_stay_integer() {
        let detected =
            MillisecondRange::from_seconds(kahawai_intro::chroma::Range::new(1.001, 2048.006))
                .unwrap();
        assert_eq!(detected.start, Milliseconds(1_001));
        assert_eq!(detected.end, Milliseconds(2_048_006));

        let credits = MillisecondRange::ending_at(
            kahawai_intro::chroma::Range::new(2000.0, 2048.006),
            Milliseconds(2_048_006),
        )
        .unwrap();
        assert_eq!(credits.end, Milliseconds(2_048_006));
        assert!(Milliseconds::from_seconds(f64::NAN).is_err());
        assert!(Milliseconds::from_seconds(-1.0).is_err());
    }

    #[test]
    fn comparison_insufficiency_does_not_condemn_the_readable_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("healthy.mkv"), b"healthy").unwrap();
        std::fs::write(dir.path().join("changed.mkv"), b"changed").unwrap();
        let collection = CollectionConfig {
            name: "series".into(),
            media_type: "series".into(),
            roots: vec![dir.path().to_path_buf()],
        };
        let root_token = collection.resolved_roots().next().unwrap().token;
        let episode = |id: &str, path: &str, stale: bool| {
            let metadata = std::fs::metadata(dir.path().join(path)).unwrap();
            SegmentEpisode {
                item_id: id.into(),
                source: Some(kahawai_proto::v1::SourcePath {
                    root_token: root_token.clone(),
                    path_rel: path.into(),
                }),
                expected_size: metadata.len() + u64::from(stale),
                expected_mtime_unix: mtime_unix(&metadata),
                duration_ms: 60_000,
            }
        };
        let job = || DetectSegments {
            request_id: "job".into(),
            detector: kahawai_core::segments::DETECTOR_GENERATION,
            collection_id: "series".into(),
            anime: false,
            episodes: vec![
                episode("healthy", "healthy.mkv", false),
                episode("changed", "changed.mkv", true),
            ],
        };
        let result = analyze(
            job(),
            std::slice::from_ref(&collection),
            &Activity::default(),
            &AtomicBool::new(false),
            kahawai_proto::PROTOCOL_MINOR,
        )
        .unwrap();

        let healthy = result
            .episodes
            .iter()
            .find(|episode| episode.item_id == "healthy")
            .unwrap();
        assert!(healthy.unreadable && healthy.retryable);
        let changed = result
            .episodes
            .iter()
            .find(|episode| episode.item_id == "changed")
            .unwrap();
        assert!(changed.unreadable && !changed.retryable);

        let legacy = analyze(
            job(),
            std::slice::from_ref(&collection),
            &Activity::default(),
            &AtomicBool::new(false),
            kahawai_proto::ProtocolFeature::RetryableSegmentResults.minimum_minor() - 1,
        )
        .unwrap();
        assert_eq!(legacy.error, kahawai_proto::SEGMENT_COMPARISON_INSUFFICIENT);
        assert!(
            legacy.episodes.is_empty(),
            "an old hub could persist a per-source terminal answer"
        );
    }

    #[tokio::test]
    async fn a_job_is_acknowledged_before_a_source_error_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let collection = CollectionConfig {
            name: "series".into(),
            media_type: "series".into(),
            roots: vec![dir.path().to_path_buf()],
        };
        let root_token = collection.resolved_roots().next().unwrap().token;
        let episode = |id: &str, path: &str| SegmentEpisode {
            item_id: id.into(),
            source: Some(kahawai_proto::v1::SourcePath {
                root_token: root_token.clone(),
                path_rel: path.into(),
            }),
            expected_size: 1,
            expected_mtime_unix: 1,
            duration_ms: 60_000,
        };
        let job = DetectSegments {
            request_id: "job".into(),
            detector: kahawai_core::segments::DETECTOR_GENERATION,
            collection_id: "series".into(),
            anime: false,
            episodes: vec![episode("one", "one.mkv"), episode("two", "two.mkv")],
        };
        let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel();
        let (result_tx, mut result_rx) = tokio::sync::mpsc::channel(4);
        let worker = tokio::spawn(run(
            job_rx,
            result_tx,
            vec![collection],
            Activity::default(),
            Arc::new(tokio::sync::Mutex::new(())),
            kahawai_proto::PROTOCOL_MINOR,
        ));
        job_tx.send(job).unwrap();

        let accepted = result_rx.recv().await.unwrap().msg.unwrap();
        assert!(matches!(
            accepted,
            host_to_hub::Msg::SegmentDetectionAccepted(SegmentDetectionAccepted {
                ref request_id,
                ref state,
                ..
            }) if request_id == "job" && state == "queued"
        ));
        let failed = result_rx.recv().await.unwrap().msg.unwrap();
        let host_to_hub::Msg::SegmentDetectionResult(result) = failed else {
            panic!("wrong result message");
        };
        assert!(result.error.is_empty());
        assert_eq!(result.episodes.len(), 2);
        assert!(result.episodes.iter().all(|episode| episode.unreadable));
        assert!(
            result
                .episodes
                .iter()
                .all(|episode| episode.error.contains("path not found"))
        );
        drop(job_tx);
        worker.await.unwrap();
    }
    #[tokio::test]
    async fn an_unsupported_detector_generation_is_rejected_without_running() {
        let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel();
        let (result_tx, mut result_rx) = tokio::sync::mpsc::channel(2);
        let worker = tokio::spawn(run(
            job_rx,
            result_tx,
            Vec::new(),
            Activity::default(),
            Arc::new(tokio::sync::Mutex::new(())),
            kahawai_proto::PROTOCOL_MINOR,
        ));
        job_tx
            .send(DetectSegments {
                request_id: "future".into(),
                detector: kahawai_core::segments::DETECTOR_GENERATION + 1,
                ..Default::default()
            })
            .unwrap();

        let reply = result_rx.recv().await.unwrap().msg.unwrap();
        assert!(matches!(
            reply,
            host_to_hub::Msg::SegmentDetectionAccepted(SegmentDetectionAccepted {
                ref request_id,
                ref state,
                ref error,
                rejection,
            }) if request_id == "future"
                && state == "rejected"
                && error.contains("unsupported detector generation")
                && rejection == SegmentDetectionRejection::UnsupportedDetector as i32
        ));
        assert!(
            result_rx.try_recv().is_err(),
            "rejected job produced a result"
        );
        drop(job_tx);
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn a_local_season_returns_boundaries_without_a_byte_lease() {
        let dir = tempfile::tempdir().unwrap();
        render(&dir.path().join("one.mkv"));
        render(&dir.path().join("two.mkv"));
        std::fs::write(dir.path().join("broken.mkv"), b"not a media file").unwrap();
        let collection = CollectionConfig {
            name: "series".into(),
            media_type: "series".into(),
            roots: vec![dir.path().to_path_buf()],
        };
        let root_token = collection.resolved_roots().next().unwrap().token;
        let episode = |id: &str, name: &str| {
            let metadata = std::fs::metadata(dir.path().join(name)).unwrap();
            SegmentEpisode {
                item_id: id.into(),
                source: Some(kahawai_proto::v1::SourcePath {
                    root_token: root_token.clone(),
                    path_rel: name.into(),
                }),
                expected_size: metadata.len(),
                expected_mtime_unix: mtime_unix(&metadata),
                duration_ms: 20_000,
            }
        };
        let missing = SegmentEpisode {
            item_id: "missing".into(),
            source: Some(kahawai_proto::v1::SourcePath {
                root_token: root_token.clone(),
                path_rel: "missing.mkv".into(),
            }),
            expected_size: 1,
            expected_mtime_unix: 1,
            duration_ms: 20_000,
        };
        let job = DetectSegments {
            request_id: "local".into(),
            detector: kahawai_core::segments::DETECTOR_GENERATION,
            collection_id: "series".into(),
            anime: false,
            episodes: vec![
                episode("one", "one.mkv"),
                episode("two", "two.mkv"),
                episode("broken", "broken.mkv"),
                missing,
            ],
        };
        let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel();
        let (result_tx, mut result_rx) = tokio::sync::mpsc::channel(4);
        let worker = tokio::spawn(run(
            job_rx,
            result_tx,
            vec![collection],
            Activity::default(),
            Arc::new(tokio::sync::Mutex::new(())),
            kahawai_proto::PROTOCOL_MINOR,
        ));
        job_tx.send(job).unwrap();
        assert!(matches!(
            result_rx.recv().await.unwrap().msg,
            Some(host_to_hub::Msg::SegmentDetectionAccepted(_))
        ));
        let result = tokio::time::timeout(Duration::from_secs(60), result_rx.recv())
            .await
            .expect("local analysis timed out")
            .unwrap()
            .msg
            .unwrap();
        let host_to_hub::Msg::SegmentDetectionResult(result) = result else {
            panic!("wrong result message");
        };
        assert!(result.error.is_empty(), "{}", result.error);
        assert_eq!(result.episodes.len(), 4);
        let failed = result
            .episodes
            .iter()
            .find(|episode| episode.item_id == "missing")
            .unwrap();
        assert!(failed.unreadable && failed.error.contains("path not found"));
        let broken = result
            .episodes
            .iter()
            .find(|episode| episode.item_id == "broken")
            .unwrap();
        assert!(
            broken.unreadable && !broken.error.is_empty(),
            "an analyzer read failure had no terminal reason: {broken:?}"
        );
        assert!(
            result
                .episodes
                .iter()
                .filter(|episode| !matches!(episode.item_id.as_str(), "missing" | "broken"))
                .all(|episode| !episode.unreadable
                    && episode
                        .segments
                        .iter()
                        .any(|segment| segment.kind == "intro")),
            "valid siblings were not preserved: {:?}",
            result.episodes
        );
        drop(job_tx);
        worker.await.unwrap();
    }
}
