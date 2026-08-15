use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub idle_threshold_minutes: u32,
    pub autostart_enabled: bool,
    pub sync_enabled: bool,
    #[serde(default)]
    pub sync_url: String,
    #[serde(default)]
    pub sync_token: String,
    #[serde(default = "default_sync_interval")]
    pub sync_interval_minutes: u32,
    #[serde(default)]
    pub onboarded: bool,
}

fn default_sync_interval() -> u32 {
    15
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            idle_threshold_minutes: 5,
            autostart_enabled: false,
            sync_enabled: false,
            sync_url: String::new(),
            sync_token: String::new(),
            sync_interval_minutes: default_sync_interval(),
            onboarded: false,
        }
    }
}
