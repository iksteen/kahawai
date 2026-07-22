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

#[derive(Default)]
pub struct Registry {
    satellites: Mutex<HashMap<String, SatelliteState>>,
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
