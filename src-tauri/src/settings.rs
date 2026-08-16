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
    // 作息时间（24 小时制 HH:MM）
    #[serde(default = "default_work_start")]
    pub work_start: String,
    #[serde(default = "default_work_end")]
    pub work_end: String,
    #[serde(default = "default_lunch_start")]
    pub lunch_start: String,
    #[serde(default = "default_lunch_end")]
    pub lunch_end: String,
}

fn default_work_start() -> String {
    "09:30".into()
}

fn default_work_end() -> String {
    "18:30".into()
}

fn default_lunch_start() -> String {
    "12:00".into()
}

fn default_lunch_end() -> String {
    "13:30".into()
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
            work_start: default_work_start(),
            work_end: default_work_end(),
            lunch_start: default_lunch_start(),
            lunch_end: default_lunch_end(),
        }
    }
}
