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
use kahawai_proto::v1::{FileError, FileRecord, FileUpsert, HostToHub, ScanProgress, host_to_hub};
use tokio::sync::mpsc::Sender;

pub use kahawai_core::media::CollectionConfig;

const AUDIO_EXTS: &[&str] = &["flac", "mp3", "m4a", "ogg", "opus", "wav", "aac", "wma"];

const MEDIA_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "webm", "avi", "mov", "ts", "m2ts", "ogv", // video
    "flac", "mp3", "ogg", "oga", "opus", "m4a", "aac", "wav", // audio
];
const BATCH: usize = 32;
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanOutcome {
    Completed,
    InSync,
    RootAdoptionAcknowledged,
}

/// Scan one collection, sending batches over the link. Errors only when the
/// link is gone; per-file failures become FileError messages (MH-8).
pub(crate) async fn scan_collection(
    cfg: CollectionConfig,
    tx: Sender<HostToHub>,
    mut manifest: tokio::sync::mpsc::Receiver<kahawai_proto::v1::Manifest>,
    force_dirs: std::collections::HashSet<std::path::PathBuf>,
    sync_version: u64,
) -> Result<ScanOutcome> {
    let (mut scanned, mut failed, mut skipped) = (0u32, 0u32, 0u32);
    // Live progress (HUB-35/HUB-26): a start beacon (after the in-sync
    // gate — a skipped scan must not leave a stale "scanning" state),
    // then interim reports for the admin UI.
    let mut last_reported = 0u32;
    let mut batch: Vec<FileRecord> = Vec::with_capacity(BATCH);

    // Collect the protocol-3 manifest (chunked). Hub slowness is not
    // permission to scan: root adoption can legitimately hold SQLite while it
    // rewrites a large legacy collection, and converting that latency into a
    // full scan defeats the lossless upgrade. Link teardown drops the engine
    // and this sender, so a closed channel still fails the cycle promptly.
    let mut known: std::collections::HashMap<(String, String), (u64, i64, String)> =
        Default::default();
    let mut compare_sidecars = false;
    loop {
        let m = manifest
            .recv()
            .await
            .context("link closed before manifest completed")?;
        if m.in_sync {
            if m.root_adoption {
                tx.send(HostToHub {
                    msg: Some(host_to_hub::Msg::RootAdoptionAck(
                        kahawai_proto::v1::RootAdoptionAck {
                            collection_id: cfg.name.clone(),
                        },
                    )),
                })
                .await
                .context("link closed before root adoption acknowledgement")?;
                tracing::info!(collection = %cfg.name,
                    "root adoption acknowledged; retrying deferred scan");
                return Ok(ScanOutcome::RootAdoptionAcknowledged);
            }
            tracing::info!(collection = %cfg.name, "in sync with hub; scan skipped");
            return Ok(ScanOutcome::InSync);
        }
        compare_sidecars |= m.sidecars_compared;
        for e in m.entries {
            let source = e.source.context("manifest entry missing exact source")?;
            known.insert(
                (source.root_token, source.path_rel),
                (e.size, e.mtime_unix, e.sidecars),
            );
        }
        if m.done {
            break;
        }
    }
    tx.send(HostToHub {
        msg: Some(host_to_hub::Msg::ScanProgress(ScanProgress {
            collection_id: cfg.name.clone(),
            scanned: 0,
            failed: 0,
            complete: false,
            skipped: 0,
            sync_version: 0,
        })),
    })
    .await
    .context("link closed")?;
    let mut seen_batch: Vec<kahawai_proto::v1::SourcePath> = Vec::new();

    let include_audio = cfg.media_type == "music";
    let force = std::sync::Arc::new(force_dirs);
    for configured_root in cfg.resolved_roots() {
        let root_token = configured_root.token;
        let root = configured_root.path;
        let paths = match tokio::task::spawn_blocking(move || walk(&root, include_audio)).await? {
            Ok(paths) => paths,
            Err(error) => {
                // One unavailable mount must not make another root stand in
                // for it, nor may reconciliation interpret its whole manifest
                // as deleted. Report the exact root and carry its prior rows
                // forward as seen; later sweeps retry it independently.
                failed += 1;
                tracing::warn!(collection = %cfg.name, %root_token,
                    error = format!("{error:#}"), "collection root unavailable");
                tx.send(HostToHub {
                    msg: Some(host_to_hub::Msg::FileError(FileError {
                        collection_id: cfg.name.clone(),
                        source: Some(kahawai_proto::v1::SourcePath {
                            root_token: root_token.clone(),
                            path_rel: String::new(),
                        }),
                        error: format!("root unavailable: {error:#}"),
                    })),
                })
                .await
                .context("link closed")?;
                seen_batch.extend(known.keys().filter(|(token, _)| token == &root_token).map(
                    |(token, path_rel)| kahawai_proto::v1::SourcePath {
                        root_token: token.clone(),
                        path_rel: path_rel.clone(),
                    },
                ));
                if seen_batch.len() >= 1000 {
                    send_seen(&tx, &cfg.name, std::mem::take(&mut seen_batch)).await?;
                }
                continue;
            }
        };
        // Stat in batches ON THE BLOCKING POOL: a network mount makes
        // each stat a round trip, and doing tens of thousands of them
        // inline starved async peers of this task for tens of seconds.
        // Each batch yields (rel, unchanged) verdicts.
        for stat_batch in paths.chunks(250).map(<[_]>::to_vec) {
            let known2 = known.clone();
            let root_token2 = root_token.clone();
            let force2 = force.clone();
            let compare2 = compare_sidecars;
            let verdicts: Vec<((std::path::PathBuf, std::path::PathBuf), String, bool)> =
                tokio::task::spawn_blocking(move || {
                    stat_batch
                        .into_iter()
                        .map(|(root_local, path)| {
                            let rel = path
                                .strip_prefix(&root_local)
                                .unwrap_or(&path)
                                .to_string_lossy()
                                .into_owned();
                            let unchanged = !path.parent().is_some_and(|p| force2.contains(p))
                                && known2.get(&(root_token2.clone(), rel.clone())).is_some_and(
                                    |(size, mtime, sidecars)| {
                                        let stat_matches =
                                            std::fs::metadata(&path).is_ok_and(|m| {
                                                m.len() == *size
                                                    && m.modified()
                                                        .ok()
                                                        .and_then(|t| {
                                                            t.duration_since(std::time::UNIX_EPOCH)
                                                                .ok()
                                                        })
                                                        .map(|d| d.as_secs() as i64)
                                                        == Some(*mtime)
                                            });
                                        // Size and mtime describe the MEDIA file and do
                                        // not move when a .nfo or a cover appears beside
                                        // it, so a sidecar dropped in next to an
                                        // already-scanned file used to stay invisible
                                        // until the file itself changed. Costs a few
                                        // stats per file, and only where the hub asked.
                                        stat_matches
                                            && (!compare2
                                                || &sidecar_sig(&root_local, &path) == sidecars)
                                    },
                                );
                            ((root_local, path), rel, unchanged)
                        })
                        .collect()
                })
                .await?;
            for ((root_local, path), rel, unchanged) in verdicts {
                if unchanged {
                    skipped += 1;
                    seen_batch.push(kahawai_proto::v1::SourcePath {
                        root_token: root_token.clone(),
                        path_rel: rel,
                    });
                    if seen_batch.len() >= 1000 {
                        send_seen(&tx, &cfg.name, std::mem::take(&mut seen_batch)).await?;
                    }
                    continue;
                }
                let (r, p) = (root_local.clone(), path.clone());
                let record = tokio::task::spawn_blocking(move || inspect(&r, &p)).await?;
                match record {
                    Ok((size, mtime_unix, head_xxh3, tail_xxh3, oshash, info)) => {
                        scanned += 1;
                        batch.push(FileRecord {
                            source: Some(kahawai_proto::v1::SourcePath {
                                root_token: root_token.clone(),
                                path_rel: rel,
                            }),
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
                                source: Some(kahawai_proto::v1::SourcePath {
                                    root_token: root_token.clone(),
                                    path_rel: rel,
                                }),
                                error: format!("{e:#}"),
                            })),
                        })
                        .await
                        .context("link closed")?;
                    }
                }
            }
            // Interim progress once per stat batch, counting skips too —
            // an incremental rescan of an unchanged collection is mostly
            // skips, and those must move the admin UI as well.
            let processed = scanned + failed + skipped;
            if processed - last_reported >= 500 {
                last_reported = processed;
                tx.send(HostToHub {
                    msg: Some(host_to_hub::Msg::ScanProgress(ScanProgress {
                        collection_id: cfg.name.clone(),
                        scanned,
                        failed,
                        complete: false,
                        skipped,
                        sync_version: 0,
                    })),
                })
                .await
                .context("link closed")?;
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
            sync_version,
        })),
    })
    .await
    .context("link closed")?;
    tracing::info!(collection = %cfg.name, scanned, failed, skipped, "scan complete");
    Ok(ScanOutcome::Completed)
}

async fn send_seen(
    tx: &Sender<HostToHub>,
    collection: &str,
    sources: Vec<kahawai_proto::v1::SourcePath>,
) -> Result<()> {
    tx.send(HostToHub {
        msg: Some(host_to_hub::Msg::FilesSeen(kahawai_proto::v1::FilesSeen {
            collection_id: collection.to_string(),
            sources,
        })),
    })
    .await
    .context("link closed")
}

async fn send_upsert(
    tx: &Sender<HostToHub>,
    collection: &str,
    files: Vec<FileRecord>,
) -> Result<()> {
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
        let is_media = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
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
    info.nfo = find_nfo(root, path);
    // The longest keyframe gap, from the container index — kilobytes of
    // reads, and the only honest bound on a copy session's segment
    // length (and so on EXT-X-TARGETDURATION). Failure is not fatal:
    // the field stays None and callers treat that as "could be long".
    if let Some(v) = info.video.first_mut() {
        match kahawai_media::subindex::max_keyframe_interval_ms(path) {
            Ok(ms) => v.max_keyframe_interval_ms = ms,
            Err(e) => tracing::debug!(
                path = %path.display(),
                error = format!("{e:#}"),
                "keyframe interval probe failed"
            ),
        }
    }
    // MH-4: declare embedded attachments (fonts) — name/mime/byte range
    // only, payloads are never read at scan time.
    if matches!(info.container.as_deref(), Some("matroska" | "webm")) {
        match kahawai_media::subindex::declare_attachments(path) {
            Ok(atts) => info.attachments = Some(atts),
            Err(e) => tracing::debug!(
                path = %path.display(),
                error = format!("{e:#}"),
                "attachment declaration failed"
            ),
        }
    }
    Ok((size, mtime_unix, head_xxh3, tail_xxh3, oshash, info))
}

/// Local artwork (MH-4): a cover image in the media file's directory.
/// Names in preference order; first hit wins.
/// The sidecars visible beside `media` right now, in the hub's spelling
/// (`kahawai_hub::registry::sidecar_sig`). The two must agree verbatim:
/// this is compared as a string, not parsed.
fn sidecar_sig(root: &Path, media: &Path) -> String {
    let nfo = find_nfo(root, media).unwrap_or_default();
    let art = find_artwork(root, media).unwrap_or_default();
    // Subtitle sidecars too (.srt/.ass/.vtt, .idx pairs): a pair
    // appearing next to an UNCHANGED movie must bust the identity
    // fast-path, or it stays invisible until the movie changes.
    let mut subs: Vec<String> = find_sidecars(root, media)
        .into_iter()
        .map(|s| s.path_rel)
        .collect();
    subs.sort();
    subs.dedup();
    if nfo.is_empty() && art.is_empty() && subs.is_empty() {
        return String::new();
    }
    format!("n:{nfo}|a:{art}|s:{}", subs.join(","))
}

fn find_artwork(root: &Path, media: &Path) -> Option<String> {
    const NAMES: &[&str] = &["cover", "folder", "poster", "front"];
    const EXTS: &[&str] = &["jpg", "jpeg", "png", "webp"];
    let dir = media.parent()?;
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
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
                return Some(
                    p.strip_prefix(root)
                        .unwrap_or(p)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    None
}

/// A Kodi-style .nfo for this file (HUB-9): `<stem>.nfo` beside it, else
/// the directory's `movie.nfo`/`tvshow.nfo`. Only the path is recorded —
/// the hub reads and parses it, because that is where the provider chain
/// lives and the file is tiny.
fn find_nfo(root: &Path, media: &Path) -> Option<String> {
    let dir = media.parent()?;
    let rel = |p: &Path| {
        p.strip_prefix(root)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned()
    };
    let beside = media.with_extension("nfo");
    if beside.is_file() {
        return Some(rel(&beside));
    }
    for name in ["movie.nfo", "tvshow.nfo"] {
        let p = dir.join(name);
        if p.is_file() {
            return Some(rel(&p));
        }
    }
    None
}

const SUBTITLE_EXTS: &[(&str, &str)] = &[
    ("srt", "srt"),
    ("ass", "ass"),
    ("ssa", "ass"),
    ("vtt", "vtt"),
];

/// Sidecar subtitles (MH-4): files in the media file's directory named
/// `<stem>.<ext>` or `<stem>.<tokens>.<ext>`; the first token after the
/// stem is recorded verbatim as the language ("Movie.en.srt" → "en").
fn find_sidecars(root: &Path, media: &Path) -> Vec<kahawai_core::media::SidecarSubtitle> {
    let mut out = Vec::new();
    let (Some(stem), Some(dir)) = (media.file_stem().and_then(|s| s.to_str()), media.parent())
    else {
        return out;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let (Some(name), Some(ext)) = (
            p.file_name().and_then(|n| n.to_str()),
            p.extension().and_then(|x| x.to_str()),
        ) else {
            continue;
        };
        let Some((_, format)) = SUBTITLE_EXTS
            .iter()
            .find(|(e, _)| e.eq_ignore_ascii_case(ext))
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
        let middle = if stem.len() + 1 < middle_end {
            &name[stem.len() + 1..middle_end]
        } else {
            ""
        };
        let language = middle
            .split('.')
            .next()
            .filter(|t| !t.is_empty() && t.len() <= 10)
            .map(|t| t.to_lowercase());
        out.push(kahawai_core::media::SidecarSubtitle {
            path_rel: p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .into_owned(),
            format: format.to_string(),
            language,
            track: None,
        });
    }
    // VobSub pairs: `<stem>.idx` + `<stem>.sub` — image subtitles, one
    // entry per track inside the .idx (a single pair commonly carries
    // several languages). The .idx is small text; reading it at scan is
    // how the languages become known at all — the filename says nothing.
    let idx = dir.join(format!("{stem}.idx"));
    if idx.is_file()
        && dir.join(format!("{stem}.sub")).is_file()
        && let Ok(text) = std::fs::read_to_string(&idx)
    {
        let path_rel = idx
            .strip_prefix(root)
            .unwrap_or(&idx)
            .to_string_lossy()
            .into_owned();
        for t in kahawai_media::vobsub_file::parse_idx(&text) {
            out.push(kahawai_core::media::SidecarSubtitle {
                path_rel: path_rel.clone(),
                format: "vobsub".into(),
                language: t.language,
                track: Some(t.id),
            });
        }
    }
    out.sort_by(|a, b| (&a.path_rel, a.track).cmp(&(&b.path_rel, b.track)));
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

    /// The two sides compare this as a STRING, so they must spell it the
    /// same way. A silent divergence here would rescan the whole library
    /// on every pass, or nothing ever again.
    #[test]
    fn the_sidecar_signature_matches_the_hub_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let media = root.join("Solaris (1972).mkv");
        std::fs::write(&media, b"x").unwrap();

        // Nothing beside it: empty on both sides, so an item with no
        // sidecars never looks changed.
        assert_eq!(sidecar_sig(root, &media), "");
        assert_eq!(kahawai_hub_sig("", "", &[]), "");

        std::fs::write(root.join("Solaris (1972).nfo"), b"<movie/>").unwrap();
        let with_nfo = sidecar_sig(root, &media);
        assert_eq!(with_nfo, kahawai_hub_sig("Solaris (1972).nfo", "", &[]));
        assert_ne!(with_nfo, "", "a dropped-in .nfo must change the signature");

        // Artwork is matched by name (cover/folder/poster/front), not
        // by the media's stem.
        std::fs::write(root.join("cover.jpg"), b"JPEG").unwrap();
        let both = sidecar_sig(root, &media);
        assert_eq!(
            both,
            kahawai_hub_sig("Solaris (1972).nfo", "cover.jpg", &[])
        );
        assert_ne!(both, with_nfo, "artwork appearing must change it too");

        // And removal is symmetric — the case that left a stale answer
        // pointing at a .nfo nobody could read.
        std::fs::remove_file(root.join("Solaris (1972).nfo")).unwrap();
        assert_ne!(
            sidecar_sig(root, &media),
            both,
            "a vanished .nfo must change it"
        );

        // A subtitle sidecar appearing beside an unchanged file must
        // change the signature too — the gap that hid 42 real .idx
        // pairs: the idx counts ONCE however many tracks it carries,
        // and the hub spells it from external_subtitles path_rels.
        let before = sidecar_sig(root, &media);
        std::fs::write(
            root.join("Solaris (1972).idx"),
            b"id: en, index: 0
id: nl, index: 1
",
        )
        .unwrap();
        std::fs::write(root.join("Solaris (1972).sub"), b"").unwrap();
        let with_subs = sidecar_sig(root, &media);
        assert_ne!(with_subs, before, "an .idx pair appearing must change it");
        assert_eq!(
            with_subs,
            kahawai_hub_sig("", "cover.jpg", &["Solaris (1972).idx".to_string()])
        );
    }

    /// The hub's spelling, copied here on purpose: if `sidecar_sig` on
    /// either side is edited alone, this test fails rather than the
    /// library silently rescanning forever.
    fn kahawai_hub_sig(nfo: &str, artwork: &str, subs: &[String]) -> String {
        if nfo.is_empty() && artwork.is_empty() && subs.is_empty() {
            return String::new();
        }
        format!("n:{nfo}|a:{artwork}|s:{}", subs.join(","))
    }

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
            .map(|s| {
                (
                    s.path_rel.as_str(),
                    s.format.as_str(),
                    s.language.as_deref(),
                )
            })
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

    #[tokio::test]
    async fn equal_relative_names_under_two_roots_scan_as_exact_sources() {
        if !kahawai_media::testutil::require_elements(&["x264enc", "flacenc"]) {
            return;
        }
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let path = a.path().join("same.mkv");
        kahawai_media::testutil::render_h264_flac_mkv(&path);
        std::fs::copy(&path, b.path().join("same.mkv")).unwrap();
        let a_token = kahawai_core::media::root_token(a.path());
        let b_token = kahawai_core::media::root_token(b.path());
        let cfg = CollectionConfig {
            name: "movies".into(),
            media_type: "movies".into(),
            // Deliberately reversed relative to token ordering: list order has
            // no place in the records emitted by the scanner.
            roots: vec![b.path().into(), a.path().into()],
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let (manifest_tx, manifest_rx) = tokio::sync::mpsc::channel(1);
        manifest_tx
            .send(kahawai_proto::v1::Manifest {
                collection_id: "movies".into(),
                entries: Vec::new(),
                done: true,
                in_sync: false,
                sidecars_compared: true,
                root_adoption: false,
            })
            .await
            .unwrap();
        drop(manifest_tx);

        assert_eq!(
            scan_collection(cfg, tx, manifest_rx, Default::default(), 1)
                .await
                .unwrap(),
            ScanOutcome::Completed
        );
        let mut sources = Vec::new();
        while let Ok(message) = rx.try_recv() {
            if let host_to_hub::Msg::FileUpsert(upsert) = message.msg.unwrap() {
                sources.extend(upsert.files.into_iter().map(|file| {
                    let source = file.source.unwrap();
                    (source.root_token, source.path_rel)
                }));
            }
        }
        sources.sort();
        let mut expected = vec![
            (a_token, "same.mkv".to_string()),
            (b_token, "same.mkv".to_string()),
        ];
        expected.sort();
        assert_eq!(sources, expected);
    }

    #[tokio::test]
    async fn an_unavailable_root_preserves_its_manifest_and_other_roots_continue() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let available = dir.path().join("available");
        std::fs::create_dir(&available).unwrap();
        let missing_token = kahawai_core::media::root_token(&missing);
        let cfg = CollectionConfig {
            name: "movies".into(),
            media_type: "movies".into(),
            roots: vec![missing, available],
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let (manifest_tx, manifest_rx) = tokio::sync::mpsc::channel(1);
        manifest_tx
            .send(kahawai_proto::v1::Manifest {
                collection_id: "movies".into(),
                entries: vec![kahawai_proto::v1::FileStat {
                    source: Some(kahawai_proto::v1::SourcePath {
                        root_token: missing_token.clone(),
                        path_rel: "kept.mkv".into(),
                    }),
                    size: 10,
                    mtime_unix: 1,
                    sidecars: String::new(),
                }],
                done: true,
                in_sync: false,
                sidecars_compared: true,
                root_adoption: false,
            })
            .await
            .unwrap();
        drop(manifest_tx);

        assert_eq!(
            scan_collection(cfg, tx, manifest_rx, Default::default(), 9)
                .await
                .unwrap(),
            ScanOutcome::Completed
        );
        let mut unavailable = false;
        let mut preserved = false;
        let mut complete = false;
        while let Ok(message) = rx.try_recv() {
            match message.msg.unwrap() {
                host_to_hub::Msg::FileError(error) => {
                    let source = error.source.unwrap();
                    unavailable = source.root_token == missing_token
                        && source.path_rel.is_empty()
                        && error.error.contains("root unavailable");
                }
                host_to_hub::Msg::FilesSeen(seen) => {
                    preserved = seen.sources.iter().any(|source| {
                        source.root_token == missing_token && source.path_rel == "kept.mkv"
                    });
                }
                host_to_hub::Msg::ScanProgress(progress) if progress.complete => {
                    complete = progress.sync_version == 9;
                }
                _ => {}
            }
        }
        assert!(unavailable, "the exact unavailable root must be reported");
        assert!(
            preserved,
            "its old catalogue rows must survive reconciliation"
        );
        assert!(complete, "the collection scan must continue and finish");
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
