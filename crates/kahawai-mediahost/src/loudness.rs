//! Source-local loudness measurement for exact playback layouts.
//!
//! Loudness retains its movie/newest-first ordering within the least-important
//! work class. The process scheduler owns admission and interruption.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use kahawai_proto::v1::{
    AudioLayoutLoudness, AudioLoudnessTrack, FileLoudness, HostToHub, LoudnessWorklist, SourcePath,
    host_to_hub,
};

use crate::scan::CollectionConfig;
use crate::scheduler::{JobPermit, Priority, Scheduler};

struct TrimOnDrop(&'static str);

impl Drop for TrimOnDrop {
    fn drop(&mut self) {
        crate::release_background_memory(self.0);
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct PendingSource {
    collection_id: String,
    source: SourcePath,
    movie: bool,
    mtime_unix: i64,
    analyzer: i64,
}

/// Ascending for `sort_by`; the worker pops the greatest priority. Paths are
/// inverted only to make equal-mtime ordering deterministic.
fn priority(left: &PendingSource, right: &PendingSource) -> std::cmp::Ordering {
    left.movie
        .cmp(&right.movie)
        .then(left.mtime_unix.cmp(&right.mtime_unix))
        .then_with(|| right.collection_id.cmp(&left.collection_id))
        .then_with(|| right.source.path_rel.cmp(&left.source.path_rel))
}

async fn enqueue(
    work: LoudnessWorklist,
    collections: &[CollectionConfig],
    scheduler: &Scheduler,
    owner: Option<&str>,
    seen: &mut std::collections::HashSet<(String, String, String)>,
    pending: &mut Vec<PendingSource>,
) -> Result<()> {
    if !matches!(work.analyzer, 3 | 4 | 5 | kahawai_media::loudness::ANALYZER) {
        tracing::warn!(
            offered = work.analyzer,
            supported = ?[3, 4, 5, kahawai_media::loudness::ANALYZER],
            "unsupported loudness analyzer worklist ignored"
        );
        return Ok(());
    }
    let movie = collections
        .iter()
        .find(|collection| collection.name == work.collection_id)
        .is_some_and(|collection| collection.media_type == "movies");
    let mut accepted = 0usize;
    for source in work.sources {
        let key = (
            work.collection_id.clone(),
            source.root_token.clone(),
            source.path_rel.clone(),
        );
        if !seen.insert(key) {
            continue;
        }
        let resources = scheduler.resources([source.root_token.as_str()], false);
        let permit = scheduler
            .acquire(
                Priority::LocalMetadata,
                resources,
                owner.map(str::to_string),
                format!(
                    "loudness ordering {}/{}",
                    work.collection_id, source.path_rel
                ),
            )
            .await?;
        let collections = collections.to_vec();
        let collection_id = work.collection_id.clone();
        let root_token = source.root_token.clone();
        let path_rel = source.path_rel.clone();
        let mtime_unix = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            crate::serve::resolve_rel(&collections, &collection_id, &root_token, &path_rel)
                .ok()
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|metadata| mtime_unix(&metadata))
                .unwrap_or(i64::MIN)
        })
        .await
        .context("loudness ordering metadata task failed")?;
        pending.push(PendingSource {
            collection_id: work.collection_id.clone(),
            source,
            movie,
            mtime_unix,
            analyzer: work.analyzer,
        });
        accepted += 1;
    }
    tracing::info!(
        collection = %work.collection_id,
        files = accepted,
        category = if movie { "movies" } else { "series/anime" },
        "loudness worklist queued"
    );
    Ok(())
}

pub async fn run(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<LoudnessWorklist>,
    tx: tokio::sync::mpsc::Sender<HostToHub>,
    collections: Vec<CollectionConfig>,
    scheduler: Scheduler,
    owner: Option<String>,
) {
    let mut seen = std::collections::HashSet::new();
    let mut pending = Vec::new();
    loop {
        if pending.is_empty() {
            let Some(work) = rx.recv().await else {
                return;
            };
            if enqueue(
                work,
                &collections,
                &scheduler,
                owner.as_deref(),
                &mut seen,
                &mut pending,
            )
            .await
            .is_err()
            {
                return;
            }
        }
        while let Ok(work) = rx.try_recv() {
            if enqueue(
                work,
                &collections,
                &scheduler,
                owner.as_deref(),
                &mut seen,
                &mut pending,
            )
            .await
            .is_err()
            {
                return;
            }
        }
        if pending.is_empty() {
            continue;
        }

        while let Ok(work) = rx.try_recv() {
            if enqueue(
                work,
                &collections,
                &scheduler,
                owner.as_deref(),
                &mut seen,
                &mut pending,
            )
            .await
            .is_err()
            {
                return;
            }
        }
        pending.sort_by(priority);
        let PendingSource {
            collection_id,
            source,
            movie,
            mtime_unix: queued_mtime,
            analyzer,
        } = pending.pop().expect("queue was nonempty before permit");

        let resources = scheduler.resources([source.root_token.as_str()], true);
        let permit = match scheduler
            .acquire(
                Priority::Loudness,
                resources,
                owner.clone(),
                format!("loudness {collection_id}/{}", source.path_rel),
            )
            .await
        {
            Ok(permit) => permit,
            Err(_) => return,
        };

        let source2 = source.clone();
        let collections2 = collections.clone();
        let started = Instant::now();
        tracing::info!(
            collection = %collection_id,
            path = %source.path_rel,
            category = if movie { "movies" } else { "series/anime" },
            queued_mtime,
            "audio loudness measurement starting"
        );
        let collection2 = collection_id.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(cancelled.clone());
        let result = tokio::task::spawn_blocking(move || {
            let _trim = TrimOnDrop("loudness analysis");
            measure_source(
                &collections2,
                &collection2,
                &source2.root_token,
                &source2.path_rel,
                &permit,
                analyzer,
                cancelled,
            )
        })
        .await;
        let (size, mtime_unix, tracks, error) = match result {
            Ok(Ok((size, mtime, Ok((tracks, error))))) => (size, mtime, tracks, error),
            Ok(Ok((size, mtime, Err(error)))) => (size, mtime, Vec::new(), format!("{error:#}")),
            Ok(Err(error)) => (0, 0, Vec::new(), format!("{error:#}")),
            Err(error) => (0, 0, Vec::new(), format!("loudness task failed: {error}")),
        };
        if error.is_empty() {
            tracing::info!(
                collection = %collection_id,
                path = %source.path_rel,
                tracks = tracks.len(),
                elapsed_secs = started.elapsed().as_secs(),
                "audio loudness measured"
            );
        } else {
            tracing::warn!(
                collection = %collection_id,
                path = %source.path_rel,
                elapsed_secs = started.elapsed().as_secs(),
                %error,
                "audio loudness measurement failed"
            );
        }
        let seen_key = (
            collection_id.clone(),
            source.root_token.clone(),
            source.path_rel.clone(),
        );
        // Stop suppressing this path before publishing its result: the hub may
        // immediately re-offer a replacement revision that made this answer
        // stale, and that targeted retry must survive the round trip.
        seen.remove(&seen_key);
        let sent = tx
            .send(HostToHub {
                msg: Some(host_to_hub::Msg::FileLoudness(FileLoudness {
                    collection_id,
                    source: Some(source),
                    size,
                    mtime_unix,
                    analyzer,
                    tracks,
                    error,
                })),
            })
            .await;
        if sent.is_err() {
            return;
        }
    }
}

type MeasuredTracks = Result<(Vec<AudioLoudnessTrack>, String)>;

fn checkpoint(
    permit: &JobPermit,
    trimmer: &crate::BackgroundMemoryTrimmer,
    cancelled: &AtomicBool,
) -> Result<()> {
    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "loudness job cancelled");
    trimmer.checkpoint("loudness checkpoint");
    permit.checkpoint_blocking()
}

#[allow(clippy::too_many_arguments)] // exact source + scheduling context
fn measure_source(
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    permit: &JobPermit,
    analyzer: i64,
    cancelled: Arc<AtomicBool>,
) -> Result<(u64, i64, MeasuredTracks)> {
    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "loudness job cancelled");
    let path = crate::serve::resolve_rel(collections, collection_id, root_token, path_rel)?;
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("reading metadata for {}", path.display()))?;
    let size = metadata.len();
    let mtime = mtime_unix(&metadata);
    if analyzer < kahawai_media::loudness::ANALYZER {
        return Ok((
            size,
            mtime,
            Err(anyhow::anyhow!(
                "legacy loudness analyzer superseded by corrected channel weighting"
            )),
        ));
    }
    let measured = (|| -> Result<(Vec<AudioLoudnessTrack>, String)> {
        let info = kahawai_media::discover(&path, Duration::from_secs(30))?;
        anyhow::ensure!(!cancelled.load(Ordering::Acquire), "loudness job cancelled");
        anyhow::ensure!(!info.audio.is_empty(), "source has no audio streams");

        let trimmer = crate::BackgroundMemoryTrimmer::every(Duration::from_secs(30));
        let mut tracks = Vec::with_capacity(info.audio.len());
        let mut failures = Vec::new();
        for stream_index in 0..info.audio.len() {
            anyhow::ensure!(!cancelled.load(Ordering::Acquire), "loudness job cancelled");
            tracing::info!(
                collection = collection_id,
                path = path_rel,
                stream_index,
                streams = info.audio.len(),
                "audio loudness track starting"
            );
            let result = (|| -> Result<AudioLoudnessTrack> {
                let permit = permit.clone();
                let trimmer = trimmer.clone();
                let cancelled = cancelled.clone();
                let source_layout = kahawai_media::loudness::AudioLayout::from_stream(
                    info.audio[stream_index].channels,
                    info.audio[stream_index].layout.as_deref(),
                );
                let measured = kahawai_media::loudness::measure_file(
                    &path,
                    stream_index,
                    source_layout,
                    move || checkpoint(&permit, &trimmer, &cancelled),
                )?;
                // Positionless noncanonical input has no honest native gain
                // key; exact canonical layouts remain usable. Legacy scalar
                // fields fall back to the largest measured conversion only.
                let native = measured.get(measured.source);
                anyhow::ensure!(
                    analyzer >= 5 || native.is_some(),
                    "legacy analyzer cannot represent positionless native layout"
                );
                let stereo = measured
                    .get(kahawai_media::loudness::AudioLayout::new(2, 0x3))
                    .or_else(|| measured.layouts.first().map(|layout| layout.loudness))
                    .context("loudness measurement has no output layouts")?;
                // Legacy scalar hubs must not relabel a canonical conversion
                // as positionless native. NaN is their historical unmeasured
                // sentinel; current hubs consume the exact layout map.
                let legacy_native = native.unwrap_or(kahawai_media::loudness::AudioLoudness {
                    integrated_lufs: f64::NAN,
                    true_peak_dbtp: f64::NAN,
                });
                Ok(AudioLoudnessTrack {
                    stream_index: stream_index as u32,
                    integrated_lufs: stereo.integrated_lufs,
                    true_peak_dbtp: stereo.true_peak_dbtp,
                    source_channels: measured.source.channels,
                    source_channel_mask: measured.source.channel_mask,
                    native_integrated_lufs: legacy_native.integrated_lufs,
                    native_true_peak_dbtp: legacy_native.true_peak_dbtp,
                    layouts: measured
                        .layouts
                        .into_iter()
                        .map(|measurement| AudioLayoutLoudness {
                            channels: measurement.layout.channels,
                            channel_mask: measurement.layout.channel_mask,
                            integrated_lufs: measurement.loudness.integrated_lufs,
                            true_peak_dbtp: measurement.loudness.true_peak_dbtp,
                        })
                        .collect(),
                })
            })();
            match result {
                Ok(track) => tracks.push(track),
                Err(error) => {
                    tracing::warn!(
                        collection = collection_id,
                        path = path_rel,
                        stream_index,
                        error = format!("{error:#}"),
                        "audio loudness track failed"
                    );
                    failures.push(format!("audio stream {stream_index}: {error:#}"));
                }
            }
        }
        let after = std::fs::metadata(&path)?;
        anyhow::ensure!(
            after.len() == size && mtime_unix(&after) == mtime,
            "source revision changed during loudness measurement"
        );
        Ok((tracks, failures.join("; ")))
    })();
    Ok((size, mtime, measured))
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

    #[test]
    fn movies_win_then_newer_mtime_within_each_category() {
        let item = |movie, mtime_unix, path: &str| PendingSource {
            collection_id: if movie { "movies" } else { "series" }.into(),
            source: SourcePath::new("root", path),
            movie,
            mtime_unix,
            analyzer: kahawai_media::loudness::ANALYZER,
        };
        let mut pending = vec![
            item(false, 100, "new-series.mkv"),
            item(true, 10, "old-movie.mkv"),
            item(false, 5, "old-series.mkv"),
            item(true, 20, "new-movie.mkv"),
        ];
        pending.sort_by(priority);
        let order = std::iter::from_fn(|| pending.pop())
            .map(|item| item.source.path_rel)
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            [
                "new-movie.mkv",
                "old-movie.mkv",
                "new-series.mkv",
                "old-series.mkv",
            ]
        );
    }
    #[tokio::test]
    async fn minor_four_worklists_keep_the_legacy_analyzer() {
        let collection = CollectionConfig {
            name: "movies".into(),
            media_type: "movies".into(),
            roots: Vec::new(),
        };
        let mut seen = std::collections::HashSet::new();
        let mut pending = Vec::new();
        let scheduler =
            Scheduler::new(std::slice::from_ref(&collection), &Default::default()).unwrap();
        enqueue(
            LoudnessWorklist {
                collection_id: "movies".into(),
                analyzer: 3,
                sources: vec![SourcePath::new("root", "film.mkv")],
            },
            &[collection],
            &scheduler,
            None,
            &mut seen,
            &mut pending,
        )
        .await
        .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].analyzer, 3);
    }

    #[test]
    fn dropping_a_link_waiter_marks_its_blocking_measurement_cancelled() {
        let cancelled = Arc::new(AtomicBool::new(false));
        drop(CancelOnDrop(cancelled.clone()));
        assert!(cancelled.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn worklist_measures_every_audio_track_locally() {
        if !kahawai_media::testutil::require_elements(&["x264enc", "fdkaacenc"]) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("episode.mkv");
        kahawai_media::testutil::render_h264_aac51_mkv(&path);
        let collection = CollectionConfig {
            name: "series".into(),
            media_type: "series".into(),
            roots: vec![dir.path().to_path_buf()],
        };
        let root_token = collection.resolved_roots().next().unwrap().token;
        let scheduler =
            Scheduler::new(std::slice::from_ref(&collection), &Default::default()).unwrap();
        let foreground = scheduler.enter_interactive(
            scheduler.resources([root_token.as_str()], false),
            "test viewer",
        );
        let (work_tx, work_rx) = tokio::sync::mpsc::unbounded_channel();
        let (result_tx, mut result_rx) = tokio::sync::mpsc::channel(2);
        let worker = tokio::spawn(run(work_rx, result_tx, vec![collection], scheduler, None));
        let work = LoudnessWorklist {
            collection_id: "series".into(),
            analyzer: kahawai_media::loudness::ANALYZER,
            sources: vec![kahawai_proto::v1::SourcePath::new(
                root_token,
                "episode.mkv",
            )],
        };
        work_tx.send(work.clone()).unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(200), result_rx.recv())
                .await
                .is_err(),
            "measurement started while a viewer lease was active"
        );
        drop(foreground);

        let message = tokio::time::timeout(Duration::from_secs(30), result_rx.recv())
            .await
            .expect("measurement timed out")
            .unwrap()
            .msg
            .unwrap();
        let host_to_hub::Msg::FileLoudness(result) = message else {
            panic!("wrong result message");
        };
        assert!(result.error.is_empty(), "{}", result.error);
        assert_eq!(result.tracks.len(), 1);
        assert!(result.tracks[0].integrated_lufs.is_finite());
        assert!(result.tracks[0].true_peak_dbtp.is_finite());
        assert_eq!(result.tracks[0].source_channels, 6);
        assert!(
            result.tracks[0].native_integrated_lufs.is_nan()
                && result.tracks[0].native_true_peak_dbtp.is_nan(),
            "positionless native is the legacy unmeasured sentinel"
        );
        assert_eq!(result.tracks[0].layouts.len(), 3);

        // `seen` is pending/in-flight deduplication, not lifetime history. A
        // later scan can invalidate this revision and send the same path. The
        // analyzer-3 request receives a terminal answer when its scalar-only
        // schema cannot honestly represent this positionless native layout.
        let mut legacy = work;
        legacy.analyzer = 3;
        work_tx.send(legacy).unwrap();
        let repeated = tokio::time::timeout(Duration::from_secs(30), result_rx.recv())
            .await
            .expect("requeued measurement timed out")
            .unwrap()
            .msg
            .unwrap();
        let host_to_hub::Msg::FileLoudness(repeated) = repeated else {
            panic!("wrong repeated result message");
        };
        assert!(
            repeated.error.contains("legacy loudness"),
            "{}",
            repeated.error
        );
        assert!(repeated.tracks.is_empty());
        assert_eq!(repeated.analyzer, 3);
        drop(work_tx);
        worker.await.unwrap();
    }
}
