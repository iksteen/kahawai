//! The idle ED2K worker (MH-9): consumes hub Hashlists, hashes one file at
//! a time, and yields to real work — it only reads while no scan is
//! running and no lease is being served, checked between every chunk.

use std::collections::{HashSet, VecDeque};
use std::io::Read;
use std::time::Duration;

use kahawai_proto::v1::{host_to_hub, FileHash, FileHashes, Hashlist, HostToHub};

use crate::ed2k::{self, Ed2k, CHUNK};
use crate::scan::CollectionConfig;
use crate::Activity;

/// Pause between chunks even when idle: bounds the read rate (~95 MB/s)
/// so the hasher never monopolizes the disk it shares with everything else.
const CHUNK_PACE: Duration = Duration::from_millis(100);
const BUSY_POLL: Duration = Duration::from_secs(2);

pub async fn run(
    mut rx: tokio::sync::mpsc::Receiver<Hashlist>,
    tx: tokio::sync::mpsc::Sender<HostToHub>,
    collections: Vec<CollectionConfig>,
    activity: Activity,
) {
    let mut queue: VecDeque<(String, String)> = VecDeque::new();
    let mut queued: HashSet<(String, String)> = HashSet::new();

    loop {
        // Drain new work; block only when the queue is empty.
        loop {
            let msg = if queue.is_empty() {
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
            for path in msg.paths {
                let key = (msg.collection_id.clone(), path);
                if queued.insert(key.clone()) {
                    queue.push_back(key);
                }
            }
        }

        let Some((collection_id, path_rel)) = queue.pop_front() else { continue };
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
            // Vanished or unreadable: the next scan reconciles; not our job.
            Err(e) => tracing::debug!(collection = %collection_id, path = %path_rel,
                error = format!("{e:#}"), "ed2k skipped"),
        }
        queued.remove(&(collection_id, path_rel));
    }
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
