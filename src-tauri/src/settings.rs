use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub idle_threshold_minutes: u32,
    pub autostart_enabled: bool,
    pub sync_enabled: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            idle_threshold_minutes: 5,
            autostart_enabled: false,
            sync_enabled: false,
        }
    }
}
