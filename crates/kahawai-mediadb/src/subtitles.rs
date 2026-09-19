//! Acquired subtitles belong to an immutable version of a physical rendition,
//! including the ordered files of a multipart release. They never follow a title,
//! assignment or another copy. Their metadata and parsed payload commit together.
//! The entry ID is historical ownership, deliberately not a cascading foreign
//! key: removing/replacing media must not discard an acquired subtitle. Such rows
//! are invisible until their exact entry/version is requested. User IDs are opaque
//! provenance; authorization remains in the hub. Downloads are durable assets,
//! not evictable caches: reacquisition spends provider entitlement again.
use crate::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, SqliteConnection};

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct DownloadedSubtitle {
    pub id: i64,
    pub media_entry_id: String,
    pub source_version: String,
    pub provider: String,
    pub provider_file_id: String,
    pub format: String,
    pub language: Option<String>,
    pub label: Option<String>,
    pub created_by: String,
    pub payload: String,
}
impl PlaybackRendition {
    pub fn source_version(&self) -> String {
        file_version(&self.files)
    }
}
fn file_version(files: &[FileInfo]) -> String {
    let facts: Vec<_> = files
        .iter()
        .map(|f| (&f.id, f.size, f.mtime, f.head_hash, f.tail_hash))
        .collect();
    Sha256::digest(serde_json::to_vec(&facts).expect("file identities serialize"))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub(crate) async fn read_downloaded(
    c: &mut SqliteConnection,
    entry: &str,
    files: &[FileInfo],
) -> Result<Vec<DownloadedSubtitle>> {
    Ok(sqlx::query_as("SELECT * FROM downloaded_subtitles WHERE media_entry_id=? AND source_version=? ORDER BY id")
        .bind(entry).bind(file_version(files)).fetch_all(c).await?)
}
impl Store {
    pub async fn put_downloaded_subtitle(&self, s: &DownloadedSubtitle) -> Result<i64> {
        ensure!(
            !s.media_entry_id.is_empty()
                && !s.source_version.is_empty()
                && !s.provider_file_id.is_empty()
                && !s.payload.is_empty(),
            "incomplete subtitle asset"
        );
        Ok(sqlx::query_scalar("INSERT INTO downloaded_subtitles(media_entry_id,source_version,provider,provider_file_id,format,language,label,created_by,payload) VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(media_entry_id,source_version,provider,provider_file_id) DO UPDATE SET id=id RETURNING id")
            .bind(&s.media_entry_id).bind(&s.source_version).bind(&s.provider).bind(&s.provider_file_id).bind(&s.format).bind(&s.language).bind(&s.label).bind(&s.created_by).bind(&s.payload).fetch_one(&self.db).await?)
    }
    pub async fn remove_downloaded_subtitle(
        &self,
        id: i64,
        entry: &str,
        version: &str,
        user: &str,
        admin: bool,
    ) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM downloaded_subtitles WHERE id=? AND media_entry_id=? AND source_version=? AND (created_by=? OR ?)")
            .bind(id).bind(entry).bind(version).bind(user).bind(admin).execute(&self.db).await?.rows_affected() != 0)
    }
}
