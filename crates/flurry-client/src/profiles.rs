//! Persistence: Devices (address + port + type + linked profile) and
//! Profiles (a named, complete set of stream settings), stored as JSON in
//! the platform config dir (e.g. %APPDATA%\flurry-client\profiles.json).
//!
//! v1 files (profiles carrying an address) migrate automatically: each old
//! profile becomes a Device + Profile pair.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Settings;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DeviceType {
    #[default]
    Old3ds,
    Old2ds,
    New3ds,
    New2ds,
}

pub const DEVICE_TYPES: [DeviceType; 4] = [
    DeviceType::Old3ds,
    DeviceType::Old2ds,
    DeviceType::New3ds,
    DeviceType::New2ds,
];

impl DeviceType {
    pub fn label(self) -> &'static str {
        match self {
            DeviceType::Old3ds => "Old 3DS / XL",
            DeviceType::Old2ds => "2DS",
            DeviceType::New3ds => "New 3DS / XL",
            DeviceType::New2ds => "New 2DS XL",
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Device {
    pub name: String,
    pub address: String,
    pub port: u16,
    pub device_type: DeviceType,
    /// Name of the profile this device uses.
    pub profile: Option<String>,
}

impl Default for Device {
    fn default() -> Self {
        Device {
            name: "My 3DS".into(),
            address: String::new(),
            port: flurry_proto::PORT,
            device_type: DeviceType::Old3ds,
            profile: None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    pub name: String,
    pub settings: Settings,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Store {
    pub version: u32,
    pub devices: Vec<Device>,
    pub profiles: Vec<Profile>,
    pub last_device: Option<String>,
}

// v1 format, for migration.
#[derive(Deserialize)]
struct OldProfile {
    name: String,
    address: String,
    settings: Settings,
}
#[derive(Deserialize)]
struct OldStore {
    profiles: Vec<OldProfile>,
    last: Option<String>,
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("flurry-client").join("profiles.json"))
}

impl Store {
    pub fn load() -> Store {
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return Store { version: 2, ..Default::default() };
        };
        if let Ok(s) = serde_json::from_str::<Store>(&text) {
            if s.version >= 2 {
                return s;
            }
        }
        // v1 migration: profile-with-address becomes Device + Profile.
        if let Ok(old) = serde_json::from_str::<OldStore>(&text) {
            let mut s = Store { version: 2, ..Default::default() };
            for p in old.profiles {
                s.devices.push(Device {
                    name: p.name.clone(),
                    address: p.address,
                    profile: Some(p.name.clone()),
                    ..Default::default()
                });
                s.profiles.push(Profile { name: p.name, settings: p.settings });
            }
            s.last_device = old.last;
            s.save();
            return s;
        }
        Store { version: 2, ..Default::default() }
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(p, json);
        }
    }

    pub fn get_device(&self, name: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.name == name)
    }

    pub fn upsert_device(&mut self, device: Device) {
        self.last_device = Some(device.name.clone());
        match self.devices.iter_mut().find(|d| d.name == device.name) {
            Some(slot) => *slot = device,
            None => self.devices.push(device),
        }
        self.save();
    }

    pub fn delete_device(&mut self, name: &str) {
        self.devices.retain(|d| d.name != name);
        if self.last_device.as_deref() == Some(name) {
            self.last_device = None;
        }
        self.save();
    }

    pub fn get_profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    pub fn upsert_profile(&mut self, profile: Profile) {
        match self.profiles.iter_mut().find(|p| p.name == profile.name) {
            Some(slot) => *slot = profile,
            None => self.profiles.push(profile),
        }
        self.save();
    }

    pub fn delete_profile(&mut self, name: &str) {
        self.profiles.retain(|p| p.name != name);
        for d in &mut self.devices {
            if d.profile.as_deref() == Some(name) {
                d.profile = None;
            }
        }
        self.save();
    }
}
