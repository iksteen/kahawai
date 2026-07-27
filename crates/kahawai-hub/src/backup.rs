//! Online backup and restore (OPS-5).
//!
//! A snapshot holds everything needed to rebuild this hub somewhere else:
//! the database, the PKI material, the downloaded subtitles and the
//! configuration. The CA is the reason restore is worth having at all —
//! with it, existing satellites reconnect on their own certificates and
//! nobody re-enrols five machines by hand.
//!
//! What is deliberately NOT in a snapshot is everything a running hub can
//! work out again: the artwork cache and the provider caches (AniDB
//! dumps, the anime-lists mapping, HTTP-API records). Those are 225 MB
//! here against 12 KB of PKI, and re-fetching them costs time rather than
//! anything irreplaceable. Subtitles ARE included: they are user-initiated
//! content, which is also why OPS-6 refuses to evict them.
//!
//! "Online" is the whole trick. `VACUUM INTO` takes a consistent snapshot
//! of the database — WAL included — while the hub keeps serving; there is
//! no window where writes are refused. The file trees are copied
//! afterwards, so a subtitle downloaded mid-backup may or may not be in
//! it. That is what a point-in-time snapshot means, and it is why the
//! manifest records when the database was taken rather than when the
//! command finished.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Written beside the snapshot so a restore can tell what it is holding —
/// and refuse a directory that merely looks like one.
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// Format of the snapshot itself, not of kahawai. Bumped only when
    /// the layout changes in a way a restore must know about.
    pub format: u32,
    pub kahawai_version: String,
    /// When the DATABASE was snapshotted, which is the consistent point.
    pub taken_at: i64,
    pub db_bytes: u64,
    pub subtitle_files: u64,
    pub subtitle_bytes: u64,
    pub has_pki: bool,
    pub has_config: bool,
}

const FORMAT: u32 = 1;
const MANIFEST: &str = "kahawai-backup.json";

/// Everything a restore puts back, relative to the data dir. Ordered so
/// the database lands first: it is the part a partial restore most needs.
const TREES: [&str; 2] = ["pki", "subtitles"];

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// Snapshot a live hub into `dest`, which must not already exist.
///
/// `config` is the loaded config file's path, copied verbatim so a
/// restored hub keeps its ports, keys and provider settings.
pub async fn backup(data_dir: &Path, config: Option<&Path>, dest: &Path) -> Result<Manifest> {
    if dest.exists() {
        bail!("{} already exists — snapshots are never merged", dest.display());
    }
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;

    // The database first, and through sqlite rather than the filesystem:
    // copying hub.db while the hub runs would catch a torn page or miss
    // the WAL entirely. VACUUM INTO serialises against writers itself.
    let pool = crate::db::open(data_dir).await.context("opening the hub database")?;
    let db_out = dest.join("hub.db");
    let taken_at = now_unix();
    sqlx::query("VACUUM INTO ?")
        .bind(db_out.to_str().context("snapshot path is not utf-8")?)
        .execute(&pool)
        .await
        .context("VACUUM INTO — is the destination on a writable filesystem?")?;
    pool.close().await;
    let db_bytes = std::fs::metadata(&db_out)?.len();

    let mut subtitle_files = 0;
    let mut subtitle_bytes = 0;
    for tree in TREES {
        let from = data_dir.join(tree);
        if !from.exists() {
            continue;
        }
        let (n, bytes) = copy_tree(&from, &dest.join(tree))
            .with_context(|| format!("copying {}", from.display()))?;
        if tree == "subtitles" {
            (subtitle_files, subtitle_bytes) = (n, bytes);
        }
    }

    // The token secret lives beside the database, not in a tree: without
    // it every session is invalidated by a restore.
    let jwt = data_dir.join("jwt.secret");
    if jwt.exists() {
        std::fs::copy(&jwt, dest.join("jwt.secret")).context("copying jwt.secret")?;
    }
    let has_config = match config {
        Some(path) if path.exists() => {
            std::fs::copy(path, dest.join("kahawai.toml")).context("copying config")?;
            true
        }
        _ => false,
    };

    let manifest = Manifest {
        format: FORMAT,
        kahawai_version: env!("CARGO_PKG_VERSION").to_string(),
        taken_at,
        db_bytes,
        subtitle_files,
        subtitle_bytes,
        has_pki: dest.join("pki").exists(),
        has_config,
    };
    std::fs::write(dest.join(MANIFEST), serde_json::to_vec_pretty(&manifest)?)
        .context("writing the manifest")?;
    Ok(manifest)
}

/// Restore a snapshot into `data_dir`.
///
/// Refuses a data dir that already holds a database unless `force`: a
/// restore over a live hub would leave the on-disk state and the running
/// process disagreeing, and the running process wins until it exits.
pub fn restore(src: &Path, data_dir: &Path, force: bool) -> Result<Manifest> {
    let raw = std::fs::read(src.join(MANIFEST))
        .with_context(|| format!("{} is not a kahawai snapshot", src.display()))?;
    let manifest: Manifest = serde_json::from_slice(&raw).context("unreadable manifest")?;
    if manifest.format > FORMAT {
        bail!(
            "snapshot format {} is newer than this build understands ({FORMAT})",
            manifest.format
        );
    }
    let existing = data_dir.join("hub.db");
    if existing.exists() && !force {
        bail!(
            "{} already holds a database — stop the hub and pass --force to replace it",
            data_dir.display()
        );
    }
    std::fs::create_dir_all(data_dir)?;

    // The WAL and shm belong to the database being replaced. Leaving them
    // would hand sqlite a journal describing a file that no longer exists.
    for stale in ["hub.db-wal", "hub.db-shm"] {
        let _ = std::fs::remove_file(data_dir.join(stale));
    }
    std::fs::copy(src.join("hub.db"), &existing).context("restoring the database")?;
    for tree in TREES {
        let from = src.join(tree);
        if from.exists() {
            copy_tree(&from, &data_dir.join(tree))
                .with_context(|| format!("restoring {}", from.display()))?;
        }
    }
    let jwt = src.join("jwt.secret");
    if jwt.exists() {
        std::fs::copy(&jwt, data_dir.join("jwt.secret")).context("restoring jwt.secret")?;
    }
    Ok(manifest)
}

/// Recursive copy, returning (files, bytes). Existing files are
/// overwritten — a restore is meant to be authoritative.
fn copy_tree(from: &Path, to: &Path) -> Result<(u64, u64)> {
    std::fs::create_dir_all(to)?;
    let (mut files, mut bytes) = (0u64, 0u64);
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            let (n, b) = copy_tree(&src, &dst)?;
            files += n;
            bytes += b;
        } else {
            bytes += std::fs::copy(&src, &dst)
                .with_context(|| format!("copying {}", src.display()))?;
            files += 1;
        }
    }
    Ok((files, bytes))
}
