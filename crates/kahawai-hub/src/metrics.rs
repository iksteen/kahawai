//! Prometheus metrics and health (NFR-6).
//!
//! Everything here is read from state the hub already keeps, at scrape
//! time. There is no background aggregation and no counter bookkeeping
//! threaded through the codebase: a scrape is a handful of `COUNT`s and a
//! walk of the in-memory registry, which costs less than the enrichment
//! pass does in a second. Counters that would need instrumenting every
//! call site are deliberately absent rather than half-present — a gauge
//! that is right beats a counter that resets on restart and nobody
//! noticed.
//!
//! **Health is per module, but served by the hub.** Satellites dial out
//! and never listen (AR-3); giving each one an HTTP server to answer
//! `/health` would invert the architecture for the sake of a checkbox.
//! The hub already knows every module's state — that is what the registry
//! is — so it reports on all of them, and a monitor gets one endpoint
//! instead of five it cannot reach through NAT anyway.

use std::fmt::Write as _;

use anyhow::Result;
use serde_json::json;

use crate::registry::Registry;
use crate::sessions::Sessions;

/// Rendered Prometheus text, plus the health verdict that shares its
/// inputs — gathered once so the two can never disagree.
pub struct Snapshot {
    pub modules: Vec<ModuleHealth>,
    pub sessions_active: usize,
    pub items: i64,
    pub files: i64,
    pub file_bytes: i64,
    pub subtitle_files: i64,
    pub enrichment_due: i64,
    pub unmatched_items: i64,
    pub anidb_banned_secs: i64,
}

pub struct ModuleHealth {
    pub module_id: String,
    pub name: String,
    pub kind: String,
    pub connected: bool,
    pub disabled: bool,
}

/// One scrape. Cheap by construction; see the module note.
pub async fn gather(registry: &Registry, sessions: &Sessions, data_dir: &std::path::Path) -> Result<Snapshot> {
    let db = registry.db();
    let modules = registry
        .satellites_overview()
        .await?
        .into_iter()
        .map(|v| ModuleHealth {
            module_id: v["module_id"].as_str().unwrap_or_default().to_string(),
            name: v["name"].as_str().unwrap_or_default().to_string(),
            kind: v["module_type"].as_str().unwrap_or_default().to_string(),
            connected: v["connected"].as_bool().unwrap_or(false),
            disabled: v["disabled"].as_bool().unwrap_or(false),
        })
        .collect();

    let one = |sql: &'static str| async move {
        sqlx::query_scalar::<_, i64>(sql).fetch_one(db).await.unwrap_or(0)
    };
    Ok(Snapshot {
        modules,
        sessions_active: sessions.list().len(),
        items: one("SELECT COUNT(*) FROM items").await,
        files: one("SELECT COUNT(*) FROM files").await,
        file_bytes: one("SELECT COALESCE(SUM(size), 0) FROM files").await,
        subtitle_files: one("SELECT COUNT(*) FROM downloaded_subtitles").await,
        enrichment_due: one(
            "SELECT COUNT(*) FROM enrichment_queue WHERE due_at <= unixepoch()",
        )
        .await,
        // The number worth alerting on: items nothing has identified.
        unmatched_items: one(
            "SELECT COUNT(*) FROM items i
              WHERE i.kind IN ('movie', 'show', 'album')
                AND NOT EXISTS (SELECT 1 FROM item_match m WHERE m.item_id = i.id)",
        )
        .await,
        anidb_banned_secs: crate::anidb::ban_remaining(data_dir).unwrap_or(0),
    })
}

/// Prometheus text exposition (v0.0.4). Names follow the convention:
/// `kahawai_` prefix, base units, `_total` only on counters — which is
/// why the gauges here do not carry it.
pub fn render(s: &Snapshot) -> String {
    let mut out = String::with_capacity(2048);
    let g = |out: &mut String, name: &str, help: &str, value: String| {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        let _ = write!(out, "{value}");
    };

    g(
        &mut out,
        "kahawai_build_info",
        "Build version, as a label on a constant 1.",
        format!("kahawai_build_info{{version=\"{}\"}} 1\n", env!("CARGO_PKG_VERSION")),
    );

    // Per module, so a dashboard can show which satellite went away
    // rather than only that one did.
    let mut lines = String::new();
    for m in &s.modules {
        let _ = writeln!(
            lines,
            "kahawai_module_up{{module=\"{}\",name=\"{}\",kind=\"{}\"}} {}",
            m.module_id,
            escape(&m.name),
            m.kind,
            u8::from(m.connected)
        );
    }
    g(&mut out, "kahawai_module_up", "1 when a satellite's link is live.", lines);

    let mut lines = String::new();
    for m in &s.modules {
        let _ = writeln!(
            lines,
            "kahawai_module_disabled{{module=\"{}\",name=\"{}\"}} {}",
            m.module_id,
            escape(&m.name),
            u8::from(m.disabled)
        );
    }
    g(
        &mut out,
        "kahawai_module_disabled",
        "1 when an admin has taken a module out of placement.",
        lines,
    );

    for (name, help, value) in [
        ("kahawai_sessions_active", "Playback sessions alive now.", s.sessions_active as i64),
        ("kahawai_items", "Catalogue items, all kinds.", s.items),
        ("kahawai_files", "Files known across every collection.", s.files),
        ("kahawai_file_bytes", "Total size of known files.", s.file_bytes),
        ("kahawai_subtitle_files", "Subtitles held in the store.", s.subtitle_files),
        (
            "kahawai_enrichment_due",
            "Items a provider owes an answer for, due now.",
            s.enrichment_due,
        ),
        (
            "kahawai_items_unmatched",
            "Top-level items no provider has identified.",
            s.unmatched_items,
        ),
        (
            "kahawai_anidb_ban_seconds",
            "Seconds AniDB contact stays suppressed; 0 when clear.",
            s.anidb_banned_secs,
        ),
    ] {
        g(&mut out, name, help, format!("{name} {value}\n"));
    }
    out
}

/// Health for every module, hub included.
///
/// "Degraded" rather than "down" when a satellite is missing: the hub is
/// still serving everything the other modules hold, and a monitor that
/// pages for one unplugged Pi at 3am is a monitor people mute.
pub fn health(s: &Snapshot) -> serde_json::Value {
    let offline: Vec<&ModuleHealth> =
        s.modules.iter().filter(|m| !m.connected && !m.disabled).collect();
    let status = if offline.is_empty() { "ok" } else { "degraded" };
    json!({
        "status": status,
        "version": env!("CARGO_PKG_VERSION"),
        "sessions_active": s.sessions_active,
        "modules": s.modules.iter().map(|m| json!({
            "module_id": m.module_id,
            "name": m.name,
            "kind": m.kind,
            // A disabled module is not unhealthy — somebody meant it.
            "status": if m.disabled { "disabled" }
                      else if m.connected { "ok" } else { "offline" },
        })).collect::<Vec<_>>(),
    })
}

/// Prometheus label values may not carry a bare quote or backslash.
fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"")
}
