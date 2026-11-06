mod activity;
mod db;
mod settings;
mod sync;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use activity::InputActivityProbe;
use chrono::{Local, TimeZone};
use db::{segment_stats, ActivitySegment, DailyRecord, HourBucket, Store, SyncSummary};
use settings::AppSettings;
use sync::HttpSyncClient;
use tauri::menu::{Menu, MenuItem, MenuItemKind};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_global_shortcut::GlobalShortcutExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_shell::ShellExt;
use tauri_plugin_updater::UpdaterExt;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

struct AppState {
    store: Mutex<Store>,
    paused: AtomicBool,
    /// 采集缓冲重置标记：1 = 清空重来（删除/清空记录后），2 = 以数据库为准重新加载（恢复备份后）
    resetting: AtomicU8,
    /// 应用锁：未解锁时所有读数据的命令都会被拒绝
    unlocked: AtomicBool,
    failed_unlocks: AtomicU32,
    cooldown_until: Mutex<Option<Instant>>,
    /// 认证节流状态的落盘位置（重启后仍能续上冷却）
    auth_throttle_path: PathBuf,
}

/// 是否设置了应用锁密码
fn has_password(state: &State<AppState>) -> bool {
    state
        .store
        .lock()
        .ok()
        .and_then(|store| store.get_settings().ok())
        .map(|settings| !settings.password_hash.is_empty())
        .unwrap_or(false)
}

fn ensure_unlocked(state: &State<AppState>) -> Result<(), String> {
    if state.unlocked.load(Ordering::Relaxed) {
        Ok(())
    } else {
        Err("应用已锁定，请先解锁".into())
    }
}

/// 认证节流状态落盘：失败次数与冷却截止时间不能只放内存，
/// 否则重启应用就能绕开 5 次失败后的 60 秒冷却。
/// 单独放数据目录的小 JSON，不写进数据库（恢复备份时不应被业务数据覆盖）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
struct AuthThrottle {
    failed_attempts: u32,
    /// Unix 秒；0 表示当前不在冷却中
    cooldown_until_unix: i64,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

fn read_auth_throttle(path: &Path) -> AuthThrottle {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<AuthThrottle>(&text).ok())
        .unwrap_or_default()
}

/// 尽力而为地写盘：磁盘不可写时只退化为「重启后重置计数」，不阻断解锁流程。
fn write_auth_throttle(path: &Path, failed_attempts: u32, cooldown: Option<Instant>) {
    let cooldown_until_unix = match cooldown {
        Some(until) => until
            .checked_duration_since(Instant::now())
            .map(|left| now_unix() + left.as_secs() as i64 + 1)
            .unwrap_or(0),
        None => 0,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(&AuthThrottle {
        failed_attempts,
        cooldown_until_unix,
    }) {
        let _ = std::fs::write(path, text);
    }
}

/// 应用锁认证的统一限流参数
const AUTH_MAX_FAILS: u32 = 5;
const AUTH_COOLDOWN_SECS: u64 = 60;

impl AppState {
    /// 所有需要校验密码/恢复码的命令共用同一套冷却检查，
    /// 避免 unlock 限流但 set_password / disable_lock 不限流被绕过。
    fn check_auth_cooldown(&self) -> Result<(), String> {
        if let Some(until) = *self
            .cooldown_until
            .lock()
            .map_err(|_| "state lock failed".to_string())?
        {
            if Instant::now() < until {
                let left = (until - Instant::now()).as_secs() + 1;
                return Err(format!("尝试次数过多，请 {left} 秒后再试"));
            }
        }
        Ok(())
    }

    /// 记一次认证失败；达到阈值时进入冷却，返回要展示给前端的错误信息
    fn register_auth_failure(&self, label: &str) -> String {
        let fails = self.failed_unlocks.fetch_add(1, Ordering::Relaxed) + 1;
        if fails >= AUTH_MAX_FAILS {
            self.failed_unlocks.store(0, Ordering::Relaxed);
            let until = Instant::now() + Duration::from_secs(AUTH_COOLDOWN_SECS);
            if let Ok(mut slot) = self.cooldown_until.lock() {
                *slot = Some(until);
            }
            write_auth_throttle(&self.auth_throttle_path, 0, Some(until));
            return "认证连续失败 5 次，请 60 秒后再试".to_string();
        }
        write_auth_throttle(&self.auth_throttle_path, fails, None);
        format!("{label}，还可尝试 {} 次", AUTH_MAX_FAILS - fails)
    }

    fn clear_auth_failures(&self) {
        self.failed_unlocks.store(0, Ordering::Relaxed);
        if let Ok(mut until) = self.cooldown_until.lock() {
            *until = None;
        }
        write_auth_throttle(&self.auth_throttle_path, 0, None);
    }
}

fn hash_secret(secret: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| err.to_string())
}

fn verify_secret(secret: &str, phc: &str) -> bool {
    PasswordHash::new(phc)
        .map(|parsed| {
            Argon2::default()
                .verify_password(secret.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

/// 16 位恢复码，去掉容易看错的 0/O/1/I，形如 ABCD-EFGH-JKLM-NPQR
fn generate_recovery_code() -> String {
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
    let mut bytes = [0u8; 16];
    let mut rng = OsRng;
    rng.fill_bytes(&mut bytes);
    let chars: Vec<char> = bytes
        .iter()
        .map(|byte| ALPHABET[(*byte as usize) % ALPHABET.len()] as char)
        .collect();
    chars
        .chunks(4)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

#[tauri::command]
fn list_daily_records(
    state: State<AppState>,
    limit: Option<u32>,
) -> Result<Vec<DailyRecord>, String> {
    ensure_unlocked(&state)?;
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .list_daily_records(limit.unwrap_or(366))
        .map_err(|err| err.to_string())
}

/// 历史总天数：配合 list_daily_records 的窗口做「加载更早」
#[tauri::command]
fn count_daily_records(state: State<AppState>) -> Result<i64, String> {
    ensure_unlocked(&state)?;
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .count_daily_records()
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn list_segments(state: State<AppState>, date: String) -> Result<Vec<ActivitySegment>, String> {
    ensure_unlocked(&state)?;
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .list_segments_by_date(&date)
        .map_err(|err| err.to_string())
}

/// 手动修正某天的时段：用整段列表覆盖并基于段落重算当日统计。
/// 今天被编辑时让采集循环从库里重载，避免被内存里的旧段落覆盖。
#[tauri::command]
fn save_segments(
    app: AppHandle,
    state: State<AppState>,
    date: String,
    segments: Vec<ActivitySegment>,
) -> Result<(), String> {
    ensure_unlocked(&state)?;
    {
        let store = state
            .store
            .lock()
            .map_err(|_| "store lock failed".to_string())?;
        store
            .replace_day_segments(&date, &segments)
            .map_err(|err| err.to_string())?;
    }
    if date == Local::now().format("%Y-%m-%d").to_string() {
        state.resetting.store(2, Ordering::Relaxed);
    }
    let _ = app.emit("records-updated", ());
    Ok(())
}

#[tauri::command]
fn get_settings(state: State<AppState>) -> Result<AppSettings, String> {
    // 锁屏态禁止读取设置：否则可通过 IPC 绕过应用锁拿到 sync_url / sync_token 等凭据
    ensure_unlocked(&state)?;
    let mut settings = state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .get_settings()
        .map_err(|err| err.to_string())?;
    // 密码/恢复码哈希、同步 Token 均不下发到前端：
    // 哈希由 save_settings 沿用库里的值；sync_token 按密码式处理（留空表示不修改）
    settings.password_hash.clear();
    settings.recovery_hash.clear();
    settings.sync_token.clear();
    Ok(settings)
}

#[tauri::command]
fn save_settings(
    settings: AppSettings,
    app: AppHandle,
    state: State<AppState>,
) -> Result<(), String> {
    ensure_unlocked(&state)?;
    // 自启注册失败不应阻断设置保存（例如系统目录不可写）：
    // 先尽力同步自启状态并记录，再照常落库，下次保存会重试。
    let autostart = app.autolaunch();
    let autostart_result = if settings.autostart_enabled {
        autostart.enable()
    } else {
        autostart.disable()
    };
    if let Err(err) = autostart_result {
        eprintln!("同步开机自启状态失败: {err}");
    }

    // 前端不持有密码哈希，保存时必须沿用库里已有的值，
    // 否则改主题这类操作会把应用锁清空。
    let mut next = settings;
    {
        let store = state
            .store
            .lock()
            .map_err(|_| "store lock failed".to_string())?;
        let existing = store.get_settings().map_err(|err| err.to_string())?;
        next.password_hash = existing.password_hash;
        next.recovery_hash = existing.recovery_hash;
        // sync_token 密码式处理：留空表示不修改，沿用库里已存的值；非空才更新
        if next.sync_token.trim().is_empty() {
            next.sync_token = existing.sync_token;
        }
        store
            .save_settings(&next)
            .map_err(|err| err.to_string())?;
    }
    // 数据保留期：保存设置后立即清理过期记录，并通知前端刷新列表
    if next.retention_days > 0 {
        let cutoff = (Local::now().date_naive()
            - chrono::Duration::days(next.retention_days as i64))
            .format("%Y-%m-%d")
            .to_string();
        if let Ok(store) = state.store.lock() {
            let _ = store.purge_old_records(&cutoff);
        }
        let _ = app.emit("records-updated", ());
    }
    Ok(())
}

#[tauri::command]
fn delete_day(app: AppHandle, state: State<AppState>, date: String) -> Result<(), String> {
    ensure_unlocked(&state)?;
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .delete_day(&date)
        .map_err(|err| err.to_string())?;
    // 删的是今天时，采集循环内存里还留着当天段落，需要让它重新开始
    if date == Local::now().format("%Y-%m-%d").to_string() {
        state.resetting.store(1, Ordering::Relaxed);
    }
    let _ = app.emit("records-updated", ());
    Ok(())
}

#[tauri::command]
fn clear_all_records(app: AppHandle, state: State<AppState>) -> Result<String, String> {
    ensure_unlocked(&state)?;
    let removed = state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .clear_records()
        .map_err(|err| err.to_string())?;
    state.resetting.store(1, Ordering::Relaxed);
    let _ = app.emit("records-updated", ());
    Ok(format!("已清除 {removed} 天记录"))
}

#[tauri::command]
fn sync_summary(state: State<AppState>) -> Result<SyncSummary, String> {
    ensure_unlocked(&state)?;
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .sync_summary()
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn export_records_csv(state: State<AppState>) -> Result<String, String> {
    ensure_unlocked(&state)?;
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .export_csv()
        .map(|path| path.to_string_lossy().to_string())
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn hourly_activity(state: State<AppState>, range_days: u32) -> Result<Vec<HourBucket>, String> {
    ensure_unlocked(&state)?;
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .hourly_activity(range_days)
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn export_db(app: AppHandle) -> Result<String, String> {
    let state = app.state::<AppState>();
    ensure_unlocked(&state)?;
    let base = dirs::download_dir().ok_or_else(|| "无法定位下载目录".to_string())?;
    let dir = base.join("Activity Worklog");
    std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    let dest = dir.join(format!(
        "worklog-backup-{}.sqlite3",
        Local::now().format("%Y%m%d-%H%M%S")
    ));
    let store = state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?;
    store.backup_to(&dest).map_err(|err| err.to_string())?;
    notify(&app, "已创建数据备份（SQLite 热备）");
    Ok(dest.to_string_lossy().to_string())
}

#[tauri::command]
async fn restore_db(app: AppHandle) -> Result<String, String> {
    ensure_unlocked(&app.state::<AppState>())?;
    #[cfg(target_os = "macos")]
    {
        let out = app
            .shell()
            .command("osascript")
            .args([
                "-e",
                "POSIX path of (choose file with prompt \"选择备份的 worklog.sqlite3 文件\")",
            ])
            .output()
            .await
            .map_err(|err| err.to_string())?;
        let src = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if src.is_empty() {
            return Err("未选择文件".into());
        }
        let state = app.state::<AppState>();
        {
            let mut store = state
                .store
                .lock()
                .map_err(|_| "store lock failed".to_string())?;
            store
                .restore_from(std::path::Path::new(&src))
                .map_err(|err| err.to_string())?;
        }
        // 恢复后以数据库为准：让采集循环重新加载当天段落，
        // 否则内存里的旧段落会在下一个采样周期把恢复结果覆盖掉
        state.resetting.store(2, Ordering::Relaxed);
        notify(&app, "已从备份恢复，列表已刷新");
        Ok("已从备份恢复，列表已刷新".into())
    }

    #[cfg(not(target_os = "macos"))]
    {
        let db_path = {
            let state = app.state::<AppState>();
            let store = state
                .store
                .lock()
                .map_err(|_| "store lock failed".to_string())?;
            store.db_path()
        };
        Err(format!(
            "当前系统暂未接入文件选择器。请先退出应用，再用备份文件覆盖此文件后重新打开：{}",
            db_path.to_string_lossy()
        ))
    }
}

#[tauri::command]
fn export_json(state: State<AppState>) -> Result<String, String> {
    ensure_unlocked(&state)?;
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .export_json()
        .map(|path| path.to_string_lossy().to_string())
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn import_json(app: AppHandle, content: String) -> Result<u32, String> {
    ensure_unlocked(&app.state::<AppState>())?;
    let imported = {
        let state = app.state::<AppState>();
        let store = state
            .store
            .lock()
            .map_err(|_| "store lock failed".to_string())?;
        store
            .import_json(&content)
            .map_err(|err| err.to_string())?
    };
    // 导入后以数据库为准：让采集循环重新加载当天段落，避免内存里旧段落覆盖导入结果
    let state = app.state::<AppState>();
    state.resetting.store(2, Ordering::Relaxed);
    let _ = app.emit("records-updated", ());
    Ok(imported)
}

/// 检测更新：仅做版本比对与下载链接提示，不在本机触发安装（安装/签名交由分发流程处理）。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo {
    status: String,
    current: String,
    latest: Option<String>,
    notes: Option<String>,
    url: Option<String>,
}

#[tauri::command]
async fn check_update(app: AppHandle) -> Result<UpdateInfo, String> {
    let current = app.package_info().version.to_string();
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => Ok(UpdateInfo {
            status: "available".into(),
            current,
            latest: Some(update.version.to_string()),
            notes: update.body,
            url: Some(update.download_url.to_string()),
        }),
        Ok(None) => Ok(UpdateInfo {
            status: "latest".into(),
            current: current.clone(),
            latest: Some(current),
            notes: None,
            url: None,
        }),
        Err(err) => Ok(UpdateInfo {
            status: "error".into(),
            current,
            latest: None,
            notes: Some(err.to_string()),
            url: None,
        }),
    }
}

/// 跨平台系统通知（Windows / macOS / Linux 均可用，依赖 tauri-plugin-notification）。
/// macOS 首次需用户在「系统设置 → 通知 → Activity Worklog」授权，未授权时静默失败。
fn notify(app: &AppHandle, message: &str) {
    let _ = app
        .notification()
        .builder()
        .title("Activity Worklog")
        .body(message)
        .show();
}

/// 立即把待同步记录推一次，不等定时器。
#[tauri::command]
async fn sync_now(app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    ensure_unlocked(&state)?;
    let settings = {
        let store = state
            .store
            .lock()
            .map_err(|_| "store lock failed".to_string())?;
        store.get_settings().map_err(|err| err.to_string())?
    };
    if !settings.sync_enabled {
        return Err("请先启用同步".into());
    }
    if settings.sync_url.trim().is_empty() {
        return Err("请先填写服务器地址".into());
    }

    let pending = {
        let store = state
            .store
            .lock()
            .map_err(|_| "store lock failed".to_string())?;
        store.pending_records().map_err(|err| err.to_string())?
    };
    if pending.is_empty() {
        return Ok("没有待同步的记录".into());
    }

    let client = HttpSyncClient::new();
    let accepted = client
        .push_records(&settings, pending)
        .await
        .map_err(|err| err.to_string())?;
    if accepted.is_empty() {
        // 服务端逐条确认但 0 条被接受：保留 pending，等待下轮重试
        return Ok("服务器未确认任何记录，已保留待同步".into());
    }

    {
        let store = state
            .store
            .lock()
            .map_err(|_| "store lock failed".to_string())?;
        store
            .mark_synced(&accepted)
            .map_err(|err| err.to_string())?;
    }
    let _ = app.emit("records-updated", ());
    Ok(format!("已同步 {} 天记录", accepted.len()))
}

/// 从服务器拉取历史日汇总，只补本地缺失的日期（换机 / 重装 / 误删后恢复）。
#[tauri::command]
async fn pull_from_server(
    app: AppHandle,
    state: State<'_, AppState>,
    days: u32,
) -> Result<String, String> {
    ensure_unlocked(&state)?;
    let settings = {
        let store = state
            .store
            .lock()
            .map_err(|_| "store lock failed".to_string())?;
        store.get_settings().map_err(|err| err.to_string())?
    };
    if !settings.sync_enabled {
        return Err("请先启用同步".into());
    }
    if settings.sync_url.trim().is_empty() {
        return Err("请先填写服务器地址".into());
    }

    let client = HttpSyncClient::new();
    let records = client
        .pull_records(&settings, days)
        .await
        .map_err(|err| err.to_string())?;
    let inserted = {
        let store = state
            .store
            .lock()
            .map_err(|_| "store lock failed".to_string())?;
        store
            .import_missing_records(&records)
            .map_err(|err| err.to_string())?
    };
    let _ = app.emit("records-updated", ());
    Ok(format!("已从服务器补回 {inserted} 天记录（仅缺失日期）"))
}

/// 把用户输入的恢复码归一化：忽略大小写、横线与空格，统一成 4 位一组
fn normalize_code(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase();
    cleaned
        .as_bytes()
        .chunks(4)
        .map(|chunk| String::from_utf8_lossy(chunk).to_string())
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthState {
    has_password: bool,
    unlocked: bool,
}

#[tauri::command]
fn auth_state(state: State<AppState>) -> AuthState {
    AuthState {
        has_password: has_password(&state),
        unlocked: state.unlocked.load(Ordering::Relaxed),
    }
}

#[tauri::command]
fn unlock(app: AppHandle, state: State<AppState>, password: String) -> Result<(), String> {
    state.check_auth_cooldown()?;

    let phc = state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .get_settings()
        .map_err(|err| err.to_string())?
        .password_hash;

    if phc.is_empty() {
        state.unlocked.store(true, Ordering::Relaxed);
        return Ok(());
    }

    if verify_secret(&password, &phc) {
        state.clear_auth_failures();
        state.unlocked.store(true, Ordering::Relaxed);
        let _ = app.emit("unlocked", ());
        set_tray_lock_status(&app, false);
        return Ok(());
    }

    Err(state.register_auth_failure("密码不正确"))
}

#[tauri::command]
fn lock(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    if !has_password(&state) {
        return Ok(());
    }
    state.unlocked.store(false, Ordering::Relaxed);
    let _ = app.emit("locked", ());
    Ok(())
}

/// 设置或修改密码，返回一次性恢复码（只在此处返回，之后只存哈希）
#[tauri::command]
fn set_password(state: State<AppState>, current: String, next: String) -> Result<String, String> {
    state.check_auth_cooldown()?;
    let next = next.trim().to_string();
    if next.chars().count() < 6 {
        return Err("密码至少 6 位".into());
    }

    let mut settings = state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .get_settings()
        .map_err(|err| err.to_string())?;

    if !settings.password_hash.is_empty() && !verify_secret(&current, &settings.password_hash) {
        return Err(state.register_auth_failure("当前密码不正确"));
    }

    let recovery = generate_recovery_code();
    settings.password_hash = hash_secret(&next)?;
    settings.recovery_hash = hash_secret(&recovery)?;
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .save_settings(&settings)
        .map_err(|err| err.to_string())?;
    state.clear_auth_failures();
    state.unlocked.store(true, Ordering::Relaxed);
    Ok(recovery)
}

#[tauri::command]
fn disable_lock(state: State<AppState>, current: String) -> Result<(), String> {
    state.check_auth_cooldown()?;
    let mut settings = state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .get_settings()
        .map_err(|err| err.to_string())?;
    if settings.password_hash.is_empty() {
        return Ok(());
    }
    if !verify_secret(&current, &settings.password_hash) {
        return Err(state.register_auth_failure("密码不正确"));
    }
    settings.password_hash.clear();
    settings.recovery_hash.clear();
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .save_settings(&settings)
        .map_err(|err| err.to_string())?;
    state.clear_auth_failures();
    state.unlocked.store(true, Ordering::Relaxed);
    Ok(())
}

/// 用恢复码重设密码，成功后轮换出一个新的恢复码
#[tauri::command]
fn reset_password_with_recovery(
    state: State<AppState>,
    code: String,
    next: String,
) -> Result<String, String> {
    state.check_auth_cooldown()?;
    let next = next.trim().to_string();
    if next.chars().count() < 6 {
        return Err("密码至少 6 位".into());
    }

    let mut settings = state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .get_settings()
        .map_err(|err| err.to_string())?;

    if settings.recovery_hash.is_empty() {
        return Err("没有可用的恢复码".into());
    }
    if !verify_secret(&normalize_code(&code), &settings.recovery_hash) {
        return Err(state.register_auth_failure("恢复码不正确"));
    }

    let recovery = generate_recovery_code();
    settings.password_hash = hash_secret(&next)?;
    settings.recovery_hash = hash_secret(&recovery)?;
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .save_settings(&settings)
        .map_err(|err| err.to_string())?;
    state.clear_auth_failures();
    state.unlocked.store(true, Ordering::Relaxed);
    Ok(recovery)
}

#[tauri::command]
async fn test_sync_connection(settings: AppSettings) -> Result<String, String> {
    let client = HttpSyncClient::new();
    client
        .test_connection(&settings)
        .await
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn is_paused(state: State<AppState>) -> bool {
    state.paused.load(Ordering::Relaxed)
}

#[tauri::command]
fn toggle_pause(state: State<AppState>) -> Result<bool, String> {
    let now_paused = !state.paused.fetch_xor(true, Ordering::Relaxed);
    if now_paused {
        // 暂停时立即关闭开放段落，避免恢复后出现跨越暂停期的假工作段
        let mut store = state.store.lock().map_err(|_| "store lock failed".to_string())?;
        let mut segments = store
            .load_segments_for_today()
            .map_err(|err| err.to_string())?;
        store
            .close_open_segments(&mut segments, Local::now())
            .map_err(|err| err.to_string())?;
    }
    Ok(now_paused)
}

/// 当前数据库文件的绝对路径：设置页直接展示，用户自己备份/迁移时不用再猜位置。
#[tauri::command]
fn db_path(state: State<'_, AppState>) -> Result<String, String> {
    ensure_unlocked(&state)?;
    let store = state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?;
    Ok(store.db_path().to_string_lossy().to_string())
}

/// 在系统文件管理器中打开数据目录（Windows 资源管理器 / macOS Finder / Linux 文件管理器）。
#[tauri::command]
fn open_data_dir(state: State<'_, AppState>) -> Result<String, String> {
    ensure_unlocked(&state)?;
    let dir = {
        let store = state
            .store
            .lock()
            .map_err(|_| "store lock failed".to_string())?;
        store
            .db_path()
            .parent()
            .map(|path| path.to_path_buf())
            .ok_or_else(|| "无法定位数据目录".to_string())?
    };

    #[cfg(target_os = "windows")]
    let opened = {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("explorer")
            .arg(&dir)
            // CREATE_NO_WINDOW：避免每次打开目录闪一个黑色控制台窗口
            .creation_flags(0x0800_0000)
            .spawn()
    };
    #[cfg(target_os = "macos")]
    let opened = std::process::Command::new("open").arg(&dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let opened = std::process::Command::new("xdg-open").arg(&dir).spawn();

    opened.map_err(|err| format!("打开数据目录失败：{err}"))?;
    Ok(dir.to_string_lossy().to_string())
}

/// 将前端生成的周报字节写入「下载 / Activity Worklog」目录，返回完整路径。
/// ext 为扩展名（不含点），如 "png" / "pdf"。
#[tauri::command]
fn export_report(filename: String, data: Vec<u8>, ext: String) -> Result<String, String> {
    // 文件名/扩展名来自前端，这里做白名单清洗，防止路径穿越
    let safe_name: String = filename
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '·') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim()
        .to_string();
    let safe_name = if safe_name.is_empty() {
        "report".to_string()
    } else {
        safe_name
    };
    let ext = ext.trim().trim_start_matches('.').to_lowercase();
    if !["png", "pdf", "jpg", "csv"].contains(&ext.as_str()) {
        return Err("不支持的导出格式".into());
    }
    let base = dirs::download_dir().ok_or_else(|| "无法定位下载目录".to_string())?;
    let dir = base.join("Activity Worklog");
    std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    let path = dir.join(format!("{safe_name}.{ext}"));
    std::fs::write(&path, &data).map_err(|err| err.to_string())?;
    Ok(path.to_string_lossy().to_string())
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", "状态：已解锁 🔓", false, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "显示记录", true, None::<&str>)?;
    let summary = MenuItem::with_id(app, "summary", "今日小结", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", "暂停记录", true, None::<&str>)?;
    let lock = MenuItem::with_id(app, "lock", "锁定并隐藏", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&status, &show, &summary, &pause, &lock, &quit])?;

    let icon = app
        .default_window_icon()
        .cloned()
        .unwrap_or_else(|| panic!("缺少默认窗口图标，无法创建托盘"));

    TrayIconBuilder::with_id("main")
        .icon(icon)
        .icon_as_template(true)
        .menu(&menu)
        .tooltip("Activity Worklog 正在后台记录活动状态")
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "summary" => {
                let _ = app.emit("tray-summary", ());
                show_main_window(app);
            }
            "pause" => {
                let state = app.state::<AppState>();
                if let Ok(now_paused) = toggle_pause(state) {
                    if let Some(MenuItemKind::MenuItem(item)) = menu.get("pause") {
                        let _ = item.set_text(if now_paused { "恢复记录" } else { "暂停记录" });
                    }
                }
            }
            "lock" => {
                let state = app.state::<AppState>();
                if has_password(&state) {
                    state.unlocked.store(false, Ordering::Relaxed);
                    let _ = app.emit("locked", ());
                    // 托盘在锁屏之外，锁定后不能继续暴露今日活跃数据
                    if let Some(tray) = app.tray_by_id("main") {
                        let _ = tray.set_tooltip(Some("Activity Worklog · 已锁定"));
                    }
                    set_tray_lock_status(app, true);
                }
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            "quit" => {
                // 双保险：部分平台 app.exit 不一定派发 ExitRequested，这里先收口再退出
                flush_before_exit(app);
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// 在托盘菜单顶部展示锁定状态文字（已锁定 / 已解锁），让隐藏入口下的安全状态可见。
fn set_tray_lock_status(app: &AppHandle, locked: bool) {
    if let Some(tray) = app.tray_by_id("main") {
        if let Some(menu) = tray.menu() {
            if let Some(MenuItemKind::MenuItem(item)) = menu.get("status") {
                let _ = item.set_text(if locked {
                    "状态：已锁定 🔒"
                } else {
                    "状态：已解锁 🔓"
                });
            }
        }
    }
}

fn spawn_activity_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let probe = InputActivityProbe::new();
        let mut current_date = Local::now().date_naive();
        let mut was_paused = false;
        let mut segments: Vec<ActivitySegment> = {
            let state = app.state::<AppState>();
            state
                .store
                .lock()
                .ok()
                .and_then(|store| store.load_segments_for_today().ok())
                .unwrap_or_default()
        };

        let mut away_notified = false;
        let mut probe_failures: u32 = 0;

        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;

            let now = Local::now();
            let state = app.state::<AppState>();

            if state.paused.load(Ordering::Relaxed) {
                was_paused = true;
                continue;
            }

            // 从暂停恢复：重新从数据库加载段落，避免把暂停期间算成工作时长
            if was_paused {
                was_paused = false;
                segments = state
                    .store
                    .lock()
                    .ok()
                    .and_then(|store| store.load_segments_for_today().ok())
                    .unwrap_or_default();
            }

            // 跨天：关闭上一天的段落，清空缓冲，开始新的一天
            if now.date_naive() != current_date {
                let close_ts = current_date
                    .and_hms_opt(23, 59, 59)
                    .and_then(|naive| Local.from_local_datetime(&naive).single())
                    .unwrap_or(now);
                if let Ok(mut store) = state.store.lock() {
                    let _ = store.close_open_segments(&mut segments, close_ts);
                }
                segments.clear();
                current_date = now.date_naive();
            }

            let settings = state
                .store
                .lock()
                .ok()
                .and_then(|store| store.get_settings().ok())
                .unwrap_or_default();

            // 采集缓冲重置：1 = 清空重来（删除/清空记录），2 = 以数据库为准重新加载（恢复备份）
            match state.resetting.swap(0, Ordering::Relaxed) {
                1 => {
                    segments.clear();
                    continue;
                }
                2 => {
                    if let Ok(store) = state.store.lock() {
                        match store.load_segments_for_today() {
                            Ok(loaded) => segments = loaded,
                            Err(_) => segments.clear(),
                        }
                    } else {
                        segments.clear();
                    }
                    continue;
                }
                _ => {}
            }

            let idle_for = match probe.idle_duration() {
                Ok(value) => {
                    if probe_failures >= 3 {
                        let _ = app.emit("capture-ok", ());
                    }
                    probe_failures = 0;
                    value
                }
                Err(err) => {
                    probe_failures += 1;
                    // 连续失败才提示，避免偶发抖动打扰
                    if probe_failures == 3 {
                        let _ = app.emit("capture-error", err.to_string());
                    }
                    continue;
                }
            };

            // 超过 12 小时无任何输入视为机器关闭/休眠，不计入离开时长
            if idle_for > Duration::from_secs(12 * 3600) {
                continue;
            }

            let threshold = Duration::from_secs(u64::from(settings.idle_threshold_minutes.max(1)) * 60);
            let in_work_session = idle_for <= threshold;

            // 长时间离开提醒：累计离开达 30 分钟时弹一次系统通知
            if idle_for >= Duration::from_secs(30 * 60) {
                if !away_notified {
                    away_notified = true;
                    notify(&app, "已离开约 30 分钟，记得起来活动一下");
                }
            } else {
                away_notified = false;
            }

            // 新段落起点回溯到最后一次真实输入时刻（now - idle_for）
            let backdate = Some(idle_for);

            let day_changed = match state.store.lock() {
                Ok(mut store) => store
                    .record_activity(now, &mut segments, in_work_session, backdate)
                    .unwrap_or(false),
                Err(_) => false,
            };
            // 只有当天统计真的变化时才通知前端刷新
            if day_changed {
                let _ = app.emit("records-updated", ());
            }

            // 托盘 tooltip 直接用内存段落计算今日活跃占比，不再每个采样周期查一遍库
            let today = now.format("%Y-%m-%d").to_string();
            let stats = segment_stats(&segments, &today);
            let total = stats.active_minutes + stats.idle_minutes;
            let pct = if total > 0 {
                ((stats.active_minutes * 100) / total) as u32
            } else {
                0
            };
            // 锁定态下托盘只显示中性文案，不泄露统计信息
            let tooltip = if state.unlocked.load(Ordering::Relaxed) {
                format!("Activity Worklog · 今日活跃 {pct}%")
            } else {
                "Activity Worklog · 已锁定".to_string()
            };
            if let Some(tray) = app.tray_by_id("main") {
                let _ = tray.set_tooltip(Some(tooltip));
            }
        }
    });
}

fn spawn_sync_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let client = HttpSyncClient::new();
        let mut next_sync_at: Option<std::time::Instant> = None;

        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;

            let settings = {
                let state = app.state::<AppState>();
                state
                    .store
                    .lock()
                    .ok()
                    .and_then(|store| store.get_settings().ok())
                    .unwrap_or_default()
            };

            if !settings.sync_enabled || settings.sync_url.trim().is_empty() {
                next_sync_at = None;
                continue;
            }

            let interval =
                Duration::from_secs(u64::from(settings.sync_interval_minutes.max(1)) * 60);
            if let Some(deadline) = next_sync_at {
                if std::time::Instant::now() < deadline {
                    continue;
                }
            }
            next_sync_at = Some(std::time::Instant::now() + interval);

            let pending: Vec<DailyRecord> = {
                let state = app.state::<AppState>();
                state
                    .store
                    .lock()
                    .ok()
                    .and_then(|store| store.pending_records().ok())
                    .unwrap_or_default()
            };
            if pending.is_empty() {
                continue;
            }

            match client.push_records(&settings, pending).await {
                Ok(accepted) => {
                    if accepted.is_empty() {
                        // 服务端逐条确认但 0 条被接受：保留 pending，短期退避后重试
                        next_sync_at = Some(std::time::Instant::now() + Duration::from_secs(60));
                    } else {
                        // 只标记服务端确认过的日期，避免“假已同步”
                        let state = app.state::<AppState>();
                        if let Ok(store) = state.store.lock() {
                            let _ = store.mark_synced(&accepted);
                        }
                        let _ = app.emit("records-updated", ());
                    }
                }
                Err(err) => {
                    // 失败退避：1 分钟后重试，不等满一个同步周期
                    next_sync_at = Some(std::time::Instant::now() + Duration::from_secs(60));
                    let _ = app.emit("sync-error", err.to_string());
                }
            }
        }
    });
}

/// 退出前把当天还在进行中的段落收口落库，
/// 避免「最后一次采样 → 退出」之间的时长凭空丢失。
fn flush_before_exit(app: &AppHandle) {
    let state = app.state::<AppState>();
    if let Ok(mut store) = state.store.lock() {
        if let Ok(mut segments) = store.load_segments_for_today() {
            let _ = store.close_open_segments(&mut segments, Local::now());
        }
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // 重复启动只把已有窗口叫到前台（若已锁定会先显示锁屏），不再起第二个采集循环
            show_main_window(app);
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            // macOS 上没有应用菜单栏时，WKWebView 拿不到标准的 ⌘C/⌘V/⌘Q，
            // 这里挂上系统默认菜单（仅 macOS，Windows 不需要窗口菜单）。
            #[cfg(target_os = "macos")]
            {
                let menu = Menu::default(app.handle())?;
                menu.set_as_app_menu()?;
            }
            setup_tray(app.handle())?;
            let store = Store::open()?;
            // 读取启动需要的设置：锁定态与保留期都从这里取，避免重复查库
            let settings0 = store.get_settings().unwrap_or_default();
            // 启动时若设过密码，先处于锁定态，前端只渲染锁屏
            let locked = !settings0.password_hash.is_empty();
            // 数据保留期：启动时清理过期记录（0 表示永久保留）
            if settings0.retention_days > 0 {
                let cutoff = (Local::now().date_naive()
                    - chrono::Duration::days(settings0.retention_days as i64))
                    .format("%Y-%m-%d")
                    .to_string();
                let _ = store.purge_old_records(&cutoff);
            }
            // 托盘菜单顶部的锁定状态项与实际启动态对齐
            set_tray_lock_status(app.handle(), locked);
            // 隐藏 Dock 图标，仅菜单栏常驻（仅 macOS 有此机制；需重启生效）
            #[cfg(target_os = "macos")]
            {
                if settings0.menubar_only {
                    let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                }
            }
            // 恢复上次进程遗留的认证节流状态：重启不能当成「失败次数清零」
            let auth_throttle_path = dirs::data_local_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("activity-worklog")
                .join("auth-throttle.json");
            let throttled = read_auth_throttle(&auth_throttle_path);
            let remaining = throttled.cooldown_until_unix.saturating_sub(now_unix());
            let cooldown = if remaining > 0 {
                Some(Instant::now() + Duration::from_secs(remaining as u64))
            } else {
                None
            };
            app.manage(AppState {
                store: Mutex::new(store),
                paused: AtomicBool::new(false),
                resetting: AtomicU8::new(0),
                unlocked: AtomicBool::new(!locked),
                failed_unlocks: AtomicU32::new(throttled.failed_attempts),
                cooldown_until: Mutex::new(cooldown),
                auth_throttle_path,
            });
            // 全局快捷键：Cmd/Ctrl+Shift+A 在隐藏或锁定时也能唤出窗口（锁定时显示锁屏）
            let _ = app.global_shortcut().register("CmdOrCtrl+Shift+A", |handle, _shortcut| {
                show_main_window(handle);
            });
            spawn_activity_loop(app.handle().clone());
            spawn_sync_loop(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭主窗口时不退出进程，隐藏到托盘继续后台运行
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                    // 隐藏窗口的同时上锁，下次打开先看到锁屏
                    let app = window.app_handle();
                    let state = app.state::<AppState>();
                    if has_password(&state) {
                        state.unlocked.store(false, Ordering::Relaxed);
                        let _ = app.emit("locked", ());
                        if let Some(tray) = app.tray_by_id("main") {
                            let _ = tray.set_tooltip(Some("Activity Worklog · 已锁定"));
                        }
                        set_tray_lock_status(&app, true);
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            auth_state,
            unlock,
            lock,
            set_password,
            disable_lock,
            reset_password_with_recovery,
            list_daily_records,
            count_daily_records,
            list_segments,
            save_segments,
            delete_day,
            clear_all_records,
            sync_summary,
            get_settings,
            save_settings,
            export_records_csv,
            test_sync_connection,
            sync_now,
            pull_from_server,
            is_paused,
            toggle_pause,
            export_report,
            db_path,
            open_data_dir,
            hourly_activity,
            export_db,
            restore_db,
            export_json,
            import_json,
            check_update
        ])
        .build(tauri::generate_context!())
        .expect("failed to build app")
        .run(|app, event| {
            // 托盘「退出」、⌘Q、系统关机都会经过这里；先收口当天段落再让进程退出
            if let tauri::RunEvent::ExitRequested { .. } = event {
                flush_before_exit(app);
            }
        });
}
