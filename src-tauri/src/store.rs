//! Settings on disk: `%APPDATA%/com.deetsairplay.app/deetsairplay.json`.
//! Last speaker (so the tray menu can offer it), volume, and the latency
//! choice. Same shape of store as DeetsRGB's scenes.rs.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum Latency {
    /// Pick the buffer from the measured keep-alive round trip.
    Auto,
    /// A fixed buffer, in milliseconds (250..2000).
    Fixed { ms: u32 },
}

impl Default for Latency {
    fn default() -> Self {
        Latency::Auto
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub last_speaker: Option<LastSpeaker>,
    pub volume: f64,
    pub latency: Latency,
    /// Extra delay on top of the buffer, for lip-sync with a screen (ms).
    pub sync_offset_ms: u32,
    pub autostart_seeded: bool,
    /// First installed run asked (once, with UAC) for the inbound-UDP rule.
    pub firewall_seeded: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LastSpeaker {
    pub name: String,
    pub ip: String,
    pub port: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self { last_speaker: None, volume: 60.0, latency: Latency::Auto, sync_offset_ms: 0, autostart_seeded: false, firewall_seeded: false }
    }
}

pub struct Store {
    path: PathBuf,
    pub settings: Mutex<Settings>,
}

impl Store {
    pub fn load(dir: PathBuf) -> Self {
        let path = dir.join("deetsairplay.json");
        let settings = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { path, settings: Mutex::new(settings) }
    }

    pub fn save(&self) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_vec_pretty(&*self.settings.lock().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, json).map_err(|e| e.to_string())
    }
}
