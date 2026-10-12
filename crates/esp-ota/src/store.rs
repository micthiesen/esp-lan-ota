//! The private esp-ota fabric and enrolled devices, kept in
//! `~/.config/esp-lan-ota/fabric.toml` (mode 0600).
//!
//! The file holds the intermediate CA key that signs device certificates and
//! the controller's operational key. Anyone with it can administer every
//! enrolled device. The root CA key is discarded after creating the ICAC.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    pub fabric: FabricRecord,
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FabricRecord {
    pub fabric_id: u64,
    pub vendor_id: u16,
    pub controller_node_id: u64,
    /// Hex TLV certificates.
    pub rcac: String,
    pub icac: String,
    pub controller_noc: String,
    /// Hex canonical P-256 secret keys.
    pub icac_key: String,
    pub controller_key: String,
    /// Hex 16-byte identity protection key.
    pub ipk: String,
    pub next_node_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceRecord {
    pub node_id: u64,
    /// Last address that answered, as `ip:port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Setup passcode and discriminator, kept so `share` can print the code
    /// Home needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passcode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discriminator: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chip: Option<String>,
    /// Endpoint hosting the esp-lan-ota cluster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<u16>,
}

impl DeviceRecord {
    pub fn new(node_id: u64) -> Self {
        Self {
            node_id,
            address: None,
            passcode: None,
            discriminator: None,
            project: None,
            chip: None,
            endpoint: None,
        }
    }
}

pub fn default_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("ESP_OTA_STORE") {
        return Ok(PathBuf::from(path));
    }
    let base = dirs::home_dir().context("no home directory")?;
    Ok(base.join(".config").join("esp-lan-ota").join("fabric.toml"))
}

impl Store {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(Some(
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Writes atomically with owner-only permissions.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("store path has no parent")?;
        fs::create_dir_all(dir)?;
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        // A leftover temp file would keep its old permissions.
        let _ = fs::remove_file(&tmp);
        {
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn device(&self, name: &str) -> Result<&DeviceRecord> {
        match self.devices.get(name) {
            Some(device) => Ok(device),
            None => bail!(
                "unknown device {name:?}; enrolled: {}",
                self.devices.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
        }
    }

    pub fn allocate_node_id(&mut self) -> u64 {
        let id = self.fabric.next_node_id;
        self.fabric.next_node_id += 1;
        id
    }
}

pub fn hex_bytes(text: &str) -> Result<Vec<u8>> {
    hex::decode(text).context("invalid hex in store")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_with_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("fabric.toml");
        let mut store = Store {
            fabric: FabricRecord {
                fabric_id: 7,
                vendor_id: 0xFFF1,
                controller_node_id: 1,
                rcac: "00".into(),
                icac: "01".into(),
                controller_noc: "02".into(),
                icac_key: "03".into(),
                controller_key: "04".into(),
                ipk: "05".into(),
                next_node_id: 100,
            },
            devices: BTreeMap::new(),
        };
        let id = store.allocate_node_id();
        store.devices.insert("lamp".into(), DeviceRecord::new(id));
        store.save(&path).unwrap();
        let loaded = Store::load(&path).unwrap().unwrap();
        assert_eq!(loaded.devices["lamp"].node_id, 100);
        assert_eq!(loaded.fabric.next_node_id, 101);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(Store::load(&dir.path().join("missing.toml"))
            .unwrap()
            .is_none());
    }
}
