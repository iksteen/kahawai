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
//! Album Artist collages follow the same rule but are built by background
//! enrichment before their version is exposed. Their `.collage` manifest is
//! the durable readiness claim and records the exact newest-four album set;
//! the generated JPEG is keyed by both library and artist so a shared artist
//! cannot expose a cover across a library grant boundary. Rebuilding costs at
//! most four cached cover reads plus one composition and the named resizes;
//! required-time latency stays one local cache read.
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
use image::GenericImageView;
use serde::{Deserialize, Serialize};

use crate::registry::Registry;
use crate::sessions::Sessions;

pub struct Artwork {
    dir: PathBuf,
    enricher: Arc<crate::enrich::Enricher>,
    /// Per-key locks so concurrent requests fetch once.
    inflight: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

#[derive(Deserialize, Serialize)]
struct ArtistCollageManifest {
    library: String,
    artist_key: String,
    revision: String,
    albums: Vec<String>,
}

struct ArtistCollageAlbum {
    id: String,
    art_version: Option<i64>,
    poster: String,
}

const ARTIST_COLLAGE_SCHEMA: &str = "artist-collage-v1";
const ARTIST_COLLAGE_EDGE: u32 = 480;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CacheWrite {
    /// A viewer already has the original bytes. A failed cache write must not
    /// turn a usable response into a 500.
    BestEffort,
    /// Background prefetch may advertise a portrait only after every public
    /// derivative is actually durable.
    Required,
}

/// The sizes a client may ask for, as `name → longest edge in pixels`.
///
/// Add, rename or re-number freely: the next startup drops whatever no
/// longer appears here, and the first request for a new entry builds it.
/// `card` and `card1x` are one poster at two densities, offered together
/// in a `srcset` so the client picks: a 1× display showing a 128px shelf
/// card was being sent 320×480 and scaling it down, 6× the pixels it had
/// any use for. `card` stays sized for 2×; `card1x` covers the widest 1×
/// use, which is the library grid rather than the narrower shelves.
///
/// `thumb` is for the search-result rows and is asked for on its own.
/// Asking for no size still serves the original.
pub const SIZES: &[(&str, u32)] = &[("thumb", 96), ("card1x", 320), ("card", 480)];

/// Directory holding one size's derivatives. The pixel count is IN the
/// name, so changing a size in [`SIZES`] renames the directory and the
/// old one becomes stale by construction rather than by remembering to
/// invalidate anything.
fn variant_dir(name: &str, px: u32) -> String {
    format!("size-{name}-{px}")
}

fn remote_cache_key(url: &str) -> String {
    // Keep every existing provider poster at its historical key. Artist
    // portraits are new and get an honest namespace from day one.
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase));
    let prefix = match host.as_deref() {
        Some(host) if host == "fanart.tv" || host.ends_with(".fanart.tv") => "fanart",
        Some(host) if host == "theaudiodb.com" || host.ends_with(".theaudiodb.com") => "theaudiodb",
        _ => "tmdb",
    };
    format!(
        "{prefix}-{:016x}",
        xxhash_rust::xxh3::xxh3_64(url.as_bytes())
    )
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
    /// Write a cache file so that a reader never sees a partial one.
    ///
    /// A straight write to the final path is durable only if nothing interrupts
    /// it: kill the hub or fill the disk halfway through an original and the
    /// file exists, reads back as a complete image for ever, and is not a
    /// derivative so the sweep below never collects it. Decoding then fails on
    /// every request, which this branch answers with a fixed 500 — a permanent
    /// failure for that item with no path back but deleting the file by hand.
    fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    }

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
            // A miss sentinel whose hour is up, and any `.part` left by a
            // write that was interrupted. Neither is a size directory, so
            // without this they accumulate one file per coverless poster for
            // the life of the install.
            if name.ends_with(".miss") || name.ends_with(".part") {
                let stale = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|m| m.elapsed().ok())
                    .is_some_and(|age| age > MISS_TTL);
                if stale {
                    let _ = std::fs::remove_file(entry.path());
                }
                continue;
            }
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

    async fn at_from_original(
        &self,
        bytes: Vec<u8>,
        ctype: &'static str,
        key: String,
        size: Option<&str>,
        cache_write: CacheWrite,
    ) -> Result<(Vec<u8>, &'static str)> {
        let Some(name) = size else {
            return Ok((bytes, ctype));
        };
        let Some((_, px)) = SIZES.iter().find(|(n, _)| *n == name) else {
            tracing::debug!(size = %name, "unknown artwork size; serving the original");
            return Ok((bytes, ctype));
        };
        let dir = self.dir.join(variant_dir(name, *px));
        let path = dir.join(&key);
        if let Ok(small) = std::fs::read(&path) {
            return Ok((small, "image/jpeg"));
        }
        // Decoding and re-encoding is CPU work on a runtime thread that
        // is otherwise serving requests, so it goes to the blocking pool.
        let px = *px;
        let small = tokio::task::spawn_blocking(move || resize_to(&bytes, px)).await??;
        let persisted =
            std::fs::create_dir_all(&dir).and_then(|()| Self::write_atomic(&path, &small));
        if let Err(error) = persisted {
            if cache_write == CacheWrite::Required {
                return Err(error.into());
            }
            tracing::warn!(path = %path.display(), error = %error,
                "resized artwork served but could not be cached");
        }
        Ok((small, "image/jpeg"))
    }

    /// Serve an Album Artist portrait strictly from the cache populated by
    /// enrichment. Browsing must never turn into provider traffic.
    pub(crate) async fn get_cached_remote_at(
        &self,
        url: &str,
        size: Option<&str>,
    ) -> Result<Option<(Vec<u8>, &'static str)>> {
        let key = remote_cache_key(url);
        let path = size
            .and_then(|name| {
                SIZES
                    .iter()
                    .find(|(candidate, _)| *candidate == name)
                    .map(|(name, px)| self.dir.join(variant_dir(name, *px)).join(&key))
            })
            .unwrap_or_else(|| self.dir.join(&key));
        Ok(std::fs::read(path).ok().map(|bytes| (bytes, "image/jpeg")))
    }

    fn read_artist_collage_manifest(
        &self,
        library: &str,
        artist_key: &str,
    ) -> Option<ArtistCollageManifest> {
        let manifest: ArtistCollageManifest = serde_json::from_slice(
            &std::fs::read(artist_collage_manifest_path(&self.dir, library, artist_key)).ok()?,
        )
        .ok()?;
        if manifest.library != library
            || manifest.artist_key != artist_key
            || !self.cache_key_complete(&artist_collage_cache_key(library, artist_key))
        {
            return None;
        }
        Some(manifest)
    }

    fn cache_key_complete(&self, key: &str) -> bool {
        let present = |path: &std::path::Path| {
            std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() > 0)
        };
        present(&self.dir.join(key))
            && SIZES
                .iter()
                .all(|(name, px)| present(&self.dir.join(variant_dir(name, *px)).join(key)))
    }

    /// Whether the durable promise behind a ready artist row still holds.
    /// Atomic writes guarantee that a file created by this process is whole;
    /// non-empty existence catches deletion, incomplete restores and manual
    /// damage without decoding every portrait on every enrichment pass.
    pub(crate) fn remote_cache_complete(&self, url: &str) -> bool {
        let key = remote_cache_key(url);
        self.cache_key_complete(&key)
    }

    /// Fetch an artist portrait and materialise every public size before its
    /// database row is made visible. An overview request is consequently a
    /// local cache read, including on its first visit.
    pub(crate) async fn prefetch_remote(&self, url: &str) -> Result<bool> {
        let Some((bytes, ctype, key)) = self.remote_poster(url).await? else {
            return Ok(false);
        };
        for (name, _) in SIZES {
            self.at_from_original(
                bytes.clone(),
                ctype,
                key.clone(),
                Some(name),
                CacheWrite::Required,
            )
            .await?;
        }
        Ok(true)
    }
}

impl Artwork {
    /// `remote_poster` for the integration test that counts provider requests.
    /// The real caller reaches it through `original`, which needs a registry, a
    /// mediahost and a resolved item — none of which say anything about whether
    /// a miss is remembered.
    #[doc(hidden)]
    pub async fn remote_poster_for_test(
        &self,
        poster: &str,
    ) -> Result<Option<(Vec<u8>, &'static str, String)>> {
        self.remote_poster(poster).await
    }

    /// A poster held by the provider itself, cached like local artwork.
    async fn remote_poster(&self, poster: &str) -> Result<Option<(Vec<u8>, &'static str, String)>> {
        let cache_key = remote_cache_key(poster);
        let lock = {
            let mut map = self.inflight.lock().unwrap();
            map.entry(cache_key.clone()).or_default().clone()
        };
        let _guard = lock.lock().await;
        let cache_path = self.dir.join(&cache_key);
        if let Ok(bytes) = std::fs::read(&cache_path) {
            return Ok(Some((bytes, "image/jpeg", cache_key)));
        }
        // A miss is remembered for an hour, as an empty file beside the real
        // ones. Nothing was written before, on the reasoning that an upload
        // later should be picked up with nothing to invalidate — true, and it
        // made every request for a coverless release an outbound provider
        // fetch. Those go through the per-host gate one at a time, spaced
        // seconds apart, so a shelf of coverless records could hold that queue
        // at saturation for as long as somebody kept asking: enrichment for the
        // whole hub starves behind it, and on the stricter providers it walks
        // towards the ban the gate exists to avoid.
        //
        // An hour, because the sentinel expires: the upload is still picked up
        // without anybody invalidating anything, just not within the minute.
        let miss_path = self.dir.join(format!("{cache_key}.miss"));
        if let Ok(meta) = std::fs::metadata(&miss_path)
            && meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age < MISS_TTL)
        {
            return Ok(None);
        }
        let Some(bytes) = self.enricher.fetch_poster(poster).await? else {
            std::fs::create_dir_all(&self.dir)?;
            // Best effort: failing to remember a miss costs a provider request,
            // not correctness.
            let _ = std::fs::write(&miss_path, []);
            return Ok(None);
        };
        std::fs::create_dir_all(&self.dir)?;
        Self::write_atomic(&cache_path, &bytes)?;
        Ok(Some((bytes, "image/jpeg", cache_key)))
    }
}

fn artist_collage_cache_key(library: &str, artist_key: &str) -> String {
    format!(
        "artist-collage-{:016x}",
        xxhash_rust::xxh3::xxh3_64(format!("{library}\n{artist_key}").as_bytes())
    )
}

fn artist_collage_manifest_path(dir: &std::path::Path, library: &str, artist_key: &str) -> PathBuf {
    dir.join(format!(
        "{}.collage",
        artist_collage_cache_key(library, artist_key)
    ))
}

fn artist_collage_revision(
    library: &str,
    artist_key: &str,
    albums: &[ArtistCollageAlbum],
) -> String {
    let mut material = format!("{ARTIST_COLLAGE_SCHEMA}\n{library}\n{artist_key}");
    for album in albums {
        material.push_str(&format!(
            "\n{}\n{}\n{}",
            album.id,
            album.art_version.unwrap_or_default(),
            album.poster
        ));
    }
    format!(
        "{ARTIST_COLLAGE_SCHEMA}-{:016x}",
        xxhash_rust::xxh3::xxh3_64(material.as_bytes())
    )
}

fn compose_artist_collage(covers: &[Vec<u8>]) -> Result<Vec<u8>> {
    anyhow::ensure!(!covers.is_empty(), "an artist collage needs an album cover");
    let images = covers
        .iter()
        .take(4)
        .map(|cover| image::load_from_memory(cover).context("decoding album cover"))
        .collect::<Result<Vec<_>>>()?;
    let mut collage = image::RgbImage::new(ARTIST_COLLAGE_EDGE, ARTIST_COLLAGE_EDGE);
    let half = ARTIST_COLLAGE_EDGE / 2;
    let rectangles = match images.len() {
        1 => vec![(0, 0, ARTIST_COLLAGE_EDGE, ARTIST_COLLAGE_EDGE)],
        2 => vec![
            (0, 0, half, ARTIST_COLLAGE_EDGE),
            (half, 0, half, ARTIST_COLLAGE_EDGE),
        ],
        3 => vec![
            (0, 0, half, ARTIST_COLLAGE_EDGE),
            (half, 0, half, half),
            (half, half, half, half),
        ],
        _ => vec![
            (0, 0, half, half),
            (half, 0, half, half),
            (0, half, half, half),
            (half, half, half, half),
        ],
    };
    for (image, (x, y, width, height)) in images.iter().zip(rectangles) {
        let tile = crop_fill(image, width, height);
        image::imageops::replace(&mut collage, &tile, i64::from(x), i64::from(y));
    }
    let mut out = std::io::Cursor::new(Vec::new());
    collage
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .context("encoding artist collage")?;
    Ok(out.into_inner())
}

fn crop_fill(image: &image::DynamicImage, width: u32, height: u32) -> image::RgbImage {
    let (source_width, source_height) = image.dimensions();
    let scale = f64::max(
        f64::from(width) / f64::from(source_width),
        f64::from(height) / f64::from(source_height),
    );
    let resized_width = (f64::from(source_width) * scale).ceil() as u32;
    let resized_height = (f64::from(source_height) * scale).ceil() as u32;
    let resized = image.resize_exact(
        resized_width,
        resized_height,
        image::imageops::FilterType::Lanczos3,
    );
    resized
        .crop_imm(
            (resized_width - width) / 2,
            (resized_height - height) / 2,
            width,
            height,
        )
        .to_rgb8()
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

/// How long a provider's "no poster" answer is remembered. Long enough that a
/// browse cannot hammer the outbound gate, short enough that an artwork upload
/// appears without anybody clearing a cache.
const MISS_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Scheme marking a `poster_path` that names a file in a collection
/// rather than a provider's own URL.
pub const LOCAL: &str = "local://";

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

impl Artwork {
    /// Mediadb preview: the caller supplies an authorized copy and a URL from
    /// its stored metadata. Local paths must still be advertised by that copy.
    pub(crate) async fn catalogue_at(
        &self,
        registry: &Registry,
        sessions: &Sessions,
        input: &kahawai_mediadb::EnrichmentInput,
        poster: &str,
        size: Option<&str>,
    ) -> Result<Option<(Vec<u8>, &'static str)>> {
        let original = if let Some(local) = poster.strip_prefix("local://") {
            let (root, path) = local
                .split_once('/')
                .context("invalid local artwork address")?;
            let source = input
                .sources
                .iter()
                .find(|s| {
                    s.root_token == root
                        && s.media.as_ref().and_then(|m| m.artwork.as_deref()) == Some(path)
                })
                .context("artwork is not advertised by this copy")?;
            let key = format!(
                "{:016x}",
                xxhash_rust::xxh3::xxh3_64(
                    format!(
                        "{}\n{}\n{}\n{}\n{:?}",
                        input.mediahost_id, input.remote_id, root, path, source.media
                    )
                    .as_bytes()
                )
            );
            let cache = self.dir.join(&key);
            let bytes = match std::fs::read(&cache) {
                Ok(bytes) => bytes,
                Err(_) => {
                    let lease = sessions
                        .open_lease(
                            registry,
                            &input.mediahost_id,
                            &input.remote_id,
                            root,
                            path,
                            crate::sessions::Reader::Sweep,
                        )
                        .await?;
                    let bytes = read_all(lease).await?;
                    std::fs::create_dir_all(&self.dir)?;
                    Self::write_atomic(&cache, &bytes)?;
                    bytes
                }
            };
            let mime = match path
                .rsplit('.')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str()
            {
                "png" => "image/png",
                "webp" => "image/webp",
                _ => "image/jpeg",
            };
            Some((bytes, mime, key))
        } else {
            self.remote_poster(poster).await?
        };
        let Some((bytes, mime, key)) = original else {
            return Ok(None);
        };
        Ok(Some(
            self.at_from_original(bytes, mime, key, size, CacheWrite::BestEffort)
                .await?,
        ))
    }
}
impl Artwork {
    pub(crate) async fn prefetch_catalogue_collage(
        &self,
        registry: &Registry,
        sessions: &Sessions,
        item: &str,
        library: &str,
    ) -> Result<()> {
        let input = registry.catalogue().enrichment_input(item).await?;
        let artist = input.artist.as_deref().unwrap_or_default();
        let artist = kahawai_mediadb::title_key(artist);
        let mut donors = Vec::new();
        let mut covers = Vec::new();
        for donor in registry.catalogue().artist_donors(item, library).await? {
            if !donor.poster.starts_with("local://") && !self.remote_cache_complete(&donor.poster) {
                continue;
            }
            let copy = registry.catalogue().enrichment_input(&donor.id).await?;
            let Some((bytes, _)) = self
                .catalogue_at(registry, sessions, &copy, &donor.poster, Some("card"))
                .await?
            else {
                continue;
            };
            covers.push(bytes);
            donors.push(ArtistCollageAlbum {
                id: donor.id,
                art_version: Some(donor.revision),
                poster: donor.poster,
            });
            if covers.len() == 4 {
                break;
            }
        }
        if covers.is_empty() {
            return Ok(());
        }
        let revision = artist_collage_revision(library, &artist, &donors);
        if self
            .read_artist_collage_manifest(library, &artist)
            .is_some_and(|m| m.revision == revision)
        {
            return Ok(());
        }
        let key = artist_collage_cache_key(library, &artist);
        let bytes = tokio::task::spawn_blocking(move || compose_artist_collage(&covers)).await??;
        Self::write_atomic(&self.dir.join(&key), &bytes)?;
        for (size, _) in SIZES {
            self.at_from_original(
                bytes.clone(),
                "image/jpeg",
                key.clone(),
                Some(size),
                CacheWrite::Required,
            )
            .await?;
        }
        let manifest = ArtistCollageManifest {
            library: library.into(),
            artist_key: artist.clone(),
            revision,
            albums: donors.into_iter().map(|d| d.id).collect(),
        };
        Self::write_atomic(
            &artist_collage_manifest_path(&self.dir, library, &artist),
            &serde_json::to_vec(&manifest)?,
        )?;
        Ok(())
    }
    pub(crate) async fn catalogue_artist_at(
        &self,
        registry: &Registry,
        item: &str,
        library: &str,
    ) -> Result<Option<(Vec<u8>, &'static str)>> {
        if !registry
            .catalogue()
            .copy_libraries(item)
            .await?
            .iter()
            .any(|id| id == library)
        {
            return Ok(None);
        }
        for url in registry.catalogue().artist_artwork(item, library).await? {
            if let Some(image) = self.get_cached_remote_at(&url, Some("card")).await? {
                return Ok(Some(image));
            }
        }
        let input = registry.catalogue().enrichment_input(item).await?;
        let artist = kahawai_mediadb::title_key(input.artist.as_deref().unwrap_or_default());
        let Some(manifest) = self.read_artist_collage_manifest(library, &artist) else {
            return Ok(None);
        };
        let available = registry.catalogue().artist_donors(item, library).await?;
        let mut donors = Vec::new();
        for id in &manifest.albums {
            let Some(d) = available.iter().find(|d| &d.id == id) else {
                return Ok(None);
            };
            donors.push(ArtistCollageAlbum {
                id: d.id.clone(),
                art_version: Some(d.revision),
                poster: d.poster.clone(),
            });
        }
        if artist_collage_revision(library, &artist, &donors) != manifest.revision {
            return Ok(None);
        }
        let key = artist_collage_cache_key(library, &artist);
        Ok(
            std::fs::read(self.dir.join(variant_dir("card", 480)).join(key))
                .ok()
                .map(|bytes| (bytes, "image/jpeg")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_png(color: [u8; 3]) -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(16, 16, image::Rgb(color))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn assert_near(pixel: image::Rgb<u8>, expected: [u8; 3]) {
        for (actual, expected) in pixel.0.into_iter().zip(expected) {
            assert!(actual.abs_diff(expected) < 10, "{pixel:?} != {expected:?}");
        }
    }

    #[test]
    fn artist_collage_layouts_are_stable() {
        let red = solid_png([240, 0, 0]);
        let green = solid_png([0, 240, 0]);
        let blue = solid_png([0, 0, 240]);
        let yellow = solid_png([240, 240, 0]);
        let cases = [
            (vec![red.clone()], vec![((240, 240), [240, 0, 0])]),
            (
                vec![red.clone(), green.clone()],
                vec![((120, 240), [240, 0, 0]), ((360, 240), [0, 240, 0])],
            ),
            (
                vec![red.clone(), green.clone(), blue.clone()],
                vec![
                    ((120, 240), [240, 0, 0]),
                    ((360, 120), [0, 240, 0]),
                    ((360, 360), [0, 0, 240]),
                ],
            ),
            (
                vec![red, green, blue, yellow],
                vec![
                    ((120, 120), [240, 0, 0]),
                    ((360, 120), [0, 240, 0]),
                    ((120, 360), [0, 0, 240]),
                    ((360, 360), [240, 240, 0]),
                ],
            ),
        ];
        for (covers, samples) in cases {
            let image = image::load_from_memory(&compose_artist_collage(&covers).unwrap())
                .unwrap()
                .to_rgb8();
            assert_eq!(image.dimensions(), (480, 480));
            for ((x, y), expected) in samples {
                assert_near(*image.get_pixel(x, y), expected);
            }
        }
    }

    #[test]
    fn artist_provider_caches_have_distinct_namespaces() {
        assert!(remote_cache_key("https://assets.fanart.tv/a.jpg").starts_with("fanart-"));
        assert!(remote_cache_key("https://r2.theaudiodb.com/a.jpg").starts_with("theaudiodb-"));
    }

    #[tokio::test]
    async fn artist_portraits_are_served_only_from_prefetched_files() {
        let tmp = tempfile::tempdir().unwrap();
        let enricher = Arc::new(crate::enrich::Enricher::new(tmp.path().to_path_buf()));
        let artwork = Artwork::new(tmp.path().to_path_buf(), enricher);
        let url = "https://assets.fanart.tv/fanart/music/artist/portrait.jpg";
        let key = remote_cache_key(url);

        assert!(!artwork.remote_cache_complete(url));
        assert!(
            artwork
                .get_cached_remote_at(url, Some("card"))
                .await
                .unwrap()
                .is_none()
        );

        let (name, px) = SIZES.iter().find(|(name, _)| *name == "card").unwrap();
        let path = tmp.path().join(variant_dir(name, *px)).join(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"prefetched").unwrap();
        assert!(
            !artwork.remote_cache_complete(url),
            "one derivative must not satisfy the durable portrait promise"
        );

        let (bytes, content_type) = artwork
            .get_cached_remote_at(url, Some("card"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bytes, b"prefetched");
        assert_eq!(content_type, "image/jpeg");
    }

    #[test]
    fn a_ready_portrait_requires_its_original_and_every_named_size() {
        let tmp = tempfile::tempdir().unwrap();
        let enricher = Arc::new(crate::enrich::Enricher::new(tmp.path().to_path_buf()));
        let artwork = Artwork::new(tmp.path().to_path_buf(), enricher);
        let url = "https://assets.fanart.tv/fanart/music/artist/complete.jpg";
        let key = remote_cache_key(url);
        std::fs::write(tmp.path().join(&key), b"original").unwrap();
        for (name, px) in SIZES {
            let path = tmp.path().join(variant_dir(name, *px)).join(&key);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"derivative").unwrap();
        }
        assert!(artwork.remote_cache_complete(url));

        std::fs::write(
            tmp.path()
                .join(variant_dir(SIZES[0].0, SIZES[0].1))
                .join(&key),
            [],
        )
        .unwrap();
        assert!(!artwork.remote_cache_complete(url));
    }

    #[tokio::test]
    async fn viewer_resize_survives_a_derivative_cache_write_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let blocked = tmp.path().join("cache-is-a-file");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let enricher = Arc::new(crate::enrich::Enricher::new(tmp.path().to_path_buf()));
        let artwork = Artwork::new(blocked, enricher);
        let mut original = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut original, image::ImageFormat::Png)
            .unwrap();

        let served = artwork
            .at_from_original(
                original.get_ref().clone(),
                "image/png",
                "key".into(),
                Some("thumb"),
                CacheWrite::BestEffort,
            )
            .await
            .expect("a cache failure must not break viewer artwork");
        assert_eq!(served.1, "image/jpeg");
        assert!(!served.0.is_empty());

        assert!(
            artwork
                .at_from_original(
                    original.into_inner(),
                    "image/png",
                    "key".into(),
                    Some("thumb"),
                    CacheWrite::Required,
                )
                .await
                .is_err(),
            "prefetch must not publish a portrait whose derivative was not cached"
        );
    }
}
