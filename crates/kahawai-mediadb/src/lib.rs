//! Media catalogue, metadata storage and library identities for the hub.
//! Runtime connections, provider requests, HTTP and playback remain in the hub.
//! Source observations, physical collection occurrences and provider evidence
//! remain separate from durable library items.
//! Each occurrence references exactly one library item. Its immutable identity is
//! media type + normalized title/year; albums and incomplete identities also
//! include the occurrence ID as a discriminator. The discriminator is historical
//! identity data, not a foreign key to a copy that might later be deleted.
//!
//! Identity corrections move copies between items in the same write transaction;
//! they never rename a key, merge IDs or redirect an old item. Returning to an
//! earlier key reuses its ID. A library item with no copies is archived by
//! definition: there is no flag, archive operation or resurrection bookkeeping.
//! Removing a namespace deletes its copies and assignments but retains library
//! items and provider records. Fresh imports use current evidence, never restore
//! deleted assignments, and give new singleton occurrences new library IDs.
//!
//! Library identity is global within a media type; browse visibility and copy
//! descriptions remain scoped to each library's collection membership/order.
//! The stored title labels the identity as first created; it is not a resolved
//! description snapshot. `library_item_record` reads that identity even when
//! archived; `library_item` requires an accessible copy. Users and watch state
//! belong to the separate hub database. Mediadb owns its own embedded, append-only
//! SQLx migration history; pending migrations finish before a Store is returned.
//! See the database module for migration and database ownership semantics.
//!
//! Physical identity is `(mediahost, collection, root, occurrence)`. A movie
//! occurrence is one whole file or one explicitly numbered release family;
//! episodes/tracks belong to their physical title/album directory. `media_parts`
//! assembles one rendition, never files selected across library copies.
//!
//! `files` stores the last base FileRecord, with a nullable probe for diagnostics
//! that arrive before any successful discovery. `source_facts` stores typed
//! protocol observations separately (not resolved copies of base fields). Its
//! protobuf bytes preserve every provider-independent discovery field. Consumers
//! inspect these through `SourceFact`, not untyped JSON or SQL payload decoding.
//! Hashes are eight-byte little-endian blobs so unsigned identities round-trip.
//!
//! A provider record is reusable evidence. An assignment selects identity as a
//! whole, supplements fill only absent descriptive fields, and clearing/changing
//! the assignment cascades its supplemental links. Neither operation edits source
//! observations. Library collection order chooses one copy before resolving its
//! description; metadata never leaks from copies outside that library.
//!
//! All writes go through Store operations and one serialized writer. There are no
//! repair-on-read paths, maintenance callbacks, or derived grouping projections.
//! Every committed collection item has a media entry with at least one probed
//! file: creation validates parts, and import/removal prunes empty entries and
//! occurrences before committing. Browse can rely on that invariant without
//! revisiting hundreds of thousands of physical source rows to list titles.
//! Only library items store normalized identity keys. Indexed copy references
//! serve browse visibility and derived archival; browsing pages identities before
//! reading descriptions, without GROUP BY or separate assigned/detected scans.
//! SQL foreign keys enforce physical ownership; the Rust operations additionally
//! enforce metadata type compatibility and shape-dependent invariants.
mod catalogue;
mod children;
pub use children::*;
mod database;
mod inspection;
mod library;
pub use inspection::{CatalogueStats, CollectionSummary};
mod enrichment;
mod metadata;
pub use enrichment::*;
mod occurrence;
mod subtitles;
mod types;
pub use subtitles::DownloadedSubtitle;

pub use catalogue::SourceFact;
pub use types::*;

use anyhow::{Context, Result, ensure};
use kahawai_sqlite::Database;
use unicode_normalization::UnicodeNormalization;

#[derive(Clone)]
pub struct Store {
    pub(crate) db: Database,
}

pub fn title_key(title: &str) -> String {
    title
        .nfc()
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
pub(crate) fn id() -> String {
    ulid::Ulid::generate().to_string()
}
pub(crate) fn integer(n: u64) -> Result<i64> {
    i64::try_from(n).context("catalogue integer exceeds database range")
}

impl Store {
    pub async fn close(self) {
        self.db.close().await;
    }
    pub async fn put_mediahost(&self, host: &str, name: &str) -> Result<()> {
        ensure!(!host.is_empty(), "empty mediahost ID");
        sqlx::query(
            "INSERT INTO mediahosts VALUES(?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name",
        )
        .bind(host)
        .bind(name)
        .execute(&self.db)
        .await?;
        Ok(())
    }
    pub async fn remove_collection(&self, collection: &str) -> Result<()> {
        sqlx::query("DELETE FROM collections WHERE id=?")
            .bind(collection)
            .execute(&self.db)
            .await?;
        Ok(())
    }
    pub async fn remove_mediahost(&self, host: &str) -> Result<()> {
        let mut tx = self.db.begin().await?;
        sqlx::query("DELETE FROM collections WHERE mediahost_id=?")
            .bind(host)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mediahosts WHERE id=?")
            .bind(host)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}
