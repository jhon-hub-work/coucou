// Preferences, stored as plain JSON in settings.json under platform::config_dir().
// No secret ever lands here — API keys live in the OS keychain (see secrets.rs).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default)]
    pub glass: Glass,
    pub sound_enabled: bool,
    pub sound_volume: f64,
    pub auto_close_interval: f64,
    pub absence_interval: f64,
    pub active_integrations: Vec<String>,
    /// "primary" = the main display, "cursor" = whichever display the mouse is on.
    pub screen: String,
    pub autostart: bool,
    pub hooks_installed: bool,
    /// OpenCode Go model used by the chat. Changeable in the settings window.
    /// Defaulted explicitly so a settings.json written by an older build still loads.
    #[serde(default = "default_model")]
    pub model: String,
    /// Folder of the video publishing board (tracker.py, status.json…), read by
    /// the Board pill. Never written to.
    #[serde(default = "default_board_dir")]
    pub board_dir: String,
    /// Publish agent states to the phone's ntfy topic (phone.rs). Off by default.
    #[serde(default)]
    pub phone_sync: bool,
    /// Private ntfy topic for the phone, made once on first load.
    #[serde(default)]
    pub phone_topic: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Glass {
    #[default]
    Clear,
    Tinted,
}

fn default_board_dir() -> String {
    crate::board::DEFAULT_DIR.to_string()
}

fn default_model() -> String {
    crate::opencode::DEFAULT_MODEL.to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            glass: Glass::Clear,
            sound_enabled: true,
            sound_volume: 0.12,
            auto_close_interval: 15.0,
            absence_interval: 180.0,
            active_integrations: vec![
                "integration_board".into(),
                "integration_resend".into(),
                "integration_n8n".into(),
                "integration_vercel".into(),
                "integration_github".into(),
            ],
            screen: "primary".into(),
            autostart: false,
            hooks_installed: false,
            model: default_model(),
            board_dir: default_board_dir(),
            phone_sync: false,
            phone_topic: String::new(),
        }
    }
}

pub use crate::platform::{config_dir, local_dir};

pub fn hook_exe_path() -> PathBuf {
    local_dir().join("bin").join(crate::platform::HOOK_EXE)
}

fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn load() -> Settings {
    let mut settings: Settings = match std::fs::read(settings_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Settings::default(),
    };
    if settings.phone_topic.is_empty() {
        settings.phone_topic = crate::phone::new_topic();
        let _ = save(&settings);
    }
    settings
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    let dir = config_dir();
    crate::platform::ensure_private_dir(&dir)?;
    let json = serde_json::to_vec_pretty(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(settings_path(), json)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A settings.json written by an older build must load whole, not reset.
    #[test]
    fn old_settings_file_keeps_its_values() {
        let old = r#"{"soundEnabled":false,"soundVolume":0.3,"autoCloseInterval":9.0,
            "absenceInterval":180.0,"activeIntegrations":["integration_n8n"],
            "screen":"primary","autostart":true,"hooksInstalled":true}"#;
        let s: Settings = serde_json::from_str(old).expect("old file must still parse");
        assert_eq!(s.board_dir, default_board_dir());
        assert_eq!(s.glass, Glass::Clear);
        let mut tinted = s.clone();
        tinted.glass = Glass::Tinted;
        let saved = serde_json::to_string(&tinted).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&saved).unwrap().glass, Glass::Tinted);
        assert!(s.autostart && s.hooks_installed && !s.sound_enabled);
        assert_eq!(s.active_integrations, vec!["integration_n8n".to_string()]);
    }
}
