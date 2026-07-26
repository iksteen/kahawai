//! Local artwork serving (MH-4): cover/folder/poster images detected by
//! the scan, fetched through the source's read lease once and cached on
//! the hub. Albums and shows inherit artwork from their children's
//! directories (they have no sources of their own).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use sqlx::Row;

use crate::registry::Registry;
use crate::sessions::Sessions;

pub struct Artwork {
    dir: PathBuf,
    enricher: Arc<crate::enrich::Enricher>,
    /// Per-key locks so concurrent requests fetch once.
    inflight: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Artwork {
    pub fn new(dir: PathBuf, enricher: Arc<crate::enrich::Enricher>) -> Self {
        Self { dir, enricher, inflight: Default::default() }
    }

    /// Image bytes + content type for an item, or None when it has no
    /// local artwork. Checks the item's own sources first, then its
    /// children's (albums/shows).
    pub async fn get(
        &self,
        registry: &Registry,
        sessions: &Sessions,
        item_id: &str,
    ) -> Result<Option<(Vec<u8>, &'static str)>> {
        let Some((module_id, collection_id, art_rel)) =
            find_artwork_source(registry, item_id).await?
        else {
            // No local artwork: fall back to the enrichment poster.
            return self.tmdb_poster(registry, item_id).await;
        };

        let ctype = match art_rel.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
            Some("png") => "image/png",
            Some("webp") => "image/webp",
            _ => "image/jpeg",
        };
        let cache_key = format!(
            "{:016x}",
            xxhash_rust::xxh3::xxh3_64(
                format!("{module_id}\n{collection_id}\n{art_rel}").as_bytes()
            )
        );
        let lock = {
            let mut map = self.inflight.lock().unwrap();
            map.entry(cache_key.clone()).or_default().clone()
        };
        let _guard = lock.lock().await;

        let cache_path = self.dir.join(&cache_key);
        if let Ok(bytes) = std::fs::read(&cache_path) {
            return Ok(Some((bytes, ctype)));
        }
        let lease =
            sessions.open_lease(registry, &module_id, &collection_id, &art_rel).await?;
        let bytes = read_all(lease).await?;
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(&cache_path, &bytes)?;
        Ok(Some((bytes, ctype)))
    }
}

impl Artwork {
    /// Enrichment poster (TMDB), cached like local artwork. The item's
    /// own metadata first, its parent's (episodes) as fallback.
    async fn tmdb_poster(
        &self,
        registry: &Registry,
        item_id: &str,
    ) -> Result<Option<(Vec<u8>, &'static str)>> {
        let poster: Option<String> = sqlx::query_scalar(
            "SELECT m.poster_path FROM items i
             JOIN resolved_metadata m ON m.item_id IN (i.id, i.parent_id)
             WHERE i.id = ? AND m.poster_path IS NOT NULL
             ORDER BY m.item_id = i.id DESC LIMIT 1",
        )
        .bind(item_id)
        .fetch_optional(registry.db())
        .await?
        .flatten();
        let Some(poster) = poster else { return Ok(None) };

        let cache_key =
            format!("tmdb-{:016x}", xxhash_rust::xxh3::xxh3_64(poster.as_bytes()));
        let lock = {
            let mut map = self.inflight.lock().unwrap();
            map.entry(cache_key.clone()).or_default().clone()
        };
        let _guard = lock.lock().await;
        let cache_path = self.dir.join(&cache_key);
        if let Ok(bytes) = std::fs::read(&cache_path) {
            return Ok(Some((bytes, "image/jpeg")));
        }
        let bytes = self.enricher.fetch_poster(&poster).await?;
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(&cache_path, &bytes)?;
        Ok(Some((bytes, "image/jpeg")))
    }
}

/// The artwork path recorded on any of the item's (or its children's)
/// sources, preferring connected mediahosts.
async fn find_artwork_source(
    registry: &Registry,
    item_id: &str,
) -> Result<Option<(String, String, String)>> {
    let rows = sqlx::query(
        "SELECT s.module_id, s.collection_id,
                json_extract(f.streams_json, '$.artwork') AS art
         FROM item_sources s
         JOIN files f ON (f.module_id, f.collection_id, f.path_rel)
                       = (s.module_id, s.collection_id, s.path_rel)
         WHERE (s.item_id = ?1
                OR s.item_id IN (SELECT id FROM items WHERE parent_id = ?1))
           AND art IS NOT NULL
         ORDER BY f.size DESC",
    )
    .bind(item_id)
    .fetch_all(registry.db())
    .await
    .context("artwork lookup")?;
    let row = rows
        .iter()
        .find(|r| registry.is_connected(&r.get::<String, _>("module_id")))
        .or(rows.first());
    Ok(row.map(|r| (r.get("module_id"), r.get("collection_id"), r.get("art"))))
}

/// Drain a whole (small) file through a lease in chunks.
async fn read_all(lease: crate::leases::Lease) -> Result<Vec<u8>> {
    const CHUNK: u64 = 1 << 20;
    const MAX: usize = 32 << 20;
    let mut out = Vec::new();
    loop {
        let mut stream = lease.read_range(out.len() as u64, CHUNK).into_inner();
        let mut got = 0u64;
        while let Some(chunk) = stream.recv().await {
            let bytes = chunk.map_err(|e| anyhow::anyhow!("lease read: {e}"))?;
            got += bytes.len() as u64;
            out.extend_from_slice(&bytes);
            anyhow::ensure!(out.len() <= MAX, "artwork file too large");
        }
        if got < CHUNK {
            return Ok(out);
        }
    }
}
