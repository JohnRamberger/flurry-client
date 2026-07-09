//! Named device profiles: a saved address + settings set, persisted as JSON
//! in the platform config dir (e.g. %APPDATA%\flurry-client\profiles.json).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Settings;

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    pub name: String,
    pub address: String,
    pub settings: Settings,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Store {
    pub profiles: Vec<Profile>,
    /// Name of the profile selected when the app last ran.
    pub last: Option<String>,
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("flurry-client").join("profiles.json"))
}

impl Store {
    pub fn load() -> Store {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
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

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    /// Insert or overwrite by name.
    pub fn upsert(&mut self, profile: Profile) {
        self.last = Some(profile.name.clone());
        match self.profiles.iter_mut().find(|p| p.name == profile.name) {
            Some(slot) => *slot = profile,
            None => self.profiles.push(profile),
        }
        self.save();
    }

    pub fn delete(&mut self, name: &str) {
        self.profiles.retain(|p| p.name != name);
        if self.last.as_deref() == Some(name) {
            self.last = None;
        }
        self.save();
    }
}
