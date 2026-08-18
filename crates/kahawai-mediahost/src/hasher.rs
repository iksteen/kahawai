//! The background job worker: ED2K hashing (MH-9) and subtitle
//! extraction (efficiency ladder step 2), one file at a time, in three
//! tiers — urgent subtitle jobs (a viewer waits: run immediately),
//! ED2K (idle-gated), background subtitle pre-warm (idle-gated,
//! drained only when the ED2K queue is empty).

use std::collections::{HashSet, VecDeque};
use std::io::Read;
use std::time::Duration;

use kahawai_proto::v1::{
    AttachmentsWorklist, ExtractSubs, FileAttachments, FileHash, FileHashes, FileKeyframeInterval,
    FileSubtitles, FileVideoGeometry, Hashlist, HostToHub, SubTrack, SubsWorklist, host_to_hub,
};

use crate::Activity;
use crate::ed2k::{self, CHUNK, Ed2k};
use crate::scan::CollectionConfig;

/// Pause between chunks even when idle: bounds the read rate (~95 MB/s)
/// so the hasher never monopolizes the disk it shares with everything else.
const CHUNK_PACE: Duration = Duration::from_millis(100);
const BUSY_POLL: Duration = Duration::from_secs(2);

/// Work arriving from the hub via the link dispatch loop.
pub enum JobMsg {
    Hashlist(Hashlist),
    SubsWorklist(SubsWorklist),
    AttachmentsWorklist(AttachmentsWorklist),
    KeyframeWorklist(kahawai_proto::v1::KeyframeWorklist),
    VideoGeometryWorklist(kahawai_proto::v1::VideoGeometryWorklist),
    Urgent(ExtractSubs),
    UrgentImage(kahawai_proto::v1::ExtractImageSubs),
}

/// One deduped work queue (collection, root token, path_rel) per tier.
#[derive(Default)]
struct Tier {
    q: VecDeque<(String, String, String)>,
    seen: HashSet<(String, String, String)>,
}

fn source(root_token: &str, path_rel: &str) -> Option<kahawai_proto::v1::SourcePath> {
    Some(kahawai_proto::v1::SourcePath {
        root_token: root_token.to_string(),
        path_rel: path_rel.to_string(),
    })
}

impl Tier {
    fn push(&mut self, collection_id: &str, sources: Vec<kahawai_proto::v1::SourcePath>) {
        for source in sources {
            let key = (
                collection_id.to_string(),
                source.root_token,
                source.path_rel,
            );
            if self.seen.insert(key.clone()) {
                self.q.push_back(key);
            }
        }
    }
}

#[derive(Default)]
struct Queues {
    urgent: VecDeque<(String, String, String)>,
    urgent_image: VecDeque<kahawai_proto::v1::ExtractImageSubs>,
    ed2k: Tier,
    subs: Tier,
    atts: Tier,
    keys: Tier,
    geometry: Tier,
}

/// Route one message into its tier; returns true for urgent work.
fn intake(msg: JobMsg, queues: &mut Queues) -> bool {
    match msg {
        // Same urgency as a text extraction: a viewer is waiting on it
        // to start a burn-in session.
        JobMsg::UrgentImage(e) => {
            queues.urgent_image.push_back(e);
            true
        }
        JobMsg::Urgent(e) => {
            if let Some(source) = e.source {
                queues
                    .urgent
                    .push_back((e.collection_id, source.root_token, source.path_rel));
            }
            true
        }
        JobMsg::Hashlist(h) => {
            queues.ed2k.push(&h.collection_id, h.sources);
            false
        }
        JobMsg::SubsWorklist(w) => {
            queues.subs.push(&w.collection_id, w.sources);
            false
        }
        JobMsg::AttachmentsWorklist(w) => {
            queues.atts.push(&w.collection_id, w.sources);
            false
        }
        JobMsg::KeyframeWorklist(w) => {
            // Logged on receipt, mirroring the hub's "sending" line:
            // between them, a worklist that never arrives is one grep
            // rather than a guess.
            tracing::info!(collection = %w.collection_id, files = w.sources.len(),
                "keyframe worklist received");
            queues.keys.push(&w.collection_id, w.sources);
            false
        }
        JobMsg::VideoGeometryWorklist(w) => {
            tracing::info!(collection = %w.collection_id, files = w.sources.len(),
                "video geometry worklist received");
            queues.geometry.push(&w.collection_id, w.sources);
            false
        }
    }
}

/// Which background tier a drained job came from — three of them now,
/// and a bool cannot say.
#[derive(Clone, Copy)]
enum Bg {
    Atts,
    Keyframe,
    Geometry,
    Subs,
}

pub async fn run(
    mut rx: tokio::sync::mpsc::Receiver<JobMsg>,
    tx: tokio::sync::mpsc::Sender<HostToHub>,
    collections: Vec<CollectionConfig>,
    activity: Activity,
) {
    let mut queues = Queues::default();

    loop {
        // Drain new work; block only when every queue is empty.
        //
        // EVERY queue, and the background ones are listed once so they
        // cannot drift apart: a tier missing from this check is work
        // that sits in its queue while the loop blocks on `recv()`
        // waiting for a message that never comes — which is exactly
        // what a fourth tier did on the day it was added.
        loop {
            let background = [&queues.atts, &queues.keys, &queues.geometry, &queues.subs];
            let empty = queues.urgent.is_empty()
                && queues.urgent_image.is_empty()
                && queues.ed2k.q.is_empty()
                && background.iter().all(|t| t.q.is_empty());
            let msg = if empty {
                match rx.recv().await {
                    Some(m) => m,
                    None => return,
                }
            } else {
                match rx.try_recv() {
                    Ok(m) => m,
                    Err(_) => break,
                }
            };
            intake(msg, &mut queues);
        }

        // Tier order: urgent (never idle-gated — the active lease IS the
        // requesting viewer) → ED2K → background subs.
        if let Some((collection_id, root_token, path_rel)) = queues.urgent.pop_front() {
            extract_and_send(&collections, &collection_id, &root_token, &path_rel, &tx).await;
            continue;
        }
        if let Some(e) = queues.urgent_image.pop_front() {
            if let Some(source) = e.source {
                extract_image_and_send(
                    &collections,
                    &e.collection_id,
                    &source.root_token,
                    &source.path_rel,
                    e.sub_index,
                    &tx,
                )
                .await;
            }
            continue;
        }
        if let Some((collection_id, root_token, path_rel)) = queues.ed2k.q.pop_front() {
            match hash_one(
                &collections,
                &collection_id,
                &root_token,
                &path_rel,
                &activity,
            )
            .await
            {
                Ok(mut fh) => {
                    fh.source = source(&root_token, &path_rel);
                    tracing::info!(collection = %collection_id, path = %path_rel,
                        ed2k = %fh.ed2k_hex, crc_ok = fh.crc_ok || !fh.crc_checked, "ed2k computed");
                    let msg = HostToHub {
                        msg: Some(host_to_hub::Msg::FileHashes(FileHashes {
                            collection_id: collection_id.clone(),
                            hashes: vec![fh],
                        })),
                    };
                    if tx.send(msg).await.is_err() {
                        return; // link gone; the next session gets a fresh list
                    }
                }
                // Vanished or unreadable: the next scan reconciles.
                Err(e) => tracing::debug!(collection = %collection_id, path = %path_rel,
                    error = format!("{e:#}"), "ed2k skipped"),
            }
            queues
                .ed2k
                .seen
                .remove(&(collection_id, root_token, path_rel));
            continue;
        }
        // Background tiers share the idle gate. Attachment declaration
        // drains FIRST: it is ~10x cheaper per file (header reads only)
        // and unblocks font serving, while the subs pre-warm is a
        // long-tail warmup that can wait behind it.
        // Attachments first (cheapest, unblocks font serving), then
        // keyframe intervals — the same index-read cost, and every file
        // still missing one forces a conservative TARGETDURATION until
        // it lands. The subs pre-warm is the long tail and waits.
        let (which, job) = match queues.atts.q.pop_front() {
            Some(j) => (Bg::Atts, Some(j)),
            None => match queues.keys.q.pop_front() {
                Some(j) => (Bg::Keyframe, Some(j)),
                None => match queues.geometry.q.pop_front() {
                    Some(j) => (Bg::Geometry, Some(j)),
                    None => (Bg::Subs, queues.subs.q.pop_front()),
                },
            },
        };
        if let Some((collection_id, root_token, path_rel)) = job {
            // Idle gate before starting; the work itself is a bounded
            // local read (ponytail: can't pause a gst pipeline mid-walk).
            let mut preempted = false;
            while activity.busy() {
                match tokio::time::timeout(BUSY_POLL, rx.recv()).await {
                    Ok(Some(m)) => {
                        if intake(m, &mut queues) {
                            preempted = true;
                            break;
                        }
                    }
                    Ok(None) => return,
                    Err(_) => {} // still busy; keep waiting
                }
            }
            let tier = match which {
                Bg::Subs => &mut queues.subs,
                Bg::Atts => &mut queues.atts,
                Bg::Keyframe => &mut queues.keys,
                Bg::Geometry => &mut queues.geometry,
            };
            if preempted || !queues.urgent.is_empty() {
                // Preempted: put the background job back and loop.
                tier.q.push_front((collection_id, root_token, path_rel));
                continue;
            }
            match which {
                Bg::Subs => {
                    extract_and_send(&collections, &collection_id, &root_token, &path_rel, &tx)
                        .await
                }
                Bg::Atts => {
                    declare_and_send(&collections, &collection_id, &root_token, &path_rel, &tx)
                        .await
                }
                Bg::Keyframe => {
                    measure_keyframes_and_send(
                        &collections,
                        &collection_id,
                        &root_token,
                        &path_rel,
                        &tx,
                    )
                    .await
                }
                Bg::Geometry => {
                    probe_geometry_and_send(
                        &collections,
                        &collection_id,
                        &root_token,
                        &path_rel,
                        &tx,
                    )
                    .await
                }
            }
            tier.seen.remove(&(collection_id, root_token, path_rel));
        }
    }
}

/// MH-4 backfill: declare one file's attachments and its chapters —
/// sparse header reads, ~0.3 s even over a network mount — and ship them
/// to the hub. An error here sends NOTHING, on purpose: the reader settles
/// deterministic shape problems as "[]" itself, so what escapes is
/// weather (I/O on the mount), and the file stays listed to be retried.
async fn declare_and_send(
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
) {
    let result: anyhow::Result<(u64, String, String)> = async {
        let path = crate::serve::resolve_rel(collections, collection_id, root_token, path_rel)?;
        let size = std::fs::metadata(&path)?.len();
        let (atts, chapters) =
            tokio::task::spawn_blocking(move || kahawai_media::subindex::declare_container(&path))
                .await??;
        Ok((
            size,
            serde_json::to_string(&atts)?,
            serde_json::to_string(&chapters)?,
        ))
    }
    .await;
    let (size, attachments_json, chapters_json) = match result {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(collection = %collection_id, path = %path_rel,
                error = format!("{e:#}"), "attachment declaration failed");
            return; // vanished/unreadable: the next scan reconciles
        }
    };
    // Say which, because the one investigation this line shows up in is
    // "why has this file no chapters", and a message that claims both when
    // only the fonts were found answers it wrongly.
    if attachments_json != "[]" || chapters_json != "[]" {
        tracing::info!(
            collection = %collection_id, path = %path_rel,
            attachments = attachments_json != "[]",
            chapters = chapters_json != "[]",
            "container header declared"
        );
    }
    let msg = HostToHub {
        msg: Some(host_to_hub::Msg::FileAttachments(FileAttachments {
            collection_id: collection_id.to_string(),
            source: source(root_token, path_rel),
            size,
            attachments_json,
            chapters_json: Some(chapters_json),
        })),
    };
    let _ = tx.send(msg).await;
}

/// HUB-17 backfill: measure one file's longest keyframe gap from the
/// container index and ship it. An UNKNOWN result is reported too —
/// silence would leave the file in the worklist forever, and "we looked
/// and this container has no index we can read" is a real answer.
async fn measure_keyframes_and_send(
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
) {
    let result: anyhow::Result<(u64, Option<u32>)> = async {
        let path = crate::serve::resolve_rel(collections, collection_id, root_token, path_rel)?;
        let size = std::fs::metadata(&path)?.len();
        let ms = tokio::task::spawn_blocking(move || {
            kahawai_media::subindex::max_keyframe_interval_ms(&path)
        })
        .await?
        .unwrap_or(None);
        Ok((size, ms))
    }
    .await;
    let (size, ms) = match result {
        Ok(v) => v,
        // Vanished or unreadable: say nothing and let the next scan
        // reconcile, rather than recording a measurement we did not make.
        Err(e) => {
            tracing::debug!(collection = %collection_id, path = %path_rel,
                error = format!("{e:#}"), "keyframe measurement failed");
            return;
        }
    };
    tracing::debug!(collection = %collection_id, path = %path_rel, ms = ?ms,
        "keyframe interval measured");
    let msg = HostToHub {
        msg: Some(host_to_hub::Msg::FileKeyframeInterval(
            FileKeyframeInterval {
                collection_id: collection_id.to_string(),
                source: source(root_token, path_rel),
                size,
                max_keyframe_interval_ms: ms,
            },
        )),
    };
    let _ = tx.send(msg).await;
}

/// Source-owned PAR/orientation/display dimensions for one exact file. This is
/// deliberately a targeted probe: it opens only the named source and does no
/// directory walk, hash, sidecar inspection, reconciliation or generation work.
async fn probe_geometry_and_send(
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
) {
    let path = match crate::serve::resolve_rel(collections, collection_id, root_token, path_rel) {
        Ok(path) => path,
        Err(e) => {
            let _ = tx
                .send(HostToHub {
                    msg: Some(host_to_hub::Msg::FileVideoGeometry(FileVideoGeometry {
                        collection_id: collection_id.to_string(),
                        source: source(root_token, path_rel),
                        size: 0,
                        geometry_json: String::new(),
                        error: format!("{e:#}"),
                    })),
                })
                .await;
            return;
        }
    };
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let result = tokio::task::spawn_blocking(move || {
        kahawai_media::probe_video_geometry(&path, Duration::from_secs(30))
    })
    .await;
    let (geometry_json, error) = match result {
        Ok(Ok(geometry)) => (
            serde_json::to_string(&geometry).unwrap_or_default(),
            String::new(),
        ),
        Ok(Err(e)) => (String::new(), format!("{e:#}")),
        Err(e) => (String::new(), format!("geometry probe task failed: {e}")),
    };
    let msg = HostToHub {
        msg: Some(host_to_hub::Msg::FileVideoGeometry(FileVideoGeometry {
            collection_id: collection_id.to_string(),
            source: source(root_token, path_rel),
            size,
            geometry_json,
            error,
        })),
    };
    let _ = tx.send(msg).await;
}

/// HUB-32b: one image subtitle track's raw display-set blocks, read
/// through the container's own index. Undecoded on purpose — the
/// payloads are compact this way and the pipeline worker owns the
/// decoders.
async fn extract_image_and_send(
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    sub_index: u32,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
) {
    let started = std::time::Instant::now();
    let (collections2, cid, token, prel) = (
        collections.to_vec(),
        collection_id.to_string(),
        root_token.to_string(),
        path_rel.to_string(),
    );
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let path = crate::serve::resolve_rel(&collections2, &cid, &token, &prel)?;
        // A `.idx` path means a VobSub sidecar pair, not a container:
        // `sub_index` is the track index INSIDE the idx, and the result
        // is shaped exactly like a demuxed S_VOBSUB track (idx text as
        // codec_private), so nothing downstream tells them apart.
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("idx"))
        {
            let idx = std::fs::read_to_string(&path)?;
            let sub = std::fs::read(path.with_extension("sub"))?;
            let blocks = kahawai_media::vobsub_file::extract_track(&idx, &sub, sub_index)?;
            return Ok(Some(kahawai_media::subindex::ImageTrack {
                codec: "S_VOBSUB".into(),
                codec_private: Some(idx.into_bytes()),
                blocks,
            }));
        }
        let mut src = kahawai_media::remux::FileSource::open(&path)?;
        // Local disk: no budget needed, and a header walk is still
        // only a few percent of the file.
        kahawai_media::subindex::extract_image_track(
            &mut src,
            sub_index as usize,
            std::time::Duration::from_secs(120),
        )
    })
    .await;

    let msg = match result {
        Ok(Ok(Some(track))) => {
            tracing::info!(
                collection = collection_id,
                path = path_rel,
                track = sub_index,
                blocks = track.blocks.len(),
                ms = started.elapsed().as_millis(),
                "image display sets extracted"
            );
            // Chunked, because a track is not a message: a PGS stream
            // of a whole film runs to tens of MiB and the largest that
            // ever crossed intact was 63.8 MiB against a 64 MiB limit.
            // Over it the shared link stream resets, taking scans and
            // leases with it.
            let blocks: Vec<kahawai_proto::v1::ImageSubBlock> = track
                .blocks
                .into_iter()
                .map(
                    |(start_ms, dur, payload)| kahawai_proto::v1::ImageSubBlock {
                        start_ms,
                        duration_ms: dur.unwrap_or(0),
                        payload,
                    },
                )
                .collect();
            send_chunked(
                tx,
                collection_id,
                root_token,
                path_rel,
                sub_index,
                track.codec,
                track.codec_private.unwrap_or_default(),
                blocks,
            )
            .await;
            return;
        }
        other => {
            let error = match other {
                Ok(Ok(None)) => "no such image track, or the container has no usable index".into(),
                Ok(Err(e)) => format!("{e:#}"),
                Err(e) => format!("{e}"),
                Ok(Ok(Some(_))) => unreachable!("handled above"),
            };
            tracing::warn!(collection = collection_id, path = path_rel, track = sub_index,
                %error, "image display-set extraction failed");
            kahawai_proto::v1::ImageSubtitles {
                collection_id: collection_id.into(),
                source: source(root_token, path_rel),
                sub_index,
                error,
                // One message, and it is the last one.
                done: Some(true),
                ..Default::default()
            }
        }
    };
    let _ = tx
        .send(HostToHub {
            msg: Some(host_to_hub::Msg::ImageSubtitles(msg)),
        })
        .await;
}

/// Extract every text subtitle track of one local file (single demux
/// pass at disk speed) and ship the results to the hub.
async fn extract_and_send(
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
) {
    let started = std::time::Instant::now();
    let result: anyhow::Result<(u64, Vec<(usize, kahawai_media::subtitles::Extracted)>)> = async {
        let path = crate::serve::resolve_rel(collections, collection_id, root_token, path_rel)?;
        let size = std::fs::metadata(&path)?.len();
        let tracks = tokio::task::spawn_blocking(move || {
            // Sparse first (index-driven reads, no demux); trust it
            // only when it actually produced events — a parser gap
            // must never look like "no subtitles".
            match kahawai_media::subindex::extract_sparse(&path) {
                Ok(Some(tracks)) if tracks.iter().any(|(_, ex)| !ex.cues.is_empty()) => {
                    tracing::debug!(path = %path.display(), "sparse extraction");
                    Ok(tracks)
                }
                _ => {
                    tracing::debug!(path = %path.display(), "sequential extraction");
                    let source = kahawai_media::remux::FileSource::open(&path)?;
                    kahawai_media::subtitles::extract_embedded_all(Box::new(source))
                }
            }
        })
        .await??;
        Ok((size, tracks))
    }
    .await;

    let msg = match result {
        Ok((size, tracks)) => {
            tracing::info!(collection = %collection_id, path = %path_rel,
                tracks = tracks.len(), elapsed = ?started.elapsed(), "subtitles extracted");
            FileSubtitles {
                collection_id: collection_id.to_string(),
                source: source(root_token, path_rel),
                size,
                tracks: tracks
                    .into_iter()
                    .map(|(idx, ex)| SubTrack {
                        key: format!("e{idx}"),
                        ass: ex.ass.unwrap_or_default(),
                        cues_json: serde_json::to_string(&ex.cues).unwrap_or_default(),
                    })
                    .collect(),
                error: String::new(),
            }
        }
        Err(e) => {
            tracing::warn!(collection = %collection_id, path = %path_rel,
                error = format!("{e:#}"), "subtitle extraction failed");
            FileSubtitles {
                collection_id: collection_id.to_string(),
                source: source(root_token, path_rel),
                size: 0,
                tracks: vec![],
                error: format!("{e:#}"),
            }
        }
    };
    let _ = tx
        .send(HostToHub {
            msg: Some(host_to_hub::Msg::FileSubtitles(msg)),
        })
        .await;
}

async fn hash_one(
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    activity: &Activity,
) -> anyhow::Result<FileHash> {
    let path = crate::serve::resolve_rel(collections, collection_id, root_token, path_rel)?;
    let claimed_crc = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(ed2k::filename_crc32);

    let size = std::fs::metadata(&path)?.len();
    let mut file = std::fs::File::open(&path)?;
    let mut ed2k = Ed2k::default();
    let mut crc = crc32fast::Hasher::new();
    let mut remaining = size;

    while remaining > 0 {
        // Yield to scans and playback between chunks (MH-9: low priority).
        while activity.busy() {
            tokio::time::sleep(BUSY_POLL).await;
        }
        let want = remaining.min(CHUNK as u64) as usize;
        let (f, buf) = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; want];
            file.read_exact(&mut buf).map(|_| (file, buf))
        })
        .await??;
        file = f;
        ed2k.update(&buf);
        if claimed_crc.is_some() {
            crc.update(&buf);
        }
        remaining -= want as u64;
        tokio::time::sleep(CHUNK_PACE).await;
    }

    let crc_ok = claimed_crc.map(|want| crc.finalize() == want);
    if crc_ok == Some(false) {
        tracing::warn!(path = %path.display(), "filename CRC32 mismatch — file may be corrupt");
    }
    Ok(FileHash {
        source: None, // caller fills
        ed2k_hex: ed2k.finish(),
        size,
        crc_checked: claimed_crc.is_some(),
        crc_ok: crc_ok.unwrap_or(false),
    })
}

/// How much block payload rides in one message.
///
/// Far below the 64 MiB the link allows, because the limit is a cliff
/// rather than a budget: crossing it resets the shared stream, not just
/// this transfer. 4 MiB also keeps the receiver's buffer small and fits
/// inside the 4 MiB default that any less generous peer would impose.
const SETS_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Send one track's display sets as a run of messages, the last marked
/// done. The header fields ride on every chunk so the receiver can key
/// them without remembering an opening message.
#[allow(clippy::too_many_arguments)] // one complete wire source/track identity
async fn send_chunked(
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    sub_index: u32,
    codec: String,
    codec_private: Vec<u8>,
    blocks: Vec<kahawai_proto::v1::ImageSubBlock>,
) {
    let mut chunk: Vec<kahawai_proto::v1::ImageSubBlock> = Vec::new();
    let mut bytes = 0usize;
    let mut sent = 0usize;
    let total = blocks.len();
    let mut iter = blocks.into_iter().peekable();
    while let Some(b) = iter.next() {
        bytes += b.payload.len();
        chunk.push(b);
        let last = iter.peek().is_none();
        if bytes < SETS_CHUNK_BYTES && !last {
            continue;
        }
        sent += chunk.len();
        let msg = kahawai_proto::v1::ImageSubtitles {
            collection_id: collection_id.into(),
            source: source(root_token, path_rel),
            sub_index,
            codec: codec.clone(),
            codec_private: codec_private.clone(),
            blocks: std::mem::take(&mut chunk),
            error: String::new(),
            done: Some(last),
        };
        bytes = 0;
        if tx
            .send(HostToHub {
                msg: Some(host_to_hub::Msg::ImageSubtitles(msg)),
            })
            .await
            .is_err()
        {
            tracing::warn!(
                collection = collection_id,
                path = path_rel,
                track = sub_index,
                sent,
                total,
                "link closed mid-transfer; display sets abandoned"
            );
            return;
        }
    }
    // A track with no blocks at all still needs its one message, or the
    // hub waits for a transfer that never starts.
    if total == 0 {
        let _ = tx
            .send(HostToHub {
                msg: Some(host_to_hub::Msg::ImageSubtitles(
                    kahawai_proto::v1::ImageSubtitles {
                        collection_id: collection_id.into(),
                        source: source(root_token, path_rel),
                        sub_index,
                        codec,
                        codec_private,
                        done: Some(true),
                        ..Default::default()
                    },
                )),
            })
            .await;
    }
}
