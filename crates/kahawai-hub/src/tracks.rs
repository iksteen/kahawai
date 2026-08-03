//! Subtitle tracks as first-class rows — ONE keyspace for what used to
//! be three (`e{n}` embedded, `s{n}` sidecar, `d{id}` downloaded/OCR),
//! per the 2026-07-31 unification: "downloaded tracks, OCR tracks etc.
//! should just be extra tracks available to pick from."
//!
//! ## `subtitle_tracks` schema (authority for migration 0046)
//!
//! - `id` — THE subtitle key, everywhere: listing, serving
//!   (`/items/{id}/subtitles/{track_id}.vtt|.ass`), OCR generation,
//!   deletion, session selection, and per-item preference memory
//!   (`subs.track`). Stable across rescans (the stream upsert preserves
//!   ids while a stream keeps its position).
//! - `origin` — `embedded` (a stream inside the media container),
//!   `sidecar` (a file next to it: .srt/.ass/.vtt, or an .idx/.sub
//!   VobSub pair), `downloaded` (HUB-24 provider fetch), `ocr`
//!   (HUB-32c machine-read text), `raster` (HUB-32d: a styled script
//!   rendered to display sets, served item-level rather than through
//!   the session tap).
//! - `module_id`/`collection_id`/`path_rel` — the MEDIA file the track
//!   belongs to, for embedded/sidecar rows (NULL for hub-stored
//!   origins, which bind to the item). List filtering and the load
//!   path both key on the media file, so sidecar rows bind to it too;
//!   the sidecar file's own path lives in `external_subtitles`.
//! - `stream_index` — embedded: index into `streams_json.subtitles`;
//!   sidecar: index into `streams_json.external_subtitles`. The
//!   pipeline's tap files (`subs-e{n}.*`) and the burn plan still
//!   speak stream indexes; this column is the translation.
//! - `label` — provider release name, or the OCR row's legacy
//!   `ocr:{key}:{model}` tag (superseded by `derived_from`).
//! - `machine` — machine-generated AND IMPERFECT, user-visible as such
//!   (HUB-32c OCR). Not merely "derived": a HUB-32d `raster` row is
//!   generated too, but it renders the author's own typesetting
//!   exactly, so it is not flagged.
//! - `derived_from` — OCR rows point at the exact image-track row they
//!   were read from. Replaces string-parsing `label`, and is
//!   per-source correct where the old item+index tie was not.
//!
//! ## Delivery
//!
//! What a track means FOR THIS CLIENT is computed per request, never
//! stored: capability changes a track's delivery, not its existence
//! (owner decision: the API always lists; the UI disables). See
//! [`delivery`].

use anyhow::Result;
use serde::Serialize;
use sqlx::Row;

#[derive(Debug, Clone, Serialize)]
pub struct Track {
    pub id: i64,
    pub item_id: String,
    pub origin: String,
    #[serde(skip)]
    pub module_id: Option<String>,
    #[serde(skip)]
    pub collection_id: Option<String>,
    #[serde(skip)]
    pub path_rel: Option<String>,
    pub stream_index: Option<i64>,
    pub format: String,
    pub language: Option<String>,
    pub label: Option<String>,
    pub machine: bool,
    pub derived_from: Option<i64>,
}

/// How a track can be served to a given client — the tier ladder
/// (HUB-32a/b/c) expressed per track instead of per plan.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Plain timed text (VTT/live cue tap).
    Text,
    /// Faithful ASS, rendered by the client (JASSUB).
    Ass,
    /// Bitmap display sets composited by the client (session tap).
    Overlay,
    /// Composited into the picture by the encoder — selecting this
    /// track restarts the session with a forced video encode.
    Burn,
    /// Nothing can serve it to this client.
    None,
}

pub fn is_image_format(format: &str) -> bool {
    matches!(format, "pgs" | "vobsub" | "dvdsub")
}

/// HUB-32a/d: the user's ASS ladder, resolved for one user against one
/// fleet. `native` is not in the stored order — it is not a fallback,
/// and nothing a server does beats the client rendering the real
/// script itself.
pub async fn ass_policy_for_user(
    db: &sqlx::SqlitePool,
    user_id: &str,
    burn_capable: bool,
) -> kahawai_media::negotiate::AssPolicy {
    let stored = sqlx::query_scalar::<_, String>(
        "SELECT value FROM user_prefs
          WHERE user_id = ? AND scope = '' AND key = 'ass_order'",
    )
    .bind(user_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    let mut policy = kahawai_media::negotiate::AssPolicy {
        burn_capable,
        ..Default::default()
    };
    if let Some(v) = stored {
        // Always a full permutation — `parse_order` appends whatever
        // the stored value left out, so a corrupt or truncated
        // preference reorders rather than removes.
        policy.order = kahawai_media::negotiate::AssPolicy::parse_order(&v);
    }
    policy
}

/// The delivery matrix. `burn_capable` is the hub-side fact from
/// HUB-32b (the display-set timeline is readable where the encode
/// runs); `ass_render`/`graphics_overlay` come from the client
/// profile; `ass` is HUB-32a/d's ordered ladder.
pub fn delivery(
    track: &Track,
    ass_render: bool,
    graphics_overlay: bool,
    burn_capable: bool,
    ass: &kahawai_media::negotiate::AssPolicy,
) -> (Delivery, &'static str) {
    // HUB-32d: a rasterised script is display sets like any other, but
    // it is an ITEM-level artefact — no stream index, no session tap,
    // so it needs neither `burn_capable` nor an embedded origin.
    //
    // It offers itself only when the LADDER picked the overlay rung.
    // That is what stops the parent script and its raster both
    // claiming the same delivery — and they would send the client to
    // different URLs. Whichever rung won is the only one that reads as
    // playable, so the client's existing "best delivery wins" pick
    // lands on it without having to know the user's order.
    if track.origin == "raster" {
        let profile = kahawai_core::media::CapabilityProfile {
            ass_render,
            graphics_overlay,
            ..Default::default()
        };
        return match ass.choose(&profile) {
            kahawai_media::negotiate::AssTier::Overlay => (
                Delivery::Overlay,
                "rasterised — full typesetting, no encode",
            ),
            _ if !graphics_overlay => (Delivery::None, "needs an overlay-capable client"),
            _ => (
                Delivery::None,
                "another tier in your subtitle order comes first",
            ),
        };
    }
    if is_image_format(&track.format) {
        // Overlay needs the session tap, which only embedded streams
        // have (sidecar .idx/.sub is never in the pipeline).
        if graphics_overlay && track.origin == "embedded" {
            return (Delivery::Overlay, "");
        }
        if burn_capable {
            return (Delivery::Burn, "burned in — restarts with a video encode");
        }
        return (
            Delivery::None,
            "image subtitles need an overlay-capable client or a burn-capable source",
        );
    }
    match track.format.as_str() {
        // The ladder decides, and it is the SAME decision negotiation
        // makes — one `choose`, so a listing can never promise a tier
        // the session would not pick.
        "ass" | "ssa" => {
            let profile = kahawai_core::media::CapabilityProfile {
                ass_render,
                graphics_overlay,
                ..Default::default()
            };
            match ass.choose(&profile) {
                kahawai_media::negotiate::AssTier::Native => (Delivery::Ass, ""),
                kahawai_media::negotiate::AssTier::Burn => {
                    (Delivery::Burn, "burned in — restarts with a video encode")
                }
                // The overlay rung is served by the RASTER row, not by
                // this one — they are different URLs. The script's own
                // remaining form is the flattened VTT, which is honest
                // and still fetchable; the raster simply outranks it.
                kahawai_media::negotiate::AssTier::Overlay => (
                    Delivery::Text,
                    "flattened to VTT — the rasterised overlay is preferred",
                ),
                kahawai_media::negotiate::AssTier::Flatten => (Delivery::Text, "flattened to VTT"),
            }
        }
        _ => (Delivery::Text, ""),
    }
}

pub async fn get(db: &sqlx::SqlitePool, id: i64) -> Result<Option<Track>> {
    Ok(sqlx::query(
        "SELECT id, item_id, origin, module_id, collection_id, path_rel,
                stream_index, format, language, label, machine, derived_from
         FROM subtitle_tracks WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(db)
    .await?
    .map(row_to_track))
}

/// Every track of an item that is either hub-stored (downloaded/ocr) or
/// bound to the given source — the source `source_row` picked, so the
/// list matches what a session would actually play.
pub async fn for_item_source(
    db: &sqlx::SqlitePool,
    item_id: &str,
    module_id: &str,
    collection_id: &str,
    path_rel: &str,
) -> Result<Vec<Track>> {
    Ok(sqlx::query(
        "SELECT id, item_id, origin, module_id, collection_id, path_rel,
                stream_index, format, language, label, machine, derived_from
         FROM subtitle_tracks
         WHERE item_id = ?
           AND (module_id IS NULL
                OR (module_id, collection_id, path_rel) = (?, ?, ?))
         ORDER BY origin = 'embedded' DESC, origin = 'sidecar' DESC, id",
    )
    .bind(item_id)
    .bind(module_id)
    .bind(collection_id)
    .bind(path_rel)
    .fetch_all(db)
    .await?
    .into_iter()
    .map(row_to_track)
    .collect())
}

impl Track {
    /// The legacy notation the caches, extraction ladder and pipeline
    /// still speak internally: `e{n}` / `s{n}` / `d{row id}`.
    pub fn internal_key(&self) -> String {
        match self.origin.as_str() {
            "embedded" => format!("e{}", self.stream_index.unwrap_or(0)),
            "sidecar" => format!("s{}", self.stream_index.unwrap_or(0)),
            _ => format!("d{}", self.id),
        }
    }
}

fn row_to_track(r: sqlx::sqlite::SqliteRow) -> Track {
    Track {
        id: r.get("id"),
        item_id: r.get("item_id"),
        origin: r.get("origin"),
        module_id: r.get("module_id"),
        collection_id: r.get("collection_id"),
        path_rel: r.get("path_rel"),
        stream_index: r.get("stream_index"),
        format: r.get("format"),
        language: r.get("language"),
        label: r.get("label"),
        machine: r.get::<i64, _>("machine") != 0,
        derived_from: r.get("derived_from"),
    }
}

/// Sync the embedded and sidecar rows of one source with its freshly
/// probed streams — called inside `upsert_files`' transaction, right
/// after the `item_sources` upsert. Preserves ids while a stream keeps
/// its position (ON CONFLICT updates in place); deletes rows for
/// streams that vanished, and rows left under a previous item after a
/// re-match.
pub async fn sync_source_tracks(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    item_id: &str,
    module_id: &str,
    collection_id: &str,
    path_rel: &str,
    info: &kahawai_core::media::MediaInfo,
) -> Result<()> {
    // A re-match moved this source to another item: rows bound to the
    // source under any OTHER item are stale.
    sqlx::query(
        "DELETE FROM subtitle_tracks
         WHERE (module_id, collection_id, path_rel) = (?, ?, ?) AND item_id != ?",
    )
    .bind(module_id)
    .bind(collection_id)
    .bind(path_rel)
    .bind(item_id)
    .execute(&mut **tx)
    .await?;

    for (origin, count) in [
        ("embedded", info.subtitles.len() as i64),
        ("sidecar", info.external_subtitles.len() as i64),
    ] {
        sqlx::query(
            "DELETE FROM subtitle_tracks
             WHERE item_id = ? AND origin = ?
               AND (module_id, collection_id, path_rel) = (?, ?, ?)
               AND stream_index >= ?",
        )
        .bind(item_id)
        .bind(origin)
        .bind(module_id)
        .bind(collection_id)
        .bind(path_rel)
        .bind(count)
        .execute(&mut **tx)
        .await?;
    }

    let upsert = "INSERT INTO subtitle_tracks
            (item_id, origin, module_id, collection_id, path_rel,
             stream_index, format, language)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (item_id, module_id, collection_id, path_rel, origin, stream_index)
             WHERE origin IN ('embedded', 'sidecar')
         DO UPDATE SET format = excluded.format, language = excluded.language";
    for (i, s) in info.subtitles.iter().enumerate() {
        sqlx::query(upsert)
            .bind(item_id)
            .bind("embedded")
            .bind(module_id)
            .bind(collection_id)
            .bind(path_rel)
            .bind(i as i64)
            .bind(&s.format)
            .bind(&s.language)
            .execute(&mut **tx)
            .await?;
    }
    for (i, s) in info.external_subtitles.iter().enumerate() {
        sqlx::query(upsert)
            .bind(item_id)
            .bind("sidecar")
            .bind(module_id)
            .bind(collection_id)
            .bind(path_rel)
            .bind(i as i64)
            .bind(&s.format)
            .bind(&s.language)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// One-time startup pass: migrated OCR rows carry their linkage only as
/// the legacy `label` (`ocr:e{n}:{model}` / `ocr:s{n}:{model}`); point
/// `derived_from` at the matching stream row. Idempotent — rows with a
/// link are skipped, unmatchable labels stay NULL (their tracks still
/// serve; they just can't dedupe regeneration by parent).
pub async fn backfill_derived_from(db: &sqlx::SqlitePool) -> Result<()> {
    let rows = sqlx::query(
        "SELECT id, item_id, label FROM subtitle_tracks
         WHERE origin = 'ocr' AND derived_from IS NULL AND label LIKE 'ocr:%'",
    )
    .fetch_all(db)
    .await?;
    let mut fixed = 0usize;
    for r in &rows {
        let id: i64 = r.get("id");
        let item: String = r.get("item_id");
        let label: String = r.get("label");
        let Some(rest) = label.strip_prefix("ocr:") else {
            continue;
        };
        let Some(key) = rest.split(':').next() else {
            continue;
        };
        let (origin, idx) = match key.split_at(1) {
            ("e", n) => ("embedded", n),
            ("s", n) => ("sidecar", n),
            _ => continue,
        };
        let Ok(idx) = idx.parse::<i64>() else {
            continue;
        };
        let parent: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM subtitle_tracks
             WHERE item_id = ? AND origin = ? AND stream_index = ?
             ORDER BY id LIMIT 1",
        )
        .bind(&item)
        .bind(origin)
        .bind(idx)
        .fetch_optional(db)
        .await?;
        if let Some(parent) = parent {
            sqlx::query("UPDATE subtitle_tracks SET derived_from = ? WHERE id = ?")
                .bind(parent)
                .bind(id)
                .execute(db)
                .await?;
            fixed += 1;
        }
    }
    if fixed > 0 {
        tracing::info!(fixed, "OCR track lineage backfilled from legacy labels");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(format: &str, origin: &str) -> Track {
        Track {
            id: 1,
            item_id: "i".into(),
            origin: origin.into(),
            module_id: Some("m".into()),
            collection_id: Some("c".into()),
            path_rel: Some("f.mkv".into()),
            stream_index: Some(0),
            format: format.into(),
            language: None,
            label: None,
            machine: false,
            derived_from: None,
        }
    }

    /// HUB-32a/d's ladder, as a table. Client-native always wins when
    /// the client declares it; below that the USER's order decides, and
    /// a rung the fleet or the client cannot serve is skipped rather
    /// than stalling the ladder.
    #[test]
    fn the_ass_ladder_follows_the_users_order() {
        use kahawai_media::negotiate::{AssPolicy, AssTier};
        let t = track("ass", "embedded");
        let ladder = |order: &[AssTier], burn: bool, overlay: bool| AssPolicy {
            order: order.to_vec(),
            burn_capable: burn,
            overlay_ready: overlay,
        };
        let all = [AssTier::Flatten, AssTier::Overlay, AssTier::Burn];

        // Native outranks every order, including one that names burn
        // first: nothing a server does beats the real renderer.
        let d = delivery(&t, true, true, true, &ladder(&[AssTier::Burn], true, true));
        assert_eq!(d.0, Delivery::Ass);

        // The default order, no client-side ASS: flatten is first and
        // always possible, so it wins even with the others available.
        let d = delivery(&t, false, true, true, &ladder(&all, true, true));
        assert_eq!(d.0, Delivery::Text);

        // Reordered: overlay first, and it is ready. The SCRIPT's own
        // delivery stays text — the overlay rung is served by the
        // rasterised row, which is a different URL — but the note says
        // which rung actually won.
        let order = [AssTier::Overlay, AssTier::Burn, AssTier::Flatten];
        let d = delivery(&t, false, true, true, &ladder(&order, true, true));
        assert_eq!(d.0, Delivery::Text);
        assert!(d.1.contains("overlay"), "unexplained: {}", d.1);
        // ...and the raster row is the one that reads as playable, so
        // "best delivery wins" on the client lands on it.
        let r = track("raster", "raster");
        let d = delivery(&r, false, true, true, &ladder(&order, true, true));
        assert_eq!(d.0, Delivery::Overlay);
        // With flatten first instead, the raster stops offering itself
        // and the script's own text form wins.
        let d = delivery(&r, false, true, true, &ladder(&all, true, true));
        assert_eq!(d.0, Delivery::None);

        // Same order, but nothing has been rasterised yet — skip to
        // the next rung the fleet can serve.
        let d = delivery(&t, false, true, true, &ladder(&order, true, false));
        assert_eq!(d.0, Delivery::Burn);

        // ...and with no burn-capable box either, down to flatten.
        let d = delivery(&t, false, true, false, &ladder(&order, false, false));
        assert_eq!(d.0, Delivery::Text);

        // A client that cannot composite skips overlay however the
        // user ordered it — capability outranks preference.
        let d = delivery(&t, false, false, true, &ladder(&order, true, true));
        assert_eq!(d.0, Delivery::Burn);

        // The ladder can never strand a client: `choose` is total,
        // because flatten is always possible and the stored order is
        // always a permutation. Even a policy built by hand with a
        // single unreachable rung falls back rather than refusing.
        let d = delivery(
            &t,
            false,
            false,
            false,
            &ladder(&[AssTier::Burn], false, false),
        );
        assert_eq!(d.0, Delivery::Text);
    }

    /// A stored order is priority, never removal: whatever it leaves
    /// out is appended in default order, so a truncated or hand-edited
    /// value reorders the ladder instead of shortening it. Unknown
    /// names vanish, duplicates collapse, and `native` is not orderable
    /// at all.
    #[test]
    fn a_stored_order_always_parses_to_a_full_permutation() {
        use kahawai_media::negotiate::{AssPolicy, AssTier};
        let all = [AssTier::Flatten, AssTier::Overlay, AssTier::Burn];
        for stored in [
            "burn, flatten",
            "overlay,overlay,burn",
            "native,burn",
            "nonsense",
            "",
        ] {
            let got = AssPolicy::parse_order(stored);
            assert_eq!(got.len(), all.len(), "{stored:?} -> {got:?}");
            for t in all {
                assert!(got.contains(&t), "{stored:?} lost {t:?}");
            }
        }
        // The stated part keeps its order; the rest follow.
        assert_eq!(
            AssPolicy::parse_order("burn"),
            vec![AssTier::Burn, AssTier::Flatten, AssTier::Overlay]
        );
        assert_eq!(AssPolicy::parse_order("native,burn")[0], AssTier::Burn);
    }
}
