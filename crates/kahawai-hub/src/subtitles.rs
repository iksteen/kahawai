//! Text subtitle serving (HUB-15/27): enumerate an item's embedded and
//! sidecar text subtitles, extract/convert them to WebVTT lazily, and
//! cache extracted cues on the hub (embedded extraction demuxes the whole
//! source once — never twice).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use kahawai_media::subtitles::{decode_text, is_text_format, parse, to_vtt, Cue};
use serde::Serialize;
use sqlx::Row;

use crate::registry::Registry;
use crate::sessions::Sessions;

#[derive(Debug, Clone, Serialize)]
pub struct SubtitleEntry {
    /// "e{n}" for embedded track n, "s{n}" for sidecar n — stable within
    /// a source's stream info.
    pub key: String,
    pub kind: &'static str, // "embedded" | "sidecar"
    pub format: String,
    pub language: Option<String>,
    /// True when the source format is ASS/SSA: serving it as VTT loses
    /// styling, and HUB-32a demands that be a labeled, explicit choice.
    pub flattened: bool,
}

pub struct Subtitles {
    dir: PathBuf,
    /// Per-cache-key locks so concurrent requests extract once.
    inflight: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Subtitles {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, inflight: Default::default() }
    }

    /// The subtitle tracks we can serve for an item, from the same source
    /// `open_source` would pick (largest, preferring connected hosts).
    pub async fn list(&self, registry: &Registry, item_id: &str) -> Result<Vec<SubtitleEntry>> {
        let (_, _, _, info) = source_row(registry, item_id).await?;
        Ok(entries(&info))
    }

    /// WebVTT for one subtitle key, cue timestamps shifted by `shift_ms`
    /// (players whose timeline starts mid-file pass a negative shift).
    pub async fn vtt(
        &self,
        registry: &Registry,
        sessions: &Sessions,
        item_id: &str,
        key: &str,
        shift_ms: i64,
    ) -> Result<String> {
        let (module_id, collection_id, path_rel, info) = source_row(registry, item_id).await?;
        let entry = entries(&info)
            .into_iter()
            .find(|e| e.key == key)
            .with_context(|| format!("no subtitle {key} on this item"))?;

        let cache_key = format!(
            "{:016x}-{key}",
            xxhash_rust::xxh3::xxh3_64(
                format!("{module_id}\n{collection_id}\n{path_rel}").as_bytes()
            )
        );
        let lock = {
            let mut map = self.inflight.lock().unwrap();
            map.entry(cache_key.clone()).or_default().clone()
        };
        let _guard = lock.lock().await;

        let cache_path = self.dir.join(format!("{cache_key}.json"));
        let cues: Vec<Cue> = match std::fs::read(&cache_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(_) => {
                let cues = if let Some(n) = key.strip_prefix('s') {
                    let idx: usize = n.parse().context("bad sidecar key")?;
                    let sidecar =
                        info.external_subtitles.get(idx).context("sidecar index out of range")?;
                    let lease = sessions
                        .open_lease(registry, &module_id, &collection_id, &sidecar.path_rel)
                        .await?;
                    let bytes = read_all(lease).await?;
                    parse(&sidecar.format, &decode_text(&bytes))?
                } else if let Some(n) = key.strip_prefix('e') {
                    let idx: usize = n.parse().context("bad embedded key")?;
                    let (_, _, size, _, lease) =
                        sessions.open_source(registry, item_id).await?;
                    let source = crate::sessions::LeaseSource {
                        lease,
                        size,
                        handle: tokio::runtime::Handle::current(),
                    };
                    tokio::task::spawn_blocking(move || {
                        kahawai_media::subtitles::extract_embedded(Box::new(source), idx)
                    })
                    .await??
                } else {
                    bail!("bad subtitle key: {key}");
                };
                std::fs::create_dir_all(&self.dir)?;
                std::fs::write(&cache_path, serde_json::to_vec(&cues)?)?;
                cues
            }
        };
        let _ = entry;
        Ok(to_vtt(&cues, shift_ms))
    }
}

fn entries(info: &kahawai_core::media::MediaInfo) -> Vec<SubtitleEntry> {
    let mut out = Vec::new();
    for (i, s) in info.subtitles.iter().enumerate() {
        if is_text_format(&s.format) {
            out.push(SubtitleEntry {
                key: format!("e{i}"),
                kind: "embedded",
                format: s.format.clone(),
                language: s.language.clone(),
                flattened: matches!(s.format.as_str(), "ass" | "ssa"),
            });
        }
    }
    for (i, s) in info.external_subtitles.iter().enumerate() {
        out.push(SubtitleEntry {
            key: format!("s{i}"),
            kind: "sidecar",
            format: s.format.clone(),
            language: s.language.clone(),
            flattened: s.format == "ass",
        });
    }
    out
}

/// The item's source the way `Sessions::open_source` picks it, without
/// opening a lease: (module, collection, path, streams info).
async fn source_row(
    registry: &Registry,
    item_id: &str,
) -> Result<(String, String, String, kahawai_core::media::MediaInfo)> {
    let rows = sqlx::query(
        "SELECT s.module_id, s.collection_id, s.path_rel, f.streams_json
         FROM item_sources s
         JOIN files f ON (f.module_id, f.collection_id, f.path_rel)
                       = (s.module_id, s.collection_id, s.path_rel)
         WHERE s.item_id = ? ORDER BY f.size DESC",
    )
    .bind(item_id)
    .fetch_all(registry.db())
    .await?;
    let row = rows
        .iter()
        .find(|r| registry.is_connected(&r.get::<String, _>("module_id")))
        .or(rows.first())
        .context("no sources for item")?;
    let info: kahawai_core::media::MediaInfo =
        serde_json::from_str(row.get::<String, _>("streams_json").as_str()).unwrap_or_default();
    Ok((row.get("module_id"), row.get("collection_id"), row.get("path_rel"), info))
}

/// Drain a whole (small) file through a lease in chunks.
async fn read_all(lease: crate::leases::Lease) -> Result<Vec<u8>> {
    const CHUNK: u64 = 1 << 20;
    const MAX: usize = 16 << 20; // sidecars are text; 16 MiB is generous
    let mut out = Vec::new();
    loop {
        let mut stream = lease.read_range(out.len() as u64, CHUNK).into_inner();
        let mut got = 0u64;
        while let Some(chunk) = stream.recv().await {
            let bytes = chunk.map_err(|e| anyhow::anyhow!("lease read: {e}"))?;
            got += bytes.len() as u64;
            out.extend_from_slice(&bytes);
            if out.len() > MAX {
                bail!("subtitle file too large");
            }
        }
        if got < CHUNK {
            return Ok(out);
        }
    }
}
