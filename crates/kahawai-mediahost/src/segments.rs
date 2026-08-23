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
    DetectSegments, DetectedSegment, HostToHub, SegmentDetectionAccepted, SegmentDetectionResult,
    SegmentEpisode, SegmentEpisodeResult, host_to_hub,
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
) {
    while let Some(job) = rx.recv().await {
        if tx
            .send(HostToHub {
                msg: Some(host_to_hub::Msg::SegmentDetectionAccepted(
                    SegmentDetectionAccepted {
                        request_id: job.request_id.clone(),
                        state: "queued".into(),
                        error: String::new(),
                    },
                )),
            })
            .await
            .is_err()
        {
            return;
        }
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
            analyze(job, &collections2, &activity2, &cancelled)
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
        preflight_failures.extend(prepared.into_iter().map(|prepared| {
            preflight_failure(
                &prepared.request,
                prepared.request.expected_size,
                prepared.request.expected_mtime_unix,
                "fewer than two readable episodes remain".into(),
            )
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
                prepared.request.duration_ms as f64 / 1000.0,
            )
            .with_id(prepared.request.item_id.clone())
        })
        .collect::<Vec<_>>();
    let config = kahawai_intro::season::Config {
        anime: job.anime,
        ..Default::default()
    };
    let between = || -> Result<()> {
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
                segments.push(segment("recap", range, "blackframe"));
            }
            if let Some(range) = answer.intro {
                segments.push(segment("intro", range, "chromaprint"));
            }
            if let Some(range) = answer.credits {
                segments.push(segment(
                    "credits",
                    range,
                    answer.credits_source.unwrap_or("chromaprint"),
                ));
            }
        }
        results.push(SegmentEpisodeResult {
            item_id: prepared.request.item_id,
            source,
            observed_size,
            observed_mtime_unix,
            unreadable: answer.unreadable || stale,
            error: stale_error.unwrap_or_default(),
            segments,
        });
    }
    results.extend(preflight_failures);

    Ok(SegmentDetectionResult {
        request_id: job.request_id,
        detector: job.detector,
        elapsed_ms: (report.seconds * 1000.0) as u64,
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
    }
}

fn segment(kind: &str, range: kahawai_intro::chroma::Range, analyzer: &str) -> DetectedSegment {
    DetectedSegment {
        kind: kind.to_string(),
        start_ms: (range.start * 1000.0) as u64,
        end_ms: (range.end * 1000.0) as u64,
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
            detector: 2,
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
    async fn a_local_season_returns_boundaries_without_a_byte_lease() {
        let dir = tempfile::tempdir().unwrap();
        render(&dir.path().join("one.mkv"));
        render(&dir.path().join("two.mkv"));
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
            detector: 2,
            collection_id: "series".into(),
            anime: false,
            episodes: vec![
                episode("one", "one.mkv"),
                episode("two", "two.mkv"),
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
        assert_eq!(result.episodes.len(), 3);
        let failed = result
            .episodes
            .iter()
            .find(|episode| episode.item_id == "missing")
            .unwrap();
        assert!(failed.unreadable && failed.error.contains("path not found"));
        assert!(
            result
                .episodes
                .iter()
                .filter(|episode| episode.item_id != "missing")
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
