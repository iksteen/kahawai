//! HUB-5: metadata providers behind a common trait, with the per-media-
//! type ordering declared as data. Adding a provider = one impl plus a
//! chain entry; the walker owns miss-recording and progress counting.
//!
//! Chains (first claim wins):
//!   anime  → anime (AniDB identity + AniList description) → tmdb → tvdb
//!   movies/series → tmdb → tvdb
//!   music  → musicbrainz
//! Local metadata (embedded tags) acts earlier, at resolution time —
//! it decides identity before enrichment ever runs (HUB-9, partial).

use anyhow::Result;
use sqlx::SqlitePool;

/// One item as the chain walker sees it. The `anime_*` fields carry the
/// selection context the anime chain needs (existing verified match,
/// pinned manual identity, current hash-verification state).
#[derive(Debug, Clone)]
pub struct ItemRef {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub year: Option<i64>,
    pub artist: Option<String>,
    /// Alternative identity from the parent directory name (movies).
    pub alt: Option<kahawai_core::names::MovieGuess>,
    pub existing: Option<(String, String)>,
    pub manual: bool,
    pub known_aid: Option<u32>,
    pub identified: bool,
}

pub enum Outcome {
    /// Identified and persisted, with this confidence ("auto" | "weak").
    Matched(&'static str),
    /// Already correctly identified — stop the chain, change nothing.
    Settled,
    /// Not this provider's item (or nothing verifiable found): next.
    Declined,
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    /// Identify + persist metadata for one item, or decline.
    async fn enrich(&self, db: &SqlitePool, item: &ItemRef) -> Result<Outcome>;
    /// End-of-run teardown (session logout etc.).
    async fn finish(&self) {}
}

/// The declared provider order per library media type.
pub fn chain_for(media_type: &str) -> &'static [&'static str] {
    match media_type {
        "anime" => &["anime", "tmdb", "tvdb"],
        "music" => &["musicbrainz"],
        _ => &["tmdb", "tvdb"],
    }
}

/// Providers instantiated for one enrichment run (credentials resolved,
/// sessions opened). Absent = unconfigured; the walker skips it.
#[derive(Default)]
pub struct ProviderSet {
    providers: Vec<Box<dyn Provider>>,
}

impl ProviderSet {
    pub fn add(&mut self, p: Box<dyn Provider>) {
        self.providers.push(p);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Provider> {
        self.providers.iter().find(|p| p.name() == name).map(|p| p.as_ref())
    }

    pub async fn finish(&self) {
        for p in &self.providers {
            p.finish().await;
        }
    }

    /// Walk the media type's chain for one item. Returns the outcome
    /// confidence, or None when every provider declined (the caller
    /// records the miss).
    pub async fn run_chain(
        &self,
        media_type: &str,
        db: &SqlitePool,
        item: &ItemRef,
    ) -> Option<&'static str> {
        for name in chain_for(media_type) {
            let Some(p) = self.get(name) else { continue };
            match p.enrich(db, item).await {
                Ok(Outcome::Matched(conf)) => return Some(conf),
                Ok(Outcome::Settled) => return Some("settled"),
                Ok(Outcome::Declined) => {}
                Err(e) => {
                    tracing::warn!(provider = name, title = %item.title,
                        error = format!("{e:#}"), "provider failed; trying next");
                }
            }
        }
        None
    }
}
