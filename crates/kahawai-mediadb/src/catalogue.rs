use crate::*;
use anyhow::{Context, Result, bail, ensure};
use kahawai_proto::v1 as p;
use prost::Message;
use sqlx::Row;
use std::path::{Component, Path};

/// Authoritative discovery observations in their existing protocol types. The
/// base FileRecord remains separate; facts are not premerged into its MediaInfo.
#[derive(Debug, Clone)]
pub enum SourceFact {
    Error(p::FileError),
    Hashes(p::FileHashes),
    Loudness(p::FileLoudness),
    Attachments(p::FileAttachments),
    Keyframe(p::FileKeyframeInterval),
    Geometry(p::FileVideoGeometry),
    Segments(p::SegmentDetectionResult),
}
impl SourceFact {
    fn decode(kind: &str, payload: &[u8]) -> Result<Self> {
        Ok(match kind {
            "file_error" => Self::Error(p::FileError::decode(payload)?),
            "file_hashes" => Self::Hashes(p::FileHashes::decode(payload)?),
            "file_loudness" => Self::Loudness(p::FileLoudness::decode(payload)?),
            "file_attachments" => Self::Attachments(p::FileAttachments::decode(payload)?),
            "file_keyframe" => Self::Keyframe(p::FileKeyframeInterval::decode(payload)?),
            "file_geometry" => Self::Geometry(p::FileVideoGeometry::decode(payload)?),
            "file_segments" => Self::Segments(p::SegmentDetectionResult::decode(payload)?),
            _ => bail!("unsupported catalogue record kind {kind}"),
        })
    }
    fn identity(&self) -> Result<(&str, &p::SourcePath, Option<u64>, Option<i64>)> {
        let (collection, source, size, mtime) = match self {
            Self::Error(v) => (v.collection_id.as_str(), v.source.as_ref(), None, None),
            Self::Hashes(v) => {
                ensure!(v.hashes.len() == 1, "hash record must contain one source");
                let h = &v.hashes[0];
                (&*v.collection_id, h.source.as_ref(), Some(h.size), None)
            }
            Self::Loudness(v) => (
                &*v.collection_id,
                v.source.as_ref(),
                Some(v.size),
                Some(v.mtime_unix),
            ),
            Self::Attachments(v) => (&*v.collection_id, v.source.as_ref(), Some(v.size), None),
            Self::Keyframe(v) => (&*v.collection_id, v.source.as_ref(), Some(v.size), None),
            Self::Geometry(v) => (&*v.collection_id, v.source.as_ref(), Some(v.size), None),
            Self::Segments(v) => {
                ensure!(
                    v.episodes.len() == 1,
                    "segment record must contain one source"
                );
                let e = &v.episodes[0];
                (
                    &*v.collection_id,
                    e.source.as_ref(),
                    Some(e.observed_size),
                    Some(e.observed_mtime_unix),
                )
            }
        };
        Ok((
            collection,
            source.context("missing exact source")?,
            size,
            mtime,
        ))
    }
}
fn source_key(key: &[u8]) -> Result<(&str, &str)> {
    let key = std::str::from_utf8(key)?;
    let (root, path) = key
        .split_once('\0')
        .context("missing exact catalogue source key")?;
    ensure!(
        !root.is_empty() && !path.is_empty() && !path.contains('\0') && !path.contains('\\'),
        "invalid source address"
    );
    ensure!(
        path.split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."),
        "source path is not normalized"
    );
    ensure!(
        Path::new(path)
            .components()
            .all(|p| matches!(p, Component::Normal(_))),
        "source path is not confined"
    );
    Ok((root, path))
}
impl Store {
    /// Each offer starts a new snapshot attempt when the cursor is unusable.
    /// Reoffering after interruption resets seen marks through a new generation.
    pub async fn offer_collection(
        &self,
        host: &str,
        offer: &p::CatalogCollection,
    ) -> Result<(String, p::CatalogCursor)> {
        let kind = MediaType::parse(&offer.media_type)?;
        ensure!(
            !offer.id.is_empty() && !offer.epoch.is_empty(),
            "empty collection identity"
        );
        ensure!(
            offer.oldest_replayable_version <= offer.current_version,
            "invalid replay interval"
        );
        integer(offer.current_version)?;
        for (i, root) in offer.roots.iter().enumerate() {
            ensure!(
                !root.root_token.is_empty()
                    && !root.root_token.contains('\0')
                    && Path::new(&root.normalized_path).is_absolute(),
                "invalid root"
            );
            ensure!(
                !Path::new(&root.normalized_path)
                    .components()
                    .any(|c| matches!(c, Component::ParentDir)),
                "root is not normalized"
            );
            for other in &offer.roots[..i] {
                ensure!(
                    root.root_token != other.root_token
                        && !Path::new(&root.normalized_path).starts_with(&other.normalized_path)
                        && !Path::new(&other.normalized_path).starts_with(&root.normalized_path),
                    "duplicate or overlapping roots"
                );
            }
        }
        let mut tx = self.db.begin().await?;
        let existing =
            sqlx::query("SELECT * FROM collections WHERE mediahost_id=? AND remote_id=?")
                .bind(host)
                .bind(&offer.id)
                .fetch_optional(&mut *tx)
                .await?;
        let (collection, version, snapshot, generation, epoch_changed) = if let Some(row) = existing
        {
            ensure!(
                row.get::<&str, _>("media_type") == kind.as_str(),
                "collection type changed; remove its old namespace explicitly first"
            );
            let version = row.get::<i64, _>("version") as u64;
            let changed = row.get::<&str, _>("epoch") != offer.epoch;
            let snapshot = changed
                || row.get::<bool, _>("snapshot_active")
                || version == 0
                || version > offer.current_version
                || version < offer.oldest_replayable_version;
            (
                row.get::<String, _>("id"),
                if snapshot { 0 } else { version },
                snapshot,
                row.get::<i64, _>("generation") + i64::from(snapshot),
                changed,
            )
        } else {
            (id(), 0, true, 1, false)
        };
        sqlx::query("INSERT INTO collections(id,mediahost_id,remote_id,media_type,epoch,version,snapshot_active,generation) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET epoch=excluded.epoch,version=excluded.version,snapshot_active=excluded.snapshot_active,generation=excluded.generation,snapshot_max=0")
            .bind(&collection).bind(host).bind(&offer.id).bind(kind.as_str()).bind(&offer.epoch).bind(integer(version)?).bind(snapshot).bind(generation).execute(&mut *tx).await?;
        if epoch_changed {
            sqlx::query("UPDATE files SET version=0 WHERE collection_id=?")
                .bind(&collection)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE source_facts SET version=0 WHERE file_id IN(SELECT id FROM files WHERE collection_id=?)").bind(&collection).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE collection_roots SET active=0 WHERE collection_id=?")
            .bind(&collection)
            .execute(&mut *tx)
            .await?;
        for root in &offer.roots {
            let old: Option<String> = sqlx::query_scalar(
                "SELECT path FROM collection_roots WHERE collection_id=? AND token=?",
            )
            .bind(&collection)
            .bind(&root.root_token)
            .fetch_optional(&mut *tx)
            .await?;
            ensure!(
                old.as_deref().is_none_or(|p| p == root.normalized_path),
                "root token changed path"
            );
            sqlx::query("INSERT INTO collection_roots VALUES(?,?,?,?,1) ON CONFLICT(collection_id,token) DO UPDATE SET active=1")
                .bind(id()).bind(&collection).bind(&root.root_token).bind(&root.normalized_path).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok((
            collection,
            p::CatalogCursor {
                collection_id: offer.id.clone(),
                epoch: offer.epoch.clone(),
                version,
                snapshot,
            },
        ))
    }
    pub async fn catalogue_cursor(&self, collection: &str) -> Result<p::CatalogCursor> {
        let row = sqlx::query("SELECT * FROM collections WHERE id=?")
            .bind(collection)
            .fetch_one(self.db.read_pool())
            .await?;
        Ok(p::CatalogCursor {
            collection_id: row.get("remote_id"),
            epoch: row.get("epoch"),
            version: row.get::<i64, _>("version") as u64,
            snapshot: row.get("snapshot_active"),
        })
    }
    /// A chunk and its cursor commit together. No ACK is returned before commit.
    /// Unknown future kinds fail the whole chunk instead of silently losing data.
    pub async fn apply_catalogue(
        &self,
        host: &str,
        delta: &p::CatalogDelta,
    ) -> Result<Option<p::CatalogAck>> {
        let mut tx = self.db.begin().await?;
        let row = sqlx::query("SELECT * FROM collections WHERE mediahost_id=? AND remote_id=?")
            .bind(host)
            .bind(&delta.collection_id)
            .fetch_one(&mut *tx)
            .await?;
        ensure!(
            row.get::<&str, _>("epoch") == delta.epoch,
            "catalogue epoch mismatch"
        );
        let collection: String = row.get("id");
        let cursor = row.get::<i64, _>("version") as u64;
        let generation: i64 = row.get("generation");
        let snapshot: bool = row.get("snapshot_active");
        integer(delta.through_version)?;
        if delta.snapshot && !snapshot {
            ensure!(
                delta.done && delta.through_version <= cursor,
                "unexpected snapshot chunk"
            );
            return Ok(Some(p::CatalogAck {
                collection_id: delta.collection_id.clone(),
                epoch: delta.epoch.clone(),
                version: cursor,
            }));
        }
        ensure!(snapshot == delta.snapshot, "snapshot mode mismatch");
        ensure!(
            !snapshot || delta.done || delta.through_version == 0,
            "unfinished snapshot advanced cursor"
        );
        let mut previous = 0;
        let mut snapshot_max = row.get::<i64, _>("snapshot_max") as u64;
        for record in &delta.records {
            ensure!(record.version > 0, "zero record version");
            integer(record.version)?;
            snapshot_max = snapshot_max.max(record.version);
            if !snapshot {
                ensure!(
                    record.version > previous && record.version <= delta.through_version,
                    "unordered catalogue chunk or unsafe cursor"
                );
                previous = record.version;
                if record.version <= cursor {
                    continue;
                }
            }
            if snapshot && delta.done {
                ensure!(
                    record.version <= delta.through_version,
                    "snapshot cursor precedes record"
                );
            }
            let (token, path) = source_key(&record.key)?;
            if record.deleted {
                ensure!(!snapshot, "snapshot contains tombstone");
                if record.kind == "file" {
                    sqlx::query("DELETE FROM files WHERE root_id IN(SELECT id FROM collection_roots WHERE collection_id=? AND token=?) AND path=?")
                        .bind(&collection).bind(token).bind(path).execute(&mut *tx).await?;
                } else {
                    // Validate the kind even though a tombstone has no payload.
                    ensure!(
                        [
                            "file_error",
                            "file_hashes",
                            "file_loudness",
                            "file_attachments",
                            "file_keyframe",
                            "file_geometry",
                            "file_segments"
                        ]
                        .contains(&record.kind.as_str()),
                        "unsupported tombstone kind"
                    );
                    sqlx::query("DELETE FROM source_facts WHERE kind=? AND file_id IN(SELECT f.id FROM files f JOIN collection_roots r ON r.id=f.root_id WHERE r.collection_id=? AND r.token=? AND f.path=?)")
                        .bind(&record.kind).bind(&collection).bind(token).bind(path).execute(&mut *tx).await?;
                }
                continue;
            }
            let root: String = sqlx::query_scalar(
                "SELECT id FROM collection_roots WHERE collection_id=? AND token=? AND active=1",
            )
            .bind(&collection)
            .bind(token)
            .fetch_one(&mut *tx)
            .await?;
            let file:String=sqlx::query_scalar("INSERT INTO files(id,collection_id,root_id,path) VALUES(?,?,?,?) ON CONFLICT(root_id,path) DO UPDATE SET path=excluded.path RETURNING id")
                .bind(id()).bind(&collection).bind(&root).bind(path).fetch_one(&mut *tx).await?;
            if record.kind == "file" {
                let value = p::FileUpsert::decode(record.payload.as_slice())?;
                ensure!(
                    value.collection_id == delta.collection_id && value.files.len() == 1,
                    "file record changed collection or count"
                );
                let value = &value.files[0];
                validate_source(value.source.as_ref(), token, path)?;
                let media: kahawai_core::media::MediaInfo =
                    serde_json::from_str(&value.streams_json)?;
                let old = sqlx::query("SELECT * FROM files WHERE id=?")
                    .bind(&file)
                    .fetch_one(&mut *tx)
                    .await?;
                sqlx::query("UPDATE files SET seen=? WHERE id=?")
                    .bind(generation)
                    .bind(&file)
                    .execute(&mut *tx)
                    .await?;
                if !snapshot && old.get::<i64, _>("version") > integer(record.version)? {
                    continue;
                }
                let changed = old.get::<Option<i64>, _>("size") != Some(integer(value.size)?)
                    || old.get::<Option<i64>, _>("mtime") != Some(value.mtime_unix)
                    || old.get::<Option<Vec<u8>>, _>("head_hash").as_deref()
                        != Some(value.head_xxh3.to_le_bytes().as_slice())
                    || old.get::<Option<Vec<u8>>, _>("tail_hash").as_deref()
                        != Some(value.tail_xxh3.to_le_bytes().as_slice());
                if changed {
                    sqlx::query("DELETE FROM source_facts WHERE file_id=?")
                        .bind(&file)
                        .execute(&mut *tx)
                        .await?;
                }
                sqlx::query("UPDATE files SET version=?,size=?,mtime=?,head_hash=?,tail_hash=?,oshash=?,media_json=? WHERE id=?")
                    .bind(integer(record.version)?).bind(integer(value.size)?).bind(value.mtime_unix).bind(value.head_xxh3.to_le_bytes().to_vec()).bind(value.tail_xxh3.to_le_bytes().to_vec())
                    .bind(value.oshash.to_le_bytes().to_vec()).bind(serde_json::to_string(&media)?).bind(&file).execute(&mut *tx).await?;
                crate::occurrence::resolve_file(&mut tx, &collection, &root, &file, path, &media)
                    .await?;
            } else {
                let fact = SourceFact::decode(&record.kind, &record.payload)?;
                let (remote, source, size, mtime) = fact.identity()?;
                ensure!(remote == delta.collection_id, "fact changed collection");
                validate_source(Some(source), token, path)?;
                if let Some(size) = size {
                    let base: (Option<i64>, Option<i64>) =
                        sqlx::query_as("SELECT size,mtime FROM files WHERE id=?")
                            .bind(&file)
                            .fetch_one(&mut *tx)
                            .await?;
                    ensure!(
                        base.0 == Some(integer(size)?) && mtime.is_none_or(|m| Some(m) == base.1),
                        "discovery fact has stale or missing source revision"
                    );
                }
                if matches!(fact, SourceFact::Error(_)) {
                    let seen: i64 = sqlx::query_scalar("SELECT seen FROM files WHERE id=?")
                        .bind(&file)
                        .fetch_one(&mut *tx)
                        .await?;
                    if snapshot && seen != generation {
                        // Snapshots are file-first. An error without a base file
                        // in this generation replaces an older playable probe;
                        // keep the address and diagnostic, not its old rendition.
                        sqlx::query("DELETE FROM media_parts WHERE file_id=?")
                            .bind(&file)
                            .execute(&mut *tx)
                            .await?;
                        sqlx::query("DELETE FROM source_facts WHERE file_id=?")
                            .bind(&file)
                            .execute(&mut *tx)
                            .await?;
                        sqlx::query("UPDATE files SET version=0,size=NULL,mtime=NULL,head_hash=NULL,tail_hash=NULL,oshash=NULL,media_json=NULL,mapping_error=NULL WHERE id=?")
                            .bind(&file).execute(&mut *tx).await?;
                    }
                    sqlx::query("UPDATE files SET seen=? WHERE id=?")
                        .bind(generation)
                        .bind(&file)
                        .execute(&mut *tx)
                        .await?;
                }
                sqlx::query("INSERT INTO source_facts VALUES(?1,?2,?3,?4,?5) ON CONFLICT(file_id,kind) DO UPDATE SET seen=excluded.seen,
                    version=CASE WHEN ?6 THEN excluded.version ELSE MAX(source_facts.version,excluded.version) END,
                    payload=CASE WHEN ?6 OR excluded.version>=source_facts.version THEN excluded.payload ELSE source_facts.payload END")
                    .bind(&file).bind(&record.kind).bind(integer(record.version)?).bind(generation).bind(&record.payload).bind(snapshot).execute(&mut *tx).await?;
            }
        }
        if snapshot && delta.done {
            ensure!(
                delta.through_version >= snapshot_max,
                "snapshot cursor precedes earlier chunk"
            );
            sqlx::query("DELETE FROM source_facts WHERE seen<>? AND file_id IN(SELECT id FROM files WHERE collection_id=?)").bind(generation).bind(&collection).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM files WHERE collection_id=? AND seen<>?")
                .bind(&collection)
                .bind(generation)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM collection_roots WHERE collection_id=? AND active=0")
                .bind(&collection)
                .execute(&mut *tx)
                .await?;
        }
        crate::occurrence::prune(&mut tx).await?;
        let version = if snapshot && !delta.done {
            0
        } else {
            cursor.max(delta.through_version)
        };
        sqlx::query("UPDATE collections SET version=?,snapshot_active=?,snapshot_max=? WHERE id=?")
            .bind(integer(version)?)
            .bind(snapshot && !delta.done)
            .bind(if snapshot && !delta.done {
                integer(snapshot_max)?
            } else {
                0
            })
            .bind(&collection)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(delta.done.then(|| p::CatalogAck {
            collection_id: delta.collection_id.clone(),
            epoch: delta.epoch.clone(),
            version,
        }))
    }
    pub async fn source_facts(&self, file: &str) -> Result<Vec<SourceFact>> {
        sqlx::query("SELECT kind,payload FROM source_facts WHERE file_id=? ORDER BY kind")
            .bind(file)
            .fetch_all(self.db.read_pool())
            .await?
            .into_iter()
            .map(|r| SourceFact::decode(r.get("kind"), r.get("payload")))
            .collect()
    }
    pub async fn files(&self, collection: &str) -> Result<Vec<FileInfo>> {
        let rows=sqlx::query("SELECT f.*,r.token,(SELECT e.item_id FROM media_parts p JOIN media_entries e ON e.id=p.entry_id WHERE p.file_id=f.id) AS item_id FROM files f JOIN collection_roots r ON r.id=f.root_id WHERE f.collection_id=? ORDER BY r.token,f.path")
            .bind(collection).fetch_all(self.db.read_pool()).await?;
        rows.into_iter()
            .map(|r| {
                Ok(FileInfo {
                    id: r.get("id"),
                    root_id: r.get("root_id"),
                    root_token: r.get("token"),
                    path: r.get("path"),
                    size: r.get::<Option<i64>, _>("size").map(|n| n as u64),
                    mtime: r.get("mtime"),
                    head_hash: hash(r.get("head_hash"))?,
                    tail_hash: hash(r.get("tail_hash"))?,
                    oshash: hash(r.get("oshash"))?,
                    media: r
                        .get::<Option<&str>, _>("media_json")
                        .map(serde_json::from_str)
                        .transpose()?,
                    item_id: r.get("item_id"),
                    mapping_error: r.get("mapping_error"),
                })
            })
            .collect()
    }
}
fn hash(bytes: Option<Vec<u8>>) -> Result<Option<u64>> {
    bytes
        .map(|b| {
            Ok(u64::from_le_bytes(
                b.try_into()
                    .map_err(|_| anyhow::anyhow!("invalid stored hash"))?,
            ))
        })
        .transpose()
}
fn validate_source(source: Option<&p::SourcePath>, root: &str, path: &str) -> Result<()> {
    let source = source.context("missing source")?;
    ensure!(
        source.root_token == root && source.path_rel == path,
        "catalogue key and payload source differ"
    );
    Ok(())
}

impl Store {
    pub async fn collections(&self, host: &str) -> Result<Vec<Collection>> {
        sqlx::query("SELECT * FROM collections WHERE mediahost_id=? ORDER BY remote_id")
            .bind(host)
            .fetch_all(self.db.read_pool())
            .await?
            .into_iter()
            .map(|row| {
                Ok(Collection {
                    id: row.get("id"),
                    mediahost_id: row.get("mediahost_id"),
                    remote_id: row.get("remote_id"),
                    media_type: MediaType::parse(row.get("media_type"))?,
                    epoch: row.get("epoch"),
                    version: row.get::<i64, _>("version") as u64,
                })
            })
            .collect()
    }
    pub async fn roots(&self, collection: &str) -> Result<Vec<Root>> {
        Ok(
            sqlx::query("SELECT * FROM collection_roots WHERE collection_id=? ORDER BY token")
                .bind(collection)
                .fetch_all(self.db.read_pool())
                .await?
                .into_iter()
                .map(|row| Root {
                    id: row.get("id"),
                    token: row.get("token"),
                    path: row.get("path"),
                    active: row.get("active"),
                })
                .collect(),
        )
    }
}
