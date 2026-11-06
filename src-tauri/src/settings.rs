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
    // 数据保留期（天）：0 表示永久保留；大于 0 时只保留最近 N 天，其余在保存设置/启动时清理
    #[serde(default)]
    pub retention_days: u32,
    // 菜单栏驻留模式：隐藏 Dock 图标，仅菜单栏常驻
    #[serde(default)]
    pub menubar_only: bool,
    // 每周工作日（0=周日 … 6=周六），默认周一至周五
    #[serde(default = "default_workdays")]
    pub workdays: Vec<u8>,
    // 法定节假日（YYYY-MM-DD）：即使是工作日也不计入达成率
    #[serde(default)]
    pub holidays: Vec<String>,
    // 调休补班日（YYYY-MM-DD）：即使是周末也按工作日统计
    #[serde(default)]
    pub makeup_days: Vec<String>,
    // 应用锁：argon2id PHC 字符串，空表示未启用
    #[serde(default)]
    pub password_hash: String,
    // 恢复码的哈希（同样 argon2id），用于忘记密码时重设
    #[serde(default)]
    pub recovery_hash: String,
    // 暂停状态持久化：重启后仍保持用户上次的暂停/记录选择
    #[serde(default)]
    pub paused: bool,
    // 通知偏好：长时间离开提醒
    #[serde(default = "default_true")]
    pub notify_away: bool,
    // 通知偏好：跨天时的日终小结
    #[serde(default = "default_true")]
    pub notify_daily_summary: bool,
    // 通知偏好：暂停太久的提醒
    #[serde(default = "default_true")]
    pub notify_pause: bool,
    // 免打扰时段（HH:MM），处于该区间内不弹通知
    #[serde(default = "default_quiet_start")]
    pub quiet_start: String,
    #[serde(default = "default_quiet_end")]
    pub quiet_end: String,
    // 周报推送地址（企业微信/飞书/钉钉机器人 webhook），空表示不推送
    #[serde(default)]
    pub report_webhook: String,
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

fn default_workdays() -> Vec<u8> {
    vec![1, 2, 3, 4, 5]
}

fn default_true() -> bool {
    true
}

fn default_quiet_start() -> String {
    "22:00".into()
}

fn default_quiet_end() -> String {
    "08:00".into()
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
            retention_days: 0,
            menubar_only: true,
            workdays: default_workdays(),
            holidays: Vec::new(),
            makeup_days: Vec::new(),
            password_hash: String::new(),
            recovery_hash: String::new(),
            paused: false,
            notify_away: true,
            notify_daily_summary: true,
            notify_pause: true,
            quiet_start: default_quiet_start(),
            quiet_end: default_quiet_end(),
            report_webhook: String::new(),
        }
    }
}
