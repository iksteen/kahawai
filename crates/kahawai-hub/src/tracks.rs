//! Subtitle tracks and client-specific delivery. Mediadb supplies physical
//! streams and source-bound acquired artifacts; this module presents them in
//! the playback API and applies the user's ASS preference. Delivery depends
//! on the client, while track ownership remains attached to its source.

use serde::Serialize;

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Track {
    #[serde(skip)]
    pub acquired: Option<std::sync::Arc<kahawai_media::subtitles::Extracted>>,
    /// Immutable physical revision key for generated catalogue artifacts.
    #[serde(skip)]
    pub artifact_key: Option<String>,
    #[serde(skip)]
    pub raster: Option<std::path::PathBuf>,
    #[serde(skip)]
    pub physical: Option<crate::subtitles::FileSource>,
    pub id: i64,
    pub item_id: String,
    pub origin: String,
    #[serde(skip)]
    pub module_id: Option<String>,
    #[serde(skip)]
    pub collection_id: Option<String>,
    #[serde(skip)]
    pub root_token: Option<String>,
    #[serde(skip)]
    pub source_path: Option<String>,
    #[serde(skip)]
    pub path_rel: Option<String>,
    #[schema(required)]
    pub stream_index: Option<i64>,
    pub format: String,
    #[schema(required)]
    pub language: Option<String>,
    #[schema(required)]
    pub label: Option<String>,
    pub machine: bool,
    #[schema(required)]
    pub derived_from: Option<i64>,
    /// Who created a hub-stored row. Never serialised — it decides
    /// `TrackListing::deletable` server-side rather than telling every
    /// client which user fetched which subtitle.
    #[serde(skip)]
    pub created_by: Option<String>,
}

/// How a track can be served to a given client — the tier ladder
/// expressed per track instead of per plan.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, utoipa::ToSchema)]
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
    profile: &kahawai_core::media::CapabilityProfile,
    burn_capable: bool,
    ass: &kahawai_media::negotiate::AssPolicy,
) -> (Delivery, &'static str) {
    // The profile arrives whole rather than as loose bools. Four
    // parallel booleans at a call site is three chances to swap two of
    // them, and this function used to REBUILD a profile internally from
    // them just to ask the ladder — which is the same object the caller
    // already had.
    let (graphics_overlay, vtt_render) = (profile.graphics_overlay, profile.vtt_render);
    // HUB-32d: a rasterised script is display sets like any other, but
    // it is a stored source artifact — no session tap,
    // so it needs neither `burn_capable` nor an embedded origin.
    //
    // It offers itself only when the LADDER picked the overlay rung.
    // That is what stops the parent script and its raster both
    // claiming the same delivery — and they would send the client to
    // different URLs. Whichever rung won is the only one that reads as
    // playable, so the client's existing "best delivery wins" pick
    // lands on it without having to know the user's order.
    if track.origin == "raster" {
        return match ass.choose(profile) {
            Some(kahawai_media::negotiate::AssTier::Overlay) => (
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
            match ass.choose(profile) {
                Some(kahawai_media::negotiate::AssTier::Native) => (Delivery::Ass, ""),
                Some(kahawai_media::negotiate::AssTier::Burn) => {
                    (Delivery::Burn, "burned in — restarts with a video encode")
                }
                // The overlay rung is served by the RASTER row, not by
                // this one — they are different URLs. The script's own
                // remaining form is the flattened VTT, which is honest
                // and still fetchable; the raster simply outranks it.
                //
                // Unless text is off: then the script has no readable
                // form of its own at all and the raster is the whole
                // answer.
                Some(kahawai_media::negotiate::AssTier::Overlay) if vtt_render => (
                    Delivery::Text,
                    "flattened to VTT — the rasterised overlay is preferred",
                ),
                Some(kahawai_media::negotiate::AssTier::Overlay) => {
                    (Delivery::None, "the rasterised overlay carries this script")
                }
                Some(kahawai_media::negotiate::AssTier::Flatten) => {
                    (Delivery::Text, "flattened to VTT")
                }
                None => (
                    Delivery::None,
                    "client renders neither ASS nor text, and no box can burn it in",
                ),
            }
        }
        // Everything else is timed text — SRT, an OCR-derived track, a
        // downloaded .srt — and reaches the client as WebVTT or not at
        // all. A client that renders none falls to burn, the same last
        // resort an image track takes when it cannot composite.
        _ if vtt_render => (Delivery::Text, ""),
        _ if burn_capable => (Delivery::Burn, "burned in — the client renders no text"),
        _ => (
            Delivery::None,
            "client renders no text and no box can burn it in",
        ),
    }
}

impl Track {
    /// The notation shared by caches, extraction and the pipeline: `e{n}` / `s{n}` / `d{row id}`.
    pub fn internal_key(&self) -> String {
        match self.origin.as_str() {
            "embedded" => format!("e{}", self.stream_index.unwrap_or(0)),
            "sidecar" => format!("s{}", self.stream_index.unwrap_or(0)),
            _ => format!("d{}", self.id),
        }
    }
}

impl Track {
    pub(crate) fn source_revision(&self) -> anyhow::Result<&str> {
        use anyhow::Context;
        let physical = self
            .physical
            .as_ref()
            .context("track has no captured source")?;
        Ok(if self.origin == "sidecar" {
            &physical.sidecar_revision
        } else {
            &physical.revision
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(format: &str, origin: &str) -> Track {
        Track {
            acquired: None,
            artifact_key: None,
            raster: None,
            physical: None,
            id: 1,
            item_id: "i".into(),
            origin: origin.into(),
            module_id: Some("m".into()),
            collection_id: Some("c".into()),
            root_token: Some("root".into()),
            source_path: Some("f.mkv".into()),
            path_rel: Some("key".into()),
            stream_index: Some(0),
            format: format.into(),
            language: None,
            label: None,
            machine: false,
            derived_from: None,
            created_by: None,
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
        // Named, because `delivery(&t, false, true, true, ..)` is three
        // booleans nobody can read at a glance — the reason the function
        // takes a profile now.
        let caps = |ass_render: bool, graphics_overlay: bool, vtt_render: bool| {
            kahawai_core::media::CapabilityProfile {
                ass_render,
                graphics_overlay,
                vtt_render,
                ..Default::default()
            }
        };

        // Native outranks every order, including one that names burn
        // first: nothing a server does beats the real renderer.
        let d = delivery(
            &t,
            &caps(true, true, true),
            true,
            &ladder(&[AssTier::Burn], true, true),
        );
        assert_eq!(d.0, Delivery::Ass);

        // The default order, no client-side ASS: flatten is first and
        // always possible, so it wins even with the others available.
        let d = delivery(
            &t,
            &caps(false, true, true),
            true,
            &ladder(&all, true, true),
        );
        assert_eq!(d.0, Delivery::Text);

        // Reordered: overlay first, and it is ready. The SCRIPT's own
        // delivery stays text — the overlay rung is served by the
        // rasterised row, which is a different URL — but the note says
        // which rung actually won.
        let order = [AssTier::Overlay, AssTier::Burn, AssTier::Flatten];
        let d = delivery(
            &t,
            &caps(false, true, true),
            true,
            &ladder(&order, true, true),
        );
        assert_eq!(d.0, Delivery::Text);
        assert!(d.1.contains("overlay"), "unexplained: {}", d.1);
        // ...and the raster row is the one that reads as playable, so
        // "best delivery wins" on the client lands on it.
        let r = track("raster", "raster");
        let d = delivery(
            &r,
            &caps(false, true, true),
            true,
            &ladder(&order, true, true),
        );
        assert_eq!(d.0, Delivery::Overlay);
        // With flatten first instead, the raster stops offering itself
        // and the script's own text form wins.
        let d = delivery(
            &r,
            &caps(false, true, true),
            true,
            &ladder(&all, true, true),
        );
        assert_eq!(d.0, Delivery::None);

        // Same order, but nothing has been rasterised yet — skip to
        // the next rung the fleet can serve.
        let d = delivery(
            &t,
            &caps(false, true, true),
            true,
            &ladder(&order, true, false),
        );
        assert_eq!(d.0, Delivery::Burn);

        // ...and with no burn-capable box either, down to flatten.
        let d = delivery(
            &t,
            &caps(false, true, true),
            false,
            &ladder(&order, false, false),
        );
        assert_eq!(d.0, Delivery::Text);

        // A client that cannot composite skips overlay however the
        // user ordered it — capability outranks preference.
        let d = delivery(
            &t,
            &caps(false, false, true),
            true,
            &ladder(&order, true, true),
        );
        assert_eq!(d.0, Delivery::Burn);

        // A client that renders text is never stranded: flatten is
        // always possible for it and the stored order is always a
        // permutation, so even a hand-built policy with a single
        // unreachable rung falls back rather than refusing.
        let d = delivery(
            &t,
            &caps(false, false, true),
            false,
            &ladder(&[AssTier::Burn], false, false),
        );
        assert_eq!(d.0, Delivery::Text);

        // Turn text off and that guarantee ends — which is the whole
        // point of the bit. Flatten needs a text renderer, so an
        // ASS-less, text-less, overlay-less client on a fleet that
        // cannot burn has no rung at all, and the honest answer is to
        // say so rather than name a tier that would deliver nothing.
        let d = delivery(
            &t,
            &caps(false, false, false),
            false,
            &ladder(&all, false, false),
        );
        assert_eq!(d.0, Delivery::None, "{}", d.1);
        assert!(d.1.contains("neither ASS nor text"), "unexplained: {}", d.1);

        // Give that same client a burn-capable box and the ladder
        // resolves again, one rung lower.
        let d = delivery(
            &t,
            &caps(false, false, false),
            true,
            &ladder(&all, true, false),
        );
        assert_eq!(d.0, Delivery::Burn, "{}", d.1);

        // And the case this was built for: a plain SRT for a client
        // that renders no text burns in, exactly as an image track does
        // when it cannot composite.
        let srt = track("embedded", "srt");
        let d = delivery(
            &srt,
            &caps(false, false, false),
            true,
            &ladder(&all, true, false),
        );
        assert_eq!(d.0, Delivery::Burn, "{}", d.1);
        let d = delivery(
            &srt,
            &caps(false, false, false),
            false,
            &ladder(&all, false, false),
        );
        assert_eq!(d.0, Delivery::None, "{}", d.1);
        // ...while a text-capable client still just gets it as text.
        let d = delivery(
            &srt,
            &caps(false, false, true),
            true,
            &ladder(&all, true, false),
        );
        assert_eq!(d.0, Delivery::Text, "{}", d.1);
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
