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

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    /// The secret files this snapshot carries, by name. Recorded rather than
    /// inferred, so a snapshot that lost one says so instead of restoring a
    /// hub that cannot open its own credentials. Absent in snapshots taken
    /// before it was recorded, which is not the same as "carried none".
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Every regular file in a format-3 snapshot except this manifest itself.
    /// Paths are portable, slash-separated names relative to the snapshot root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

/// Beside the database rather than in a tree. Without `jwt.secret` a restore
/// invalidates every session; without the credential key every stored
/// credential restores unopenable and the hub refuses to start, because the
/// database remembers seeding one; without the metrics token a restored hub
/// silently stops answering its scraper.
pub const SECRET_FILES: [&str; 3] = [
    "jwt.secret",
    crate::secrets::KEY_FILE,
    crate::api::METRICS_TOKEN_FILE,
];

const FORMAT: u32 = 3;
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
        bail!(
            "{} already exists — snapshots are never merged",
            dest.display()
        );
    }

    // The database first, and through sqlite rather than the filesystem:
    // copying hub.db while the hub runs would catch a torn page or miss
    // the WAL entirely. VACUUM INTO serialises against writers itself.
    let pool = crate::db::open(data_dir)
        .await
        .context("opening the hub database")?;
    let credential_key_expected: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
            .bind(crate::secrets::SEEDED_SETTING)
            .fetch_optional(&pool)
            .await
            .context("checking whether the credential key is required")?;
    let credential_key = if let Some(expected) = credential_key_expected {
        let path = data_dir.join(crate::secrets::KEY_FILE);
        let key = std::fs::read(&path)
            .with_context(|| format!("reading required {}", crate::secrets::KEY_FILE))?;
        anyhow::ensure!(
            crate::secrets::fingerprint(&key) == expected,
            "{} does not match the database",
            crate::secrets::KEY_FILE
        );
        Some(key)
    } else {
        None
    };
    // 0700, so what lands inside is unreachable to anyone else whatever mode
    // the writer chose: the snapshot holds every password hash, every session
    // and the key the credentials are sealed under.
    kahawai_core::private::create_dir(dest)
        .with_context(|| format!("creating {}", dest.display()))?;
    let db_out = dest.join("hub.db");
    let taken_at = now_unix();
    sqlx::query("VACUUM INTO ?")
        .bind(db_out.to_str().context("snapshot path is not utf-8")?)
        .execute(&pool)
        .await
        .context("VACUUM INTO — is the destination on a writable filesystem?")?;
    pool.close().await;
    // SQLite gives the new file 0666 & ~umask — 0644 under the usual one —
    // and there is no pragma for it. The directory above already hides it;
    // this makes it survive being moved somewhere less private.
    kahawai_core::private::narrow(&db_out).context("restricting the snapshot database")?;
    let db_bytes = std::fs::metadata(&db_out)?.len();
    let mut artifacts = vec![hash_artifact(&db_out, Path::new("hub.db"))?];

    let mut subtitle_files = 0;
    let mut subtitle_bytes = 0;
    for tree in TREES {
        let from = data_dir.join(tree);
        if !from.exists() {
            continue;
        }
        let (n, bytes) =
            copy_snapshot_tree(&from, &dest.join(tree), Path::new(tree), &mut artifacts)
                .with_context(|| format!("copying {}", from.display()))?;
        if tree == "subtitles" {
            (subtitle_files, subtitle_bytes) = (n, bytes);
        }
    }

    // A required credential key was read and validated above. Write those
    // exact bytes rather than reopening a file that could have changed while
    // the online database snapshot was being taken.
    let mut secrets = Vec::new();
    for name in SECRET_FILES {
        let from = data_dir.join(name);
        let artifact = if name == crate::secrets::KEY_FILE
            && let Some(key) = &credential_key
        {
            kahawai_core::private::write(&dest.join(name), key)
                .with_context(|| format!("copying {name}"))?;
            Some(artifact_for_bytes(Path::new(name), key)?)
        } else if from.exists() {
            Some(copy_artifact(&from, &dest.join(name), Path::new(name))?)
        } else {
            None
        };
        if let Some(artifact) = artifact {
            artifacts.push(artifact);
            secrets.push(name.to_string());
        }
    }
    let has_config = match config {
        Some(path) if path.exists() => {
            artifacts.push(
                copy_artifact(path, &dest.join("kahawai.toml"), Path::new("kahawai.toml"))
                    .context("copying config")?,
            );
            true
        }
        _ => false,
    };
    artifacts.sort_by(|a, b| a.path.cmp(&b.path));

    let manifest = Manifest {
        format: FORMAT,
        kahawai_version: env!("CARGO_PKG_VERSION").to_string(),
        taken_at,
        db_bytes,
        subtitle_files,
        subtitle_bytes,
        has_pki: dest.join("pki").exists(),
        has_config,
        secrets,
        artifacts,
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
pub async fn restore(src: &Path, data_dir: &Path, force: bool) -> Result<Manifest> {
    let raw = std::fs::read(src.join(MANIFEST))
        .with_context(|| format!("{} is not a kahawai snapshot", src.display()))?;
    let manifest: Manifest = serde_json::from_slice(&raw).context("unreadable manifest")?;
    if manifest.format > FORMAT {
        bail!(
            "snapshot format {} is newer than this build understands ({FORMAT})",
            manifest.format
        );
    }
    if manifest.format >= 3 {
        validate_artifacts(src, &manifest.artifacts)?;
    }
    // Validate the complete source before touching a destination. In
    // particular, a missing credential key must not be discovered after
    // `--force` has already replaced the live database.
    anyhow::ensure!(
        src.join("hub.db").is_file(),
        "the snapshot does not have hub.db"
    );
    for name in &manifest.secrets {
        anyhow::ensure!(
            src.join(name).is_file(),
            "the manifest lists {name}, and the snapshot does not have it"
        );
    }
    if manifest.format >= 2 {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(src.join("hub.db"))
                    .read_only(true),
            )
            .await
            .context("opening the snapshot database")?;
        let expected: Option<String> =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
                .bind(crate::secrets::SEEDED_SETTING)
                .fetch_optional(&db)
                .await
                .context("reading the snapshot credential key marker")?;
        db.close().await;
        if let Some(expected) = expected {
            let key_path = src.join(crate::secrets::KEY_FILE);
            anyhow::ensure!(
                manifest
                    .secrets
                    .iter()
                    .any(|name| name == crate::secrets::KEY_FILE)
                    && key_path.is_file(),
                "the snapshot database requires {}, but the snapshot does not carry it",
                crate::secrets::KEY_FILE
            );
            let key = std::fs::read(&key_path)
                .with_context(|| format!("reading {}", key_path.display()))?;
            anyhow::ensure!(
                crate::secrets::fingerprint(&key) == expected,
                "{} does not match the snapshot database",
                crate::secrets::KEY_FILE
            );
        }
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
    for name in SECRET_FILES {
        let from = src.join(name);
        if from.exists() {
            let to = data_dir.join(name);
            std::fs::copy(&from, &to).with_context(|| format!("restoring {name}"))?;
            // `fs::copy` carries the SOURCE's mode, and a snapshot that has
            // been through tar or an object store commonly comes back 0644.
            kahawai_core::private::narrow(&to).with_context(|| format!("restricting {name}"))?;
        }
    }
    Ok(manifest)
}

fn artifact_path(relative: &Path) -> Result<String> {
    let mut path = String::new();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            bail!("unsafe snapshot artifact path {}", relative.display());
        };
        let name = name
            .to_str()
            .context("snapshot artifact path is not utf-8")?;
        anyhow::ensure!(
            !name.contains('\\'),
            "snapshot artifact path is not portable: {}",
            relative.display()
        );
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(name);
    }
    anyhow::ensure!(!path.is_empty(), "empty snapshot artifact path");
    Ok(path)
}

fn artifact_for_bytes(relative: &Path, bytes: &[u8]) -> Result<Artifact> {
    Ok(Artifact {
        path: artifact_path(relative)?,
        bytes: bytes.len() as u64,
        sha256: data_encoding::HEXLOWER.encode(&Sha256::digest(bytes)),
    })
}

fn hash_artifact(path: &Path, relative: &Path) -> Result<Artifact> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening snapshot artifact {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading snapshot artifact {}", path.display()))?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .context("snapshot artifact is too large")?;
    }
    Ok(Artifact {
        path: artifact_path(relative)?,
        bytes,
        sha256: data_encoding::HEXLOWER.encode(&hash.finalize()),
    })
}

fn copy_artifact(from: &Path, to: &Path, relative: &Path) -> Result<Artifact> {
    let mut source =
        std::fs::File::open(from).with_context(|| format!("opening {}", from.display()))?;
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut target =
        kahawai_core::private::create(to).with_context(|| format!("creating {}", to.display()))?;
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = source
            .read(&mut buffer)
            .with_context(|| format!("reading {}", from.display()))?;
        if read == 0 {
            break;
        }
        target
            .write_all(&buffer[..read])
            .with_context(|| format!("writing {}", to.display()))?;
        hash.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .context("snapshot artifact is too large")?;
    }
    Ok(Artifact {
        path: artifact_path(relative)?,
        bytes,
        sha256: data_encoding::HEXLOWER.encode(&hash.finalize()),
    })
}

fn copy_snapshot_tree(
    from: &Path,
    to: &Path,
    relative: &Path,
    artifacts: &mut Vec<Artifact>,
) -> Result<(u64, u64)> {
    std::fs::create_dir_all(to)?;
    let (mut files, mut bytes) = (0u64, 0u64);
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let (src, dst, child) = (entry.path(), to.join(&name), relative.join(&name));
        if entry.file_type()?.is_dir() {
            let (n, b) = copy_snapshot_tree(&src, &dst, &child, artifacts)?;
            files += n;
            bytes += b;
        } else {
            let artifact = copy_artifact(&src, &dst, &child)?;
            bytes += artifact.bytes;
            files += 1;
            artifacts.push(artifact);
        }
    }
    Ok((files, bytes))
}

fn validate_artifacts(src: &Path, listed: &[Artifact]) -> Result<()> {
    let mut expected = BTreeMap::new();
    for artifact in listed {
        anyhow::ensure!(
            artifact.sha256.len() == 64
                && artifact
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid metadata for snapshot artifact {}",
            artifact.path
        );
        let relative = Path::new(&artifact.path);
        anyhow::ensure!(
            artifact_path(relative)? == artifact.path,
            "unsafe snapshot artifact path {}",
            artifact.path
        );
        anyhow::ensure!(
            expected.insert(artifact.path.as_str(), artifact).is_none(),
            "snapshot manifest lists {} more than once",
            artifact.path
        );
    }

    let mut actual = BTreeMap::new();
    collect_artifacts(src, src, &mut actual)?;
    for (path, artifact) in expected {
        let found = actual.remove(path).with_context(|| {
            format!("the manifest lists {path}, and the snapshot does not have it")
        })?;
        anyhow::ensure!(
            found.bytes == artifact.bytes,
            "snapshot artifact {path} has {} bytes, expected {}",
            found.bytes,
            artifact.bytes
        );
        anyhow::ensure!(
            found.sha256 == artifact.sha256,
            "snapshot artifact {path} failed its SHA-256 check"
        );
    }
    if let Some(path) = actual.keys().next() {
        bail!("snapshot contains unlisted artifact {path}");
    }
    Ok(())
}

fn collect_artifacts(
    root: &Path,
    directory: &Path,
    artifacts: &mut BTreeMap<String, Artifact>,
) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root)?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_artifacts(root, &path, artifacts)?;
        } else if file_type.is_file() {
            if relative == Path::new(MANIFEST) {
                continue;
            }
            let artifact = hash_artifact(&path, relative)?;
            artifacts.insert(artifact.path.clone(), artifact);
        } else {
            bail!(
                "snapshot artifact {} is not a regular file",
                relative.display()
            );
        }
    }
    Ok(())
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
            bytes +=
                std::fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
            files += 1;
        }
    }
    Ok((files, bytes))
}
