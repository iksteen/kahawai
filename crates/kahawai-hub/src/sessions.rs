//! Play sessions (HUB-18 minimal): direct play only for now — source
//! selection, lease establishment, range serving state.
//!
//! ponytail: sessions are in-memory (lost on hub restart, clients reopen);
//! idle timeout and per-user concurrency limits land with HUB-18 proper.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use kahawai_proto::v1::{hub_to_host, HubToHost, OpenRead};
use sqlx::Row;

use crate::leases::{new_lease_token, Lease, Leases};
use crate::registry::Registry;

pub struct Session {
    pub id: String,
    pub user_id: String,
    pub item_id: String,
    pub module_id: String,
    pub size: u64,
    pub container: Option<String>,
    pub lease: Lease,
}

#[derive(Default)]
pub struct Sessions {
    pub leases: Leases,
    active: Mutex<HashMap<String, Arc<Session>>>,
}

impl Sessions {
    /// Start a direct-play session for an item: pick the best available
    /// source, open a read lease on its mediahost.
    pub async fn start(
        self: &Arc<Self>,
        registry: &Registry,
        user_id: &str,
        item_id: &str,
    ) -> Result<Arc<Session>> {
        let rows = sqlx::query(
            "SELECT s.module_id, s.collection_id, s.path_rel, f.size, f.streams_json
             FROM item_sources s
             JOIN files f ON (f.module_id, f.collection_id, f.path_rel)
                           = (s.module_id, s.collection_id, s.path_rel)
             WHERE s.item_id = ? ORDER BY f.size DESC",
        )
        .bind(item_id)
        .fetch_all(registry.db())
        .await?;
        if rows.is_empty() {
            bail!("no sources for item");
        }
        let source = rows
            .iter()
            .find(|r| registry.is_connected(&r.get::<String, _>("module_id")))
            .context("no source is currently available (mediahost offline)")?;

        let module_id: String = source.get("module_id");
        let collection_id: String = source.get("collection_id");
        let path_rel: String = source.get("path_rel");
        let size = source.get::<i64, _>("size") as u64;
        let container = serde_json::from_str::<serde_json::Value>(
            source.get::<String, _>("streams_json").as_str(),
        )
        .ok()
        .and_then(|v| v["container"].as_str().map(String::from));

        let token = new_lease_token();
        let msg = HubToHost {
            msg: Some(hub_to_host::Msg::OpenRead(OpenRead {
                lease_token: token.clone(),
                collection_id,
                path_rel: path_rel.clone(),
            })),
        };
        let lease = self
            .leases
            .establish(&token, registry.send_to_host(&module_id, msg))
            .await?;

        let session = Arc::new(Session {
            id: ulid::Ulid::new().to_string(),
            user_id: user_id.to_string(),
            item_id: item_id.to_string(),
            module_id,
            size,
            container,
            lease,
        });
        self.active.lock().unwrap().insert(session.id.clone(), session.clone());
        tracing::info!(session = %session.id, item = item_id, path = %path_rel, "direct-play session started");
        Ok(session)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.active.lock().unwrap().get(id).cloned()
    }

    /// Remove a session; the lease drops with the last reference, closing
    /// the byte channel.
    pub fn end(&self, id: &str) -> bool {
        let removed = self.active.lock().unwrap().remove(id).is_some();
        if removed {
            tracing::info!(session = id, "session ended");
        }
        removed
    }
}
