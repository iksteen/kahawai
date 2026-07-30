//! Local artwork serving (MH-4): cover/folder/poster images detected by
//! the scan, fetched through the source's read lease once and cached on
//! the hub. Albums and shows inherit artwork from their children's
//! directories (they have no sources of their own).
//!
//! # Sizes (HUB-12)
//!
//! A grid of 34-pixel thumbnails should not each pull a 600-pixel cover,
//! so the endpoint serves named sizes from [`SIZES`]. Names rather than
//! free-form pixel values: a client that can ask for any width can mint
//! unbounded cache entries, and the set of sizes the UI actually uses is
//! small and known here.
//!
//! Derivatives are made on FIRST REQUEST and then kept. That follows
//! OPS-6's reasoning rather than departing from it: artwork is fetched
//! during a grid scroll, so a miss is a blank card the user watches
//! appear, and the resize itself is one decode plus one encode of a file
//! already on local disk.
//!
//! The one thing that IS evicted, at startup, is a derivative whose size
//! no longer exists — see [`Artwork::sweep_stale_sizes`]. That is not a
//! janitor and not a quota: nothing is removed because the cache grew,
//! only because the size it was made for is gone from the list above.
//! Each variant's directory is named for its size in pixels, so editing
//! a number in [`SIZES`] is itself what makes the old ones stale.

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

/// The sizes a client may ask for, as `name → longest edge in pixels`.
///
/// Add, rename or re-number freely: the next startup drops whatever no
/// longer appears here, and the first request for a new entry builds it.
/// Values are generous enough for a 2× display of the element that uses
/// them — `thumb` for the 34px search-result rows, `card` for the 220px
/// library grid. Asking for no size still serves the original.
pub const SIZES: &[(&str, u32)] = &[("thumb", 96), ("card", 480)];

/// Directory holding one size's derivatives. The pixel count is IN the
/// name, so changing a size in [`SIZES`] renames the directory and the
/// old one becomes stale by construction rather than by remembering to
/// invalidate anything.
fn variant_dir(name: &str, px: u32) -> String {
    format!("size-{name}-{px}")
}

impl Artwork {
    pub fn new(dir: PathBuf, enricher: Arc<crate::enrich::Enricher>) -> Self {
        let art = Self {
            dir,
            enricher,
            inflight: Default::default(),
        };
        art.sweep_stale_sizes();
        art
    }

    /// Drop derivatives that can no longer be reached: a whole size that
    /// has left [`SIZES`], and within the sizes that remain, any copy
    /// whose ORIGINAL is gone.
    ///
    /// Runs at startup because that is the only moment either can have
    /// changed — [`SIZES`] is a compile-time constant, and an original
    /// only disappears when something outside this process removes it.
    /// Neither is a quota: nothing here is dropped for being large, only
    /// for being unreachable. A derivative is named after the original's
    /// cache key, so "is the original still there" is one `exists()`.
    ///
    /// Best effort throughout. An unreadable cache directory costs disk,
    /// never correctness, and originals are never touched.
    fn sweep_stale_sizes(&self) {
        let keep: Vec<String> = SIZES
            .iter()
            .map(|(name, px)| variant_dir(name, *px))
            .collect();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("size-") {
                continue;
            }
            if !keep.contains(&name) {
                match std::fs::remove_dir_all(entry.path()) {
                    Ok(()) => {
                        tracing::info!(size = %name, "artwork size retired; cache dropped")
                    }
                    Err(e) => {
                        tracing::warn!(size = %name, error = %e, "could not drop retired size")
                    }
                }
                continue;
            }
            let Ok(copies) = std::fs::read_dir(entry.path()) else {
                continue;
            };
            let mut orphans = 0u32;
            for copy in copies.flatten() {
                // Same file name as the original it was made from, one
                // directory up. Gone means this can never be served.
                if self.dir.join(copy.file_name()).exists() {
                    continue;
                }
                if std::fs::remove_file(copy.path()).is_ok() {
                    orphans += 1;
                }
            }
            if orphans > 0 {
                tracing::info!(size = %name, orphans, "dropped resized copies with no original");
            }
        }
    }

    /// Artwork at a named size, built on first request and kept.
    ///
    /// An unknown name serves the ORIGINAL rather than failing. Sizes
    /// come and go from [`SIZES`], and a page loaded before one was
    /// retired would otherwise show broken images until reloaded; the
    /// wrong number of bytes beats a hole in the grid.
    pub async fn get_at(
        &self,
        registry: &Registry,
        sessions: &Sessions,
        item_id: &str,
        size: Option<&str>,
    ) -> Result<Option<(Vec<u8>, &'static str)>> {
        let Some((bytes, ctype, key)) = self.original(registry, sessions, item_id).await? else {
            return Ok(None);
        };
        let Some(name) = size else {
            return Ok(Some((bytes, ctype)));
        };
        let Some((_, px)) = SIZES.iter().find(|(n, _)| *n == name) else {
            tracing::debug!(size = %name, "unknown artwork size; serving the original");
            return Ok(Some((bytes, ctype)));
        };

        let dir = self.dir.join(variant_dir(name, *px));
        let path = dir.join(&key);
        if let Ok(small) = std::fs::read(&path) {
            return Ok(Some((small, "image/jpeg")));
        }
        // Decoding and re-encoding is CPU work on a runtime thread that
        // is otherwise serving requests, so it goes to the blocking pool.
        let px = *px;
        let small = tokio::task::spawn_blocking(move || resize_to(&bytes, px)).await??;
        std::fs::create_dir_all(&dir)?;
        // Written next to the read above, not atomically: two requests
        // racing write identical bytes, and a torn file is re-made on the
        // next miss.
        let _ = std::fs::write(&path, &small);
        Ok(Some((small, "image/jpeg")))
    }

    /// Image bytes + content type for the poster the provider chain
    /// resolved, or None when the item has none. Local artwork is one
    /// provider's answer among the rest (HUB-9), so which source wins is
    /// the chain's ranking, not a rule in here.
    pub async fn get(
        &self,
        registry: &Registry,
        sessions: &Sessions,
        item_id: &str,
    ) -> Result<Option<(Vec<u8>, &'static str)>> {
        Ok(self
            .original(registry, sessions, item_id)
            .await?
            .map(|(b, c, _)| (b, c)))
    }

    /// As [`Artwork::get`], plus the cache key the bytes are stored
    /// under — which is what a derivative is named after, so a resized
    /// copy is tied to the exact original it came from.
    async fn original(
        &self,
        registry: &Registry,
        sessions: &Sessions,
        item_id: &str,
    ) -> Result<Option<(Vec<u8>, &'static str, String)>> {
        let Some(poster) = resolved_poster(registry, item_id).await? else {
            return Ok(None);
        };
        if !poster.starts_with(LOCAL) {
            return self.remote_poster(&poster).await;
        }
        // The answer names the file; which mediahost serves it is decided
        // now, since a collection can be reachable through more than one
        // and only the connected ones can answer a lease.
        let Some((module_id, collection_id, art_rel)) =
            find_artwork_source(registry, item_id).await?
        else {
            return Ok(None);
        };

        let ctype = match art_rel
            .rsplit('.')
            .next()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
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
            return Ok(Some((bytes, ctype, cache_key)));
        }
        let lease = sessions
            .open_lease(registry, &module_id, &collection_id, &art_rel)
            .await?;
        let bytes = read_all(lease).await?;
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(&cache_path, &bytes)?;
        Ok(Some((bytes, ctype, cache_key)))
    }
}

impl Artwork {
    /// A poster held by the provider itself, cached like local artwork.
    async fn remote_poster(&self, poster: &str) -> Result<Option<(Vec<u8>, &'static str, String)>> {
        let cache_key = format!(
            "tmdb-{:016x}",
            xxhash_rust::xxh3::xxh3_64(poster.as_bytes())
        );
        let lock = {
            let mut map = self.inflight.lock().unwrap();
            map.entry(cache_key.clone()).or_default().clone()
        };
        let _guard = lock.lock().await;
        let cache_path = self.dir.join(&cache_key);
        if let Ok(bytes) = std::fs::read(&cache_path) {
            return Ok(Some((bytes, "image/jpeg", cache_key)));
        }
        let bytes = self.enricher.fetch_poster(poster).await?;
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(&cache_path, &bytes)?;
        Ok(Some((bytes, "image/jpeg", cache_key)))
    }
}

/// Fit an image inside `px` on its longest edge, as JPEG.
///
/// Always JPEG, whatever went in: these are small, lossy is invisible at
/// this scale, and one output type means one content type to serve. An
/// image already within the box is re-encoded rather than passed through,
/// so a "size" is always the size it says — a 40px cover asked for at
/// `card` does not come back claiming to be 480.
fn resize_to(bytes: &[u8], px: u32) -> Result<Vec<u8>> {
    let img = image::load_from_memory(bytes).context("decoding artwork")?;
    let small = img.resize(px, px, image::imageops::FilterType::Lanczos3);
    let mut out = std::io::Cursor::new(Vec::new());
    small
        .to_rgb8()
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .context("encoding resized artwork")?;
    Ok(out.into_inner())
}

/// Scheme marking a `poster_path` that names a file in a collection
/// rather than a provider's own URL.
pub const LOCAL: &str = "local://";

/// The poster the chain resolved for this item, its parent's as the
/// fallback for episodes.
async fn resolved_poster(registry: &Registry, item_id: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT m.poster_path FROM items i
         JOIN resolved_metadata m ON m.item_id IN (i.id, i.parent_id)
         WHERE i.id = ? AND m.poster_path IS NOT NULL
         ORDER BY m.item_id = i.id DESC LIMIT 1",
    )
    .bind(item_id)
    .fetch_optional(registry.db())
    .await?
    .flatten())
}

/// The artwork path recorded on any of the item's (or its children's)
/// sources, preferring connected mediahosts.
pub(crate) async fn find_artwork_source(
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
