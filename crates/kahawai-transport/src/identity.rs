//! Satellite identity persistence (SEC-4): private key, signed cert, and the
//! pinned hub CA, stored in the module's `state_dir`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct SatelliteIdentity {
    pub module_id: String,
    pub key_pem: String,
    pub cert_pem: String,
    /// The hub CA this satellite pins for all future connections (SEC-4).
    pub ca_pem: String,
}

const FILES: [&str; 4] = ["module_id", "sat.key", "sat.crt", "ca.crt"];

/// Load a previously enrolled identity, or `None` if not (fully) enrolled.
pub fn load(state_dir: &Path) -> Result<Option<SatelliteIdentity>> {
    if !FILES.iter().all(|f| state_dir.join(f).exists()) {
        return Ok(None);
    }
    let read = |f: &str| {
        fs::read_to_string(state_dir.join(f))
            .with_context(|| format!("reading {}", state_dir.join(f).display()))
    };
    Ok(Some(SatelliteIdentity {
        module_id: read("module_id")?.trim().to_string(),
        key_pem: read("sat.key")?,
        cert_pem: read("sat.crt")?,
        ca_pem: read("ca.crt")?,
    }))
}

pub fn store(state_dir: &Path, id: &SatelliteIdentity) -> Result<()> {
    fs::create_dir_all(state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    fs::write(state_dir.join("module_id"), &id.module_id)?;
    fs::write(state_dir.join("sat.crt"), &id.cert_pem)?;
    fs::write(state_dir.join("ca.crt"), &id.ca_pem)?;
    let key_path = state_dir.join("sat.key");
    fs::write(&key_path, &id.key_pem)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_partial_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_none());

        let id = SatelliteIdentity {
            module_id: "01H".into(),
            key_pem: "KEY".into(),
            cert_pem: "CERT".into(),
            ca_pem: "CA".into(),
        };
        store(dir.path(), &id).unwrap();
        let back = load(dir.path()).unwrap().unwrap();
        assert_eq!(back.module_id, "01H");
        assert_eq!(back.ca_pem, "CA");

        // A missing file means not enrolled — no half-identities.
        fs::remove_file(dir.path().join("ca.crt")).unwrap();
        assert!(load(dir.path()).unwrap().is_none());
    }
}
