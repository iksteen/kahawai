//! Idle source-local native and stereo loudness measurement.
//!
//! Separate from the general hasher queue: a programme decode can run for
//! minutes, while urgent subtitle extraction must remain able to enter and mark
//! the shared activity gate immediately. One background permit serializes this
//! work with hashing, extraction and season analysis; a queued season releases
//! that permit from the loudness buffer checkpoint and takes priority.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use kahawai_proto::v1::{
    AudioLoudnessTrack, FileLoudness, HostToHub, LoudnessWorklist, SourcePath, host_to_hub,
};

use crate::Activity;
use crate::scan::CollectionConfig;

struct PendingSource {
    collection_id: String,
    source: SourcePath,
    movie: bool,
    mtime_unix: i64,
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

fn enqueue(
    work: LoudnessWorklist,
    collections: &[CollectionConfig],
    seen: &mut std::collections::HashSet<(String, String, String)>,
    pending: &mut Vec<PendingSource>,
) {
    if work.analyzer != kahawai_media::loudness::ANALYZER {
        tracing::warn!(
            offered = work.analyzer,
            supported = kahawai_media::loudness::ANALYZER,
            "unsupported loudness analyzer worklist ignored"
        );
        return;
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
        let mtime_unix = crate::serve::resolve_rel(
            collections,
            &work.collection_id,
            &source.root_token,
            &source.path_rel,
        )
        .ok()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| mtime_unix(&metadata))
        .unwrap_or(i64::MIN);
        pending.push(PendingSource {
            collection_id: work.collection_id.clone(),
            source,
            movie,
            mtime_unix,
        });
        accepted += 1;
    }
    tracing::info!(
        collection = %work.collection_id,
        files = accepted,
        category = if movie { "movies" } else { "series/anime" },
        "loudness worklist queued"
    );
}

pub async fn run(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<LoudnessWorklist>,
    tx: tokio::sync::mpsc::Sender<HostToHub>,
    collections: Vec<CollectionConfig>,
    activity: Activity,
) {
    let mut seen = std::collections::HashSet::new();
    let mut pending = Vec::new();
    let mut intake_open = true;
    loop {
        if pending.is_empty() {
            let Some(work) = rx.recv().await else {
                return;
            };
            enqueue(work, &collections, &mut seen, &mut pending);
        }
        while let Ok(work) = rx.try_recv() {
            enqueue(work, &collections, &mut seen, &mut pending);
        }
        if pending.is_empty() {
            continue;
        }

        // Do not choose the next file until the permit is actually available:
        // worklists arriving during a scan, viewer lease, segment job or
        // another background tier must still be able to jump ahead.
        let background = loop {
            if !activity.busy()
                && !activity.segment_pending()
                && let Some(guard) = activity.try_background()
            {
                break Arc::new(Mutex::new(Some(guard)));
            }
            if intake_open {
                tokio::select! {
                    work = rx.recv() => match work {
                        Some(work) => enqueue(work, &collections, &mut seen, &mut pending),
                        None => intake_open = false,
                    },
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            } else {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        while let Ok(work) = rx.try_recv() {
            enqueue(work, &collections, &mut seen, &mut pending);
        }
        pending.sort_by(priority);
        let PendingSource {
            collection_id,
            source,
            movie,
            mtime_unix: queued_mtime,
        } = pending.pop().expect("queue was nonempty before permit");

        let source2 = source.clone();
        let collections2 = collections.clone();
        let activity2 = activity.clone();
        let started = Instant::now();
        tracing::info!(
            collection = %collection_id,
            path = %source.path_rel,
            category = if movie { "movies" } else { "series/anime" },
            queued_mtime,
            "audio loudness measurement starting"
        );
        let collection2 = collection_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            let background = background;
            measure_source(
                &collections2,
                &collection2,
                &source2.root_token,
                &source2.path_rel,
                &activity2,
                &background,
            )
        })
        .await;
        let (size, mtime_unix, tracks, error) = match result {
            Ok(Ok((size, mtime, Ok(tracks)))) => (size, mtime, tracks, String::new()),
            Ok(Ok((size, mtime, Err(error)))) => (size, mtime, Vec::new(), format!("{error:#}")),
            Ok(Err(error)) => (0, 0, Vec::new(), format!("{error:#}")),
            Err(error) => (0, 0, Vec::new(), format!("loudness task failed: {error}")),
        };
        crate::release_background_memory("loudness analysis");
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
        if tx
            .send(HostToHub {
                msg: Some(host_to_hub::Msg::FileLoudness(FileLoudness {
                    collection_id,
                    source: Some(source),
                    size,
                    mtime_unix,
                    analyzer: kahawai_media::loudness::ANALYZER,
                    tracks,
                    error,
                })),
            })
            .await
            .is_err()
        {
            return;
        }
    }
}

type SharedBackground = Arc<Mutex<Option<crate::BackgroundGuard>>>;

/// Segment detection is watched-first and spans a season; loudness is a
/// library backfill. Release the lower-priority permit at a GStreamer buffer
/// boundary, wait without decoding, then resume the same pipeline in place.
fn checkpoint(
    activity: &Activity,
    background: &SharedBackground,
    trimmer: &crate::BackgroundMemoryTrimmer,
    foreground_pause: &AtomicBool,
) {
    trimmer.checkpoint("loudness checkpoint");
    let snapshot = activity.snapshot();
    if snapshot.segments == 0 {
        if !snapshot.foreground_busy() {
            return;
        }
        let report = foreground_pause
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if report {
            tracing::info!(
                scans = snapshot.scans,
                leases = snapshot.leases,
                urgent = snapshot.urgent,
                "audio loudness measurement paused for foreground activity"
            );
        }
        while activity.busy() {
            std::thread::sleep(Duration::from_secs(2));
        }
        if report {
            foreground_pause.store(false, Ordering::Release);
            tracing::info!("audio loudness measurement resumed after foreground activity");
        }
        return;
    }

    let released = background.lock().unwrap().take().is_some();
    if released {
        tracing::info!("audio loudness measurement yielding to segment detection");
    }
    while activity.segment_pending() || activity.busy() {
        std::thread::sleep(Duration::from_millis(100));
    }
    loop {
        let mut held = background.lock().unwrap();
        if held.is_some() {
            break;
        }
        if let Some(guard) = activity.try_background() {
            *held = Some(guard);
            if released {
                tracing::info!("audio loudness measurement resumed");
            }
            break;
        }
        drop(held);
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn measure_source(
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    activity: &Activity,
    background: &SharedBackground,
) -> Result<(u64, i64, Result<Vec<AudioLoudnessTrack>>)> {
    let path = crate::serve::resolve_rel(collections, collection_id, root_token, path_rel)?;
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("reading metadata for {}", path.display()))?;
    let size = metadata.len();
    let mtime = mtime_unix(&metadata);
    let measured = (|| -> Result<Vec<AudioLoudnessTrack>> {
        let info = kahawai_media::discover(&path, Duration::from_secs(30))?;
        anyhow::ensure!(!info.audio.is_empty(), "source has no audio streams");

        let trimmer = crate::BackgroundMemoryTrimmer::every(Duration::from_secs(30));
        let foreground_pause = Arc::new(AtomicBool::new(false));
        let mut tracks = Vec::with_capacity(info.audio.len());
        for stream_index in 0..info.audio.len() {
            tracing::info!(
                collection = collection_id,
                path = path_rel,
                stream_index,
                streams = info.audio.len(),
                "audio loudness track starting"
            );
            let activity = activity.clone();
            let background = background.clone();
            let trimmer = trimmer.clone();
            let foreground_pause = foreground_pause.clone();
            let measured = kahawai_media::loudness::measure_file(
                &path,
                stream_index,
                info.audio[stream_index].channels,
                move || {
                    checkpoint(&activity, &background, &trimmer, &foreground_pause);
                },
            )?;
            tracks.push(AudioLoudnessTrack {
                stream_index: stream_index as u32,
                integrated_lufs: measured.stereo.integrated_lufs,
                true_peak_dbtp: measured.stereo.true_peak_dbtp,
                source_channels: measured.source_channels,
                native_integrated_lufs: measured.native.integrated_lufs,
                native_true_peak_dbtp: measured.native.true_peak_dbtp,
            });
        }
        let after = std::fs::metadata(&path)?;
        anyhow::ensure!(
            after.len() == size && mtime_unix(&after) == mtime,
            "source revision changed during loudness measurement"
        );
        Ok(tracks)
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

    #[test]
    fn foreground_pause_exposes_its_owner_and_resumes() {
        let activity = Activity::default();
        let background = Arc::new(Mutex::new(Some(
            activity.try_background().expect("initial loudness permit"),
        )));
        let lease = activity.lease();
        let snapshot = activity.snapshot();
        assert_eq!(
            (snapshot.scans, snapshot.leases, snapshot.urgent),
            (0, 1, 0)
        );

        let pause = Arc::new(AtomicBool::new(false));
        let pause2 = pause.clone();
        let activity2 = activity.clone();
        let background2 = background.clone();
        let worker = std::thread::spawn(move || {
            let trimmer = crate::BackgroundMemoryTrimmer::every(Duration::from_secs(60));
            checkpoint(&activity2, &background2, &trimmer, &pause2);
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while !pause.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "foreground pause was not recorded"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(lease);
        worker.join().unwrap();
        assert!(!pause.load(Ordering::Acquire));
        assert!(!activity.busy());
    }

    #[test]
    fn queued_segment_takes_and_returns_the_loudness_permit() {
        let activity = Activity::default();
        let background = Arc::new(Mutex::new(Some(
            activity.try_background().expect("initial loudness permit"),
        )));
        let priority = activity.segment_priority();
        let activity2 = activity.clone();
        let trimmer = crate::BackgroundMemoryTrimmer::every(Duration::from_secs(60));
        let background2 = background.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let trimmer2 = trimmer.clone();
        let pause = AtomicBool::new(false);
        let lower = std::thread::spawn(move || {
            checkpoint(&activity2, &background2, &trimmer2, &pause);
            done_tx.send(()).unwrap();
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        let segment_background = loop {
            if let Some(guard) = activity.try_background() {
                break guard;
            }
            assert!(
                Instant::now() < deadline,
                "loudness did not release its permit"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "loudness resumed while segment detection was still pending"
        );
        drop(priority);
        drop(segment_background);
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("loudness did not resume");
        lower.join().unwrap();
        assert!(
            background.lock().unwrap().is_some(),
            "loudness did not reacquire the permit"
        );
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
        let activity = Activity::default();
        let foreground = activity.lease();
        let (work_tx, work_rx) = tokio::sync::mpsc::unbounded_channel();
        let (result_tx, mut result_rx) = tokio::sync::mpsc::channel(2);
        let worker = tokio::spawn(run(work_rx, result_tx, vec![collection], activity));
        work_tx
            .send(LoudnessWorklist {
                collection_id: "series".into(),
                analyzer: kahawai_media::loudness::ANALYZER,
                sources: vec![kahawai_proto::v1::SourcePath::new(
                    root_token,
                    "episode.mkv",
                )],
            })
            .unwrap();

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
        assert!(result.tracks[0].native_integrated_lufs.is_finite());
        assert!(result.tracks[0].native_true_peak_dbtp.is_finite());
        drop(work_tx);
        worker.await.unwrap();
    }
}
