//! Collection scanner (MH-2/3/5/8, minimal first cut): walk roots, discover
//! technical metadata, compute content identity + oshash in one read pass,
//! and push FileUpsert batches up the link.
//!
//! ponytail: full rescan on every (re)connect; the journaled resumable scan
//! + fs watcher (MH-2/MH-7) land when libraries get big enough to hurt.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use kahawai_proto::v1::{host_to_hub, FileError, FileRecord, FileUpsert, HostToHub, ScanProgress};
use serde::Deserialize;
use tokio::sync::mpsc::Sender;

#[derive(Debug, Clone, Deserialize)]
pub struct CollectionConfig {
    pub name: String,
    pub media_type: String,
    pub roots: Vec<PathBuf>,
}

const AUDIO_EXTS: &[&str] = &["flac", "mp3", "m4a", "ogg", "opus", "wav", "aac", "wma"];

const MEDIA_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "webm", "avi", "mov", "ts", "m2ts", "ogv", // video
    "flac", "mp3", "ogg", "oga", "opus", "m4a", "aac", "wav", // audio
];
const BATCH: usize = 32;
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(30);

/// Scan one collection, sending batches over the link. Errors only when the
/// link is gone; per-file failures become FileError messages (MH-8).
pub async fn scan_collection(
    cfg: CollectionConfig,
    tx: Sender<HostToHub>,
    mut manifest: tokio::sync::mpsc::Receiver<kahawai_proto::v1::Manifest>,
    force_dirs: std::collections::HashSet<std::path::PathBuf>,
) -> Result<()> {
    let (mut scanned, mut failed, mut skipped) = (0u32, 0u32, 0u32);
    let mut batch: Vec<FileRecord> = Vec::with_capacity(BATCH);

    // Collect the hub's manifest (chunked); an old hub never answers, so
    // a timeout degrades to the full rescan.
    let mut known: std::collections::HashMap<String, (u64, i64)> = Default::default();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, manifest.recv()).await {
            Ok(Some(m)) => {
                known.extend(m.entries.into_iter().map(|e| (e.path_rel, (e.size, e.mtime_unix))));
                if m.done {
                    break;
                }
            }
            _ => {
                known.clear(); // partial manifest is worse than none
                break;
            }
        }
    }
    let mut seen_batch: Vec<String> = Vec::new();

    let include_audio = cfg.media_type == "music";
    for root in &cfg.roots {
        let root = root.clone();
        let paths =
            tokio::task::spawn_blocking(move || walk(&root, include_audio)).await??;
        for (root_local, path) in paths {
            let rel = path
                .strip_prefix(&root_local)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            // Unchanged since the hub last saw it? Seen, not re-inspected.
            // force_dirs overrides: a sidecar/artwork change in the dir
            // must re-inspect its media files despite matching stats.
            if !path.parent().is_some_and(|p| force_dirs.contains(p))
                && let Some(&(size, mtime)) = known.get(&rel)
                && std::fs::metadata(&path).is_ok_and(|m| {
                    m.len() == size
                        && m.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            == Some(mtime)
                })
            {
                skipped += 1;
                seen_batch.push(rel);
                if seen_batch.len() >= 1000 {
                    send_seen(&tx, &cfg.name, std::mem::take(&mut seen_batch)).await?;
                }
                continue;
            }
            let (r, p) = (root_local.clone(), path.clone());
            let record =
                tokio::task::spawn_blocking(move || inspect(&r, &p)).await?;
            match record {
                Ok((size, mtime_unix, head_xxh3, tail_xxh3, oshash, info)) => {
                    scanned += 1;
                    batch.push(FileRecord {
                        path_rel: rel,
                        size,
                        mtime_unix,
                        head_xxh3,
                        tail_xxh3,
                        oshash,
                        streams_json: serde_json::to_string(&info)?,
                    });
                    if batch.len() >= BATCH {
                        send_upsert(&tx, &cfg.name, std::mem::take(&mut batch)).await?;
                    }
                }
                Err(e) => {
                    failed += 1;
                    tracing::warn!(path = %path.display(), error = format!("{e:#}"), "scan failed");
                    tx.send(HostToHub {
                        msg: Some(host_to_hub::Msg::FileError(FileError {
                            collection_id: cfg.name.clone(),
                            path_rel: rel,
                            error: format!("{e:#}"),
                        })),
                    })
                    .await
                    .context("link closed")?;
                }
            }
        }
    }
    if !batch.is_empty() {
        send_upsert(&tx, &cfg.name, batch).await?;
    }
    if !seen_batch.is_empty() {
        send_seen(&tx, &cfg.name, seen_batch).await?;
    }
    tx.send(HostToHub {
        msg: Some(host_to_hub::Msg::ScanProgress(ScanProgress {
            collection_id: cfg.name.clone(),
            scanned,
            failed,
            complete: true,
            skipped,
        })),
    })
    .await
    .context("link closed")?;
    tracing::info!(collection = %cfg.name, scanned, failed, skipped, "scan complete");
    Ok(())
}

async fn send_seen(tx: &Sender<HostToHub>, collection: &str, paths: Vec<String>) -> Result<()> {
    tx.send(HostToHub {
        msg: Some(host_to_hub::Msg::FilesSeen(kahawai_proto::v1::FilesSeen {
            collection_id: collection.to_string(),
            path_rel: paths,
        })),
    })
    .await
    .context("link closed")
}

async fn send_upsert(tx: &Sender<HostToHub>, collection: &str, files: Vec<FileRecord>) -> Result<()> {
    tx.send(HostToHub {
        msg: Some(host_to_hub::Msg::FileUpsert(FileUpsert {
            collection_id: collection.to_string(),
            files,
        })),
    })
    .await
    .context("link closed")
}

/// Media files under `root`, sorted for deterministic batches.
fn walk(root: &Path, include_audio: bool) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let is_media = entry.path().extension().and_then(|e| e.to_str()).is_some_and(|e| {
            let e = e.to_ascii_lowercase();
            MEDIA_EXTS.contains(&e.as_str())
                || (include_audio && AUDIO_EXTS.contains(&e.as_str()))
        });
        if is_media {
            out.push((root.to_path_buf(), entry.into_path()));
        }
    }
    out.sort();
    Ok(out)
}

type Inspected = (u64, i64, u64, u64, u64, kahawai_core::media::MediaInfo);

/// One stat + one head/tail read pass + GStreamer discovery + sidecars.
fn inspect(root: &Path, path: &Path) -> Result<Inspected> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len();
    let mtime_unix = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (head_xxh3, tail_xxh3, oshash) = identity_hashes(path, size)?;
    let mut info = kahawai_media::discover(path, DISCOVER_TIMEOUT)?;
    info.external_subtitles = find_sidecars(root, path);
    info.artwork = find_artwork(root, path);
    Ok((size, mtime_unix, head_xxh3, tail_xxh3, oshash, info))
}

/// Local artwork (MH-4): a cover image in the media file's directory.
/// Names in preference order; first hit wins.
fn find_artwork(root: &Path, media: &Path) -> Option<String> {
    const NAMES: &[&str] = &["cover", "folder", "poster", "front"];
    const EXTS: &[&str] = &["jpg", "jpeg", "png", "webp"];
    let dir = media.parent()?;
    let entries: Vec<PathBuf> =
        std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
    for name in NAMES {
        for p in &entries {
            let stem_ok = p
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case(name));
            let ext_ok = p
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| EXTS.contains(&x.to_ascii_lowercase().as_str()));
            if stem_ok && ext_ok {
                return Some(p.strip_prefix(root).unwrap_or(p).to_string_lossy().into_owned());
            }
        }
    }
    None
}

const SUBTITLE_EXTS: &[(&str, &str)] = &[("srt", "srt"), ("ass", "ass"), ("ssa", "ass"), ("vtt", "vtt")];

/// Sidecar subtitles (MH-4): files in the media file's directory named
/// `<stem>.<ext>` or `<stem>.<tokens>.<ext>`; the first token after the
/// stem is recorded verbatim as the language ("Movie.en.srt" → "en").
fn find_sidecars(root: &Path, media: &Path) -> Vec<kahawai_core::media::SidecarSubtitle> {
    let mut out = Vec::new();
    let (Some(stem), Some(dir)) =
        (media.file_stem().and_then(|s| s.to_str()), media.parent())
    else {
        return out;
    };
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let p = entry.path();
        let (Some(name), Some(ext)) = (
            p.file_name().and_then(|n| n.to_str()),
            p.extension().and_then(|x| x.to_str()),
        ) else {
            continue;
        };
        let Some((_, format)) =
            SUBTITLE_EXTS.iter().find(|(e, _)| e.eq_ignore_ascii_case(ext))
        else {
            continue;
        };
        if !(name.len() > stem.len()
            && name.starts_with(stem)
            && name.as_bytes()[stem.len()] == b'.')
        {
            continue;
        }
        let middle_end = name.len() - ext.len() - 1;
        let middle = if stem.len() + 1 < middle_end { &name[stem.len() + 1..middle_end] } else { "" };
        let language = middle
            .split('.')
            .next()
            .filter(|t| !t.is_empty() && t.len() <= 10)
            .map(|t| t.to_lowercase());
        out.push(kahawai_core::media::SidecarSubtitle {
            path_rel: p.strip_prefix(root).unwrap_or(&p).to_string_lossy().into_owned(),
            format: format.to_string(),
            language,
        });
    }
    out.sort_by(|a, b| a.path_rel.cmp(&b.path_rel));
    out
}

const CHUNK: u64 = 64 * 1024;

/// Content identity (MH-5) and OpenSubtitles moviehash in the same pass:
/// xxh3 of the first and last 64 KiB, and `size + Σ u64_le(first 64 KiB)
/// + Σ u64_le(last 64 KiB)` (wrapping).
pub fn identity_hashes(path: &Path, size: u64) -> std::io::Result<(u64, u64, u64)> {
    let mut f = std::fs::File::open(path)?;
    let mut head = vec![0u8; size.min(CHUNK) as usize];
    f.read_exact(&mut head)?;
    let mut tail = vec![0u8; size.min(CHUNK) as usize];
    f.seek(SeekFrom::Start(size.saturating_sub(CHUNK)))?;
    f.read_exact(&mut tail)?;

    let head_xxh3 = xxhash_rust::xxh3::xxh3_64(&head);
    let tail_xxh3 = xxhash_rust::xxh3::xxh3_64(&tail);
    let oshash = size
        .wrapping_add(sum_u64_le(&head))
        .wrapping_add(sum_u64_le(&tail));
    Ok((head_xxh3, tail_xxh3, oshash))
}

fn sum_u64_le(bytes: &[u8]) -> u64 {
    bytes.chunks(8).fold(0u64, |acc, c| {
        let mut buf = [0u8; 8];
        buf[..c.len()].copy_from_slice(c);
        acc.wrapping_add(u64::from_le_bytes(buf))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oshash_of_zeros_is_the_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zeros.bin");
        std::fs::write(&path, vec![0u8; 128 * 1024]).unwrap();
        let (_, _, oshash) = identity_hashes(&path, 128 * 1024).unwrap();
        assert_eq!(oshash, 128 * 1024);
    }

    #[test]
    fn identity_tracks_content_not_name() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        let b = dir.path().join("b.bin");
        std::fs::write(&a, b"same content, different name").unwrap();
        std::fs::write(&b, b"same content, different name").unwrap();
        let ha = identity_hashes(&a, 28).unwrap();
        let hb = identity_hashes(&b, 28).unwrap();
        assert_eq!(ha, hb);

        std::fs::write(&b, b"other content, different name").unwrap();
        let hb2 = identity_hashes(&b, 29).unwrap();
        assert_ne!(ha, hb2);
    }

    #[test]
    fn sidecars_matched_by_stem() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("m")).unwrap();
        for f in [
            "m/Heat (1995).mkv",
            "m/Heat (1995).srt",
            "m/Heat (1995).en.srt",
            "m/Heat (1995).nl.forced.ass",
            "m/Heat (1995).vtt",
            "m/Heat (1995) extras.srt", // no dot boundary → not a sidecar
            "m/Other.srt",
        ] {
            std::fs::write(root.join(f), b"x").unwrap();
        }
        let subs = find_sidecars(root, &root.join("m/Heat (1995).mkv"));
        let got: Vec<(&str, &str, Option<&str>)> = subs
            .iter()
            .map(|s| (s.path_rel.as_str(), s.format.as_str(), s.language.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("m/Heat (1995).en.srt", "srt", Some("en")),
                ("m/Heat (1995).nl.forced.ass", "ass", Some("nl")),
                ("m/Heat (1995).srt", "srt", None),
                ("m/Heat (1995).vtt", "vtt", None),
            ]
        );
    }

    #[test]
    fn walk_filters_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("b.mkv"), b"x").unwrap();
        std::fs::write(dir.path().join("sub/a.mp4"), b"x").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("cover.jpg"), b"x").unwrap();
        let files = walk(dir.path(), false).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|(r, p)| p.strip_prefix(r).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["b.mkv", "sub/a.mp4"]);
    }
}
