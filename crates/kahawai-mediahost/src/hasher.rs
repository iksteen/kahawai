//! The background job worker: ED2K hashing (MH-9) and subtitle
//! extraction (efficiency ladder step 2), one file at a time, in three
//! tiers — urgent subtitle jobs (a viewer waits: run immediately),
//! ED2K (idle-gated), background subtitle pre-warm (idle-gated,
//! drained only when the ED2K queue is empty).

use std::collections::{HashSet, VecDeque};
use std::io::Read;
use std::time::Duration;

use kahawai_proto::v1::{
    host_to_hub, ExtractSubs, FileHash, FileHashes, FileSubtitles, Hashlist, HostToHub,
    SubTrack, SubsWorklist,
};

use crate::ed2k::{self, Ed2k, CHUNK};
use crate::scan::CollectionConfig;
use crate::Activity;

/// Pause between chunks even when idle: bounds the read rate (~95 MB/s)
/// so the hasher never monopolizes the disk it shares with everything else.
const CHUNK_PACE: Duration = Duration::from_millis(100);
const BUSY_POLL: Duration = Duration::from_secs(2);

/// Work arriving from the hub via the link dispatch loop.
pub enum JobMsg {
    Hashlist(Hashlist),
    SubsWorklist(SubsWorklist),
    Urgent(ExtractSubs),
}

pub async fn run(
    mut rx: tokio::sync::mpsc::Receiver<JobMsg>,
    tx: tokio::sync::mpsc::Sender<HostToHub>,
    collections: Vec<CollectionConfig>,
    activity: Activity,
) {
    let mut urgent: VecDeque<(String, String)> = VecDeque::new();
    let mut ed2k_q: VecDeque<(String, String)> = VecDeque::new();
    let mut subs_q: VecDeque<(String, String)> = VecDeque::new();
    let mut ed2k_seen: HashSet<(String, String)> = HashSet::new();
    let mut subs_seen: HashSet<(String, String)> = HashSet::new();

    loop {
        // Drain new work; block only when every queue is empty.
        loop {
            let empty = urgent.is_empty() && ed2k_q.is_empty() && subs_q.is_empty();
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
            match msg {
                JobMsg::Urgent(e) => urgent.push_back((e.collection_id, e.path_rel)),
                JobMsg::Hashlist(h) => {
                    for path in h.paths {
                        let key = (h.collection_id.clone(), path);
                        if ed2k_seen.insert(key.clone()) {
                            ed2k_q.push_back(key);
                        }
                    }
                }
                JobMsg::SubsWorklist(w) => {
                    for path in w.paths {
                        let key = (w.collection_id.clone(), path);
                        if subs_seen.insert(key.clone()) {
                            subs_q.push_back(key);
                        }
                    }
                }
            }
        }

        // Tier order: urgent (never idle-gated — the active lease IS the
        // requesting viewer) → ED2K → background subs.
        if let Some((collection_id, path_rel)) = urgent.pop_front() {
            extract_and_send(&collections, &collection_id, &path_rel, &tx).await;
            continue;
        }
        if let Some((collection_id, path_rel)) = ed2k_q.pop_front() {
            match hash_one(&collections, &collection_id, &path_rel, &activity).await {
                Ok(mut fh) => {
                    fh.path_rel = path_rel.clone();
                    tracing::info!(collection = %collection_id, path = %fh.path_rel,
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
            ed2k_seen.remove(&(collection_id, path_rel));
            continue;
        }
        if let Some((collection_id, path_rel)) = subs_q.pop_front() {
            // Idle gate before starting; extraction itself is a bounded
            // local read (ponytail: can't pause a gst pipeline mid-walk).
            while activity.busy() {
                match tokio::time::timeout(BUSY_POLL, rx.recv()).await {
                    Ok(Some(m)) => {
                        // Urgent work preempts the wait.
                        let requeue = matches!(&m, JobMsg::Urgent(_));
                        match m {
                            JobMsg::Urgent(e) => urgent.push_back((e.collection_id, e.path_rel)),
                            other => {
                                // Defer non-urgent intake to the drain loop.
                                match other {
                                    JobMsg::Hashlist(h) => {
                                        for path in h.paths {
                                            let key = (h.collection_id.clone(), path);
                                            if ed2k_seen.insert(key.clone()) {
                                                ed2k_q.push_back(key);
                                            }
                                        }
                                    }
                                    JobMsg::SubsWorklist(w) => {
                                        for path in w.paths {
                                            let key = (w.collection_id.clone(), path);
                                            if subs_seen.insert(key.clone()) {
                                                subs_q.push_back(key);
                                            }
                                        }
                                    }
                                    JobMsg::Urgent(_) => unreachable!(),
                                }
                            }
                        }
                        if requeue {
                            break;
                        }
                    }
                    Ok(None) => return,
                    Err(_) => {} // still busy; keep waiting
                }
            }
            if !urgent.is_empty() {
                // Preempted: put the background job back and loop.
                subs_seen.remove(&(collection_id.clone(), path_rel.clone()));
                let key = (collection_id, path_rel);
                if subs_seen.insert(key.clone()) {
                    subs_q.push_front(key);
                }
                continue;
            }
            extract_and_send(&collections, &collection_id, &path_rel, &tx).await;
            subs_seen.remove(&(collection_id, path_rel));
        }
    }
}

/// Extract every text subtitle track of one local file (single demux
/// pass at disk speed) and ship the results to the hub.
async fn extract_and_send(
    collections: &[CollectionConfig],
    collection_id: &str,
    path_rel: &str,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
) {
    let started = std::time::Instant::now();
    let result: anyhow::Result<(u64, Vec<(usize, kahawai_media::subtitles::Extracted)>)> =
        async {
            let path = crate::serve::resolve_rel(collections, collection_id, path_rel)?;
            let size = std::fs::metadata(&path)?.len();
            let source = kahawai_media::remux::FileSource::open(&path)?;
            let tracks = tokio::task::spawn_blocking(move || {
                kahawai_media::subtitles::extract_embedded_all(Box::new(source))
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
                path_rel: path_rel.to_string(),
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
                path_rel: path_rel.to_string(),
                size: 0,
                tracks: vec![],
                error: format!("{e:#}"),
            }
        }
    };
    let _ = tx
        .send(HostToHub { msg: Some(host_to_hub::Msg::FileSubtitles(msg)) })
        .await;
}

async fn hash_one(
    collections: &[CollectionConfig],
    collection_id: &str,
    path_rel: &str,
    activity: &Activity,
) -> anyhow::Result<FileHash> {
    let path = crate::serve::resolve_rel(collections, collection_id, path_rel)?;
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
        path_rel: String::new(), // caller fills
        ed2k_hex: ed2k.finish(),
        size,
        crc_checked: claimed_crc.is_some(),
        crc_ok: crc_ok.unwrap_or(false),
    })
}
