//! In-memory registry of connected satellites (HUB-1, AR-6: a disconnect
//! marks state unavailable, never deletes). Persistence arrives with SQLite.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct SatelliteState {
    pub module_type: String,
    pub name: String,
    pub cert_fingerprint: String,
    pub connected: bool,
    pub last_seen: SystemTime,
}

/// One announced collection and its file records (HUB-1). `available`
/// follows the owning mediahost's connection (AR-6).
#[derive(Debug, Clone)]
pub struct CollectionState {
    pub media_type: String,
    pub roots: Vec<String>,
    pub available: bool,
    pub files: HashMap<String, FileEntry>,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub size: u64,
    pub mtime_unix: i64,
    pub head_xxh3: u64,
    pub tail_xxh3: u64,
    pub oshash: u64,
    pub streams_json: String,
}

#[derive(Default)]
pub struct Registry {
    satellites: Mutex<HashMap<String, SatelliteState>>,
    /// Keyed by (mediahost module_id, collection id).
    collections: Mutex<HashMap<(String, String), CollectionState>>,
}

impl Registry {
    pub fn connected(&self, module_id: &str, module_type: &str, name: &str, fingerprint: &str) {
        let mut sats = self.satellites.lock().unwrap();
        sats.insert(
            module_id.to_string(),
            SatelliteState {
                module_type: module_type.to_string(),
                name: name.to_string(),
                cert_fingerprint: fingerprint.to_string(),
                connected: true,
                last_seen: SystemTime::now(),
            },
        );
        tracing::info!(%module_id, module_type, name, "satellite connected");
    }

    pub fn seen(&self, module_id: &str) {
        if let Some(s) = self.satellites.lock().unwrap().get_mut(module_id) {
            s.last_seen = SystemTime::now();
        }
    }

    pub fn disconnected(&self, module_id: &str) {
        if let Some(s) = self.satellites.lock().unwrap().get_mut(module_id) {
            s.connected = false;
            s.last_seen = SystemTime::now();
            tracing::info!(%module_id, "satellite disconnected");
        }
        // AR-6: unavailable, never deleted.
        for ((host, _), c) in self.collections.lock().unwrap().iter_mut() {
            if host == module_id {
                c.available = false;
            }
        }
    }

    pub fn announce_collection(
        &self,
        module_id: &str,
        collection_id: &str,
        media_type: &str,
        roots: Vec<String>,
    ) {
        let mut cols = self.collections.lock().unwrap();
        let key = (module_id.to_string(), collection_id.to_string());
        let entry = cols.entry(key).or_insert_with(|| CollectionState {
            media_type: media_type.to_string(),
            roots: Vec::new(),
            available: true,
            files: HashMap::new(),
        });
        entry.media_type = media_type.to_string();
        entry.roots = roots;
        entry.available = true;
        tracing::info!(%module_id, collection = collection_id, media_type, "collection announced");
    }

    /// Upsert file records; unknown collections are created implicitly so a
    /// racing announce/upsert never drops data.
    pub fn upsert_files(
        &self,
        module_id: &str,
        collection_id: &str,
        files: impl IntoIterator<Item = (String, FileEntry)>,
    ) -> usize {
        let mut cols = self.collections.lock().unwrap();
        let key = (module_id.to_string(), collection_id.to_string());
        let entry = cols.entry(key).or_insert_with(|| CollectionState {
            media_type: String::new(),
            roots: Vec::new(),
            available: true,
            files: HashMap::new(),
        });
        let mut n = 0;
        for (path_rel, file) in files {
            entry.files.insert(path_rel, file);
            n += 1;
        }
        n
    }

    pub fn collections_snapshot(&self) -> Vec<((String, String), CollectionState)> {
        let mut v: Vec<_> = self
            .collections
            .lock()
            .unwrap()
            .iter()
            .map(|(k, c)| (k.clone(), c.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    pub fn snapshot(&self) -> Vec<(String, SatelliteState)> {
        let mut v: Vec<_> = self
            .satellites
            .lock()
            .unwrap()
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
}
