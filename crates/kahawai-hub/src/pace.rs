//! What each box actually achieves, per kind of work (HUB-36 phase 4).
//!
//! # `transcoder_pace`
//!
//! One row per `(module_id, work_class)`, and this doc is the reference
//! for what those columns mean — the migration only creates them.
//!
//! * `module_id` — the satellite that did the work, or the reserved
//!   `local` for the hub's own executor. A re-enrolled satellite mints a
//!   new id and therefore starts learning again from its benchmarks,
//!   which is correct: it is not provably the same box.
//! * `work_class` — `{res}|{src}|{dst}[|tm]`, e.g. `2160|hevc|h264|tm`.
//!   Composed by [`work_class`] and by nothing else. It deliberately
//!   carries the SOURCE codec, the one dimension a benchmark cannot see
//!   (software AV1 *decode* is invisible to an encoder measurement), and
//!   the tone-map flag, which on the J5005 was the whole cost.
//! * `multiple` — content seconds produced per wall second, EWMA. Above
//!   1.0 the box produces faster than a viewer consumes.
//! * `samples` — how many runs folded in. Diagnostic, not a weight: the
//!   EWMA already discounts age, and a count that changed the weight
//!   would make an old box unmovable after a hardware swap.
//! * `updated_at` — unix seconds of the last fold.
//!
//! # Why an EWMA, and why 0.3
//!
//! A pace sample is one run on one file: it carries that title's
//! bitrate, that moment's contention, that box's thermal state. Storing
//! the last value would let one bad run condemn a box; storing a mean
//! would make a hardware change take dozens of sessions to show. At
//! α=0.3 a box converges within ~3 sessions of a change and no single
//! outlier moves the estimate more than 30%.
//!
//! # What is NOT here
//!
//! The link rate. It is per-connection, it lies the moment the network
//! changes, and it is cheap to re-learn — so it lives in memory on the
//! Registry and dies with the disconnect. Persisting it would only let
//! a stale number outlive the truth it described.

use anyhow::Result;
use kahawai_sqlite::Database as SqlitePool;

/// EWMA weight for a new sample. See the module doc.
pub const ALPHA: f64 = 0.3;

/// The reserved module id for work the hub ran itself.
pub const LOCAL: &str = "local";

/// `{res}|{src}|{dst}[|tm]` — the identity of a KIND of work.
///
/// The resolution is bucketed rather than exact because the cost step
/// that matters is 4K versus not. The cut is `> 1080`, so 1440p lands in
/// the expensive bucket: it is nearer 1080p in pixels, but guessing HIGH
/// costs a session placed on a stronger box, where guessing low costs a
/// viewer a stall. Framerate is deliberately absent: it
/// would split every class in two for a distinction most libraries never
/// exercise, and the key is a string, so an fps bucket is additive the
/// day a real library shows the skew.
pub fn work_class(height: u32, src_codec: &str, dst_codec: &str, tone_map: bool) -> String {
    let res = if height > 1080 { "2160" } else { "1080" };
    let tm = if tone_map { "|tm" } else { "" };
    format!("{res}|{src}|{dst}{tm}", src = src_codec, dst = dst_codec)
}

/// Fold one observation into `(module_id, work_class)`, returning the
/// new estimate. Write-through: placement reads its own in-memory map,
/// but a hub restart must not forget what the fleet is.
pub async fn fold(
    pool: &SqlitePool,
    module_id: &str,
    class: &str,
    multiple: f64,
    now_unix: i64,
) -> Result<f64> {
    let prev: Option<f64> = sqlx::query_scalar(
        "SELECT multiple FROM transcoder_pace WHERE module_id = ? AND work_class = ?",
    )
    .bind(module_id)
    .bind(class)
    .fetch_optional(pool)
    .await?;
    let next = blend(prev, multiple);
    sqlx::query(
        "INSERT INTO transcoder_pace (module_id, work_class, multiple, samples, updated_at)
         VALUES (?, ?, ?, 1, ?)
         ON CONFLICT(module_id, work_class) DO UPDATE SET
             multiple = excluded.multiple,
             samples = samples + 1,
             updated_at = excluded.updated_at",
    )
    .bind(module_id)
    .bind(class)
    .bind(next)
    .bind(now_unix)
    .execute(pool)
    .await?;
    Ok(next)
}

/// The EWMA step, separated so it can be reasoned about without a
/// database.
pub fn blend(prev: Option<f64>, sample: f64) -> f64 {
    match prev {
        Some(p) => ALPHA * sample + (1.0 - ALPHA) * p,
        None => sample,
    }
}

/// Everything learned so far, for the placement map at startup.
pub async fn load_all(pool: &SqlitePool) -> Result<Vec<(String, String, f64)>> {
    Ok(
        sqlx::query_as("SELECT module_id, work_class, multiple FROM transcoder_pace")
            .fetch_all(pool)
            .await?,
    )
}

/// Forget a satellite's learning. Called when it is deleted — its rows
/// describe hardware the fleet no longer has.
pub async fn forget(pool: &SqlitePool, module_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM transcoder_pace WHERE module_id = ?")
        .bind(module_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_keys_carry_source_codec_and_tonemap() {
        assert_eq!(work_class(2160, "hevc", "h264", true), "2160|hevc|h264|tm");
        assert_eq!(work_class(1080, "h264", "h264", false), "1080|h264|h264");
        // Bucketed at >1080, and anything above lands in the expensive
        // class: over-estimating costs a stronger box, under-estimating
        // costs a viewer a stall.
        assert_eq!(work_class(1080, "av1", "h264", false), "1080|av1|h264");
        assert_eq!(work_class(1081, "av1", "h264", false), "2160|av1|h264");
        assert_eq!(work_class(1440, "av1", "h264", false), "2160|av1|h264");
    }

    #[test]
    fn ewma_converges_in_about_three_samples_and_no_outlier_dominates() {
        // First sample is the estimate: nothing to blend against.
        assert_eq!(blend(None, 4.0), 4.0);
        // A single outlier moves it by at most ALPHA.
        let after = blend(Some(4.0), 0.5);
        assert!(
            (after - 4.0).abs() <= 4.0 * ALPHA + f64::EPSILON,
            "one sample moved the estimate {after}"
        );
        // A hardware change is believed within ~3 samples.
        let mut v = 4.0;
        for _ in 0..3 {
            v = blend(Some(v), 0.5);
        }
        assert!(v < 1.9, "still {v} after three slow runs");
    }
}
