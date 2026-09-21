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
//!   Composed by `work_class` and by nothing else. It deliberately
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

/// The class key, the EWMA step and its weight, and the reserved local
/// module id are the ranker's (`kahawai_playback::placement`); this module
/// owns only the table that persists what they compute.
pub use kahawai_playback::placement::{ALPHA, LOCAL, blend, work_class};

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
