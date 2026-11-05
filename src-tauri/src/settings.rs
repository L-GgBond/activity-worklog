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
    // 窗口尺寸记忆（0 表示未设置，使用默认）
    #[serde(default)]
    pub window_width: u32,
    #[serde(default)]
    pub window_height: u32,
    // 外观主题：system / light / dark
    #[serde(default = "default_theme")]
    pub theme: String,
    // 每日目标活跃时长（分钟），0 表示不设置目标
    #[serde(default = "default_daily_target")]
    pub daily_target_minutes: u32,
    // 菜单栏驻留模式：隐藏 Dock 图标，仅菜单栏常驻
    #[serde(default)]
    pub menubar_only: bool,
    // 应用锁：argon2id PHC 字符串，空表示未启用
    #[serde(default)]
    pub password_hash: String,
    // 恢复码的哈希（同样 argon2id），用于忘记密码时重设
    #[serde(default)]
    pub recovery_hash: String,
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

fn default_theme() -> String {
    "system".into()
}

fn default_daily_target() -> u32 {
    480
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
            window_width: 0,
            window_height: 0,
            theme: default_theme(),
            daily_target_minutes: default_daily_target(),
            menubar_only: true,
            password_hash: String::new(),
            recovery_hash: String::new(),
        }
    }
}
