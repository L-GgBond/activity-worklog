mod activity;
mod db;
mod logging;
mod settings;
mod sync;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use activity::InputActivityProbe;
use chrono::{NaiveDate, Datelike, Local, TimeZone, Timelike};
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
    /// 会议/专注模式截止时间（unix 秒）：在此之前强制按“工作”计算，0 表示未开启
    focus_until: AtomicI64,
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

    // 设置同步到服务端：让服务端报表与客户端使用同一套作息/工作日历口径。
    // 失败只记日志，不阻断本地保存。
    if next.sync_enabled && !next.sync_url.trim().is_empty() {
        let client = HttpSyncClient::new();
        let snapshot = next.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(err) = client.push_settings(&snapshot).await {
                logging::write("WARN", &format!("设置同步失败: {err}"));
            }
        });
    }
    Ok(())
}

#[tauri::command]
async fn delete_day(
    app: AppHandle,
    state: State<'_, AppState>,
    date: String,
) -> Result<(), String> {
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

    // 同步删除到服务器：否则下次「从服务器恢复」会把刚删掉的那天又拉回来。
    // 失败只打日志、不影响本地删除，等用户下次手动同步再处理。
    let settings = {
        let store = state
            .store
            .lock()
            .map_err(|_| "store lock failed".to_string())?;
        store.get_settings().ok()
    };
    if let Some(settings) = settings {
        if settings.sync_enabled && !settings.sync_url.trim().is_empty() {
            let client = HttpSyncClient::new();
            let target = date.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(err) = client.delete_records(&settings, vec![target]).await {
                    eprintln!("[sync] 删除同步失败: {err}");
                    logging::write("WARN", &format!("删除同步失败: {err}"));
                }
            });
        }
    }
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

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BackupFile {
    name: String,
    size: u64,
    modified: String,
}

#[tauri::command]
fn list_backups(state: State<AppState>) -> Result<Vec<BackupFile>, String> {
    ensure_unlocked(&state)?;
    let store = state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?;
    Ok(store
        .list_backups()
        .into_iter()
        .map(|(name, size, modified)| BackupFile {
            name,
            size,
            modified,
        })
        .collect())
}

/// 从自动备份列表中选一份直接恢复（不用再走文件选择器）。
#[tauri::command]
fn restore_named_backup(app: AppHandle, name: String) -> Result<String, String> {
    let state = app.state::<AppState>();
    ensure_unlocked(&state)?;
    {
        let mut store = state
            .store
            .lock()
            .map_err(|_| "store lock failed".to_string())?;
        store
            .restore_named_backup(&name)
            .map_err(|err| err.to_string())?;
    }
    state.resetting.store(2, Ordering::Relaxed);
    notify(&app, "已从备份恢复，列表已刷新");
    logging::write("INFO", &format!("从备份 {name} 恢复成功"));
    Ok("已从备份恢复，列表已刷新".into())
}

/// 发送一条测试通知，同时用来检测系统通知权限是否已授予。
#[tauri::command]
fn notify_test(app: AppHandle) -> Result<(), String> {
    let shown = app
        .notification()
        .builder()
        .title("Activity Worklog")
        .body("这是一条测试通知，能看到说明通知权限正常")
        .show();
    shown.map_err(|err| {
        logging::write("WARN", &format!("测试通知发送失败: {err}"));
        err.to_string()
    })
}

/// 向配置的周报 Webhook 发送一条测试消息，避免地址配错等一周才发现。
#[tauri::command]
async fn test_webhook(url: String) -> Result<(), String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("Webhook 地址为空".into());
    }
    if !trimmed.starts_with("https://") && !trimmed.starts_with("http://") {
        return Err("地址必须以 http:// 或 https:// 开头".into());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|err| err.to_string())?;
    let text = "Activity Worklog 测试消息：周报推送地址配置正常".to_string();
    let payload = serde_json::json!({
        "msgtype": "text",
        "msg_type": "text",
        "text": {"content": text},
        "content": {"text": text},
    });
    let response = client
        .post(trimmed)
        .json(&payload)
        .send()
        .await
        .map_err(|err| format!("推送失败：{err}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!("服务器返回 {status}：{body}"));
    }
    logging::write("INFO", "Webhook 测试推送成功");
    Ok(())
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
    // 失败分类：network / endpoint / signature / other，前端据此给可操作的提示
    error_kind: Option<String>,
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
            error_kind: None,
        }),
        Ok(None) => Ok(UpdateInfo {
            status: "latest".into(),
            current: current.clone(),
            latest: Some(current),
            notes: None,
            url: None,
            error_kind: None,
        }),
        Err(err) => {
            let raw = err.to_string();
            let lower = raw.to_lowercase();
            let kind = if lower.contains("signature") || lower.contains("verify") {
                "signature"
            } else if lower.contains("404")
                || lower.contains("not found")
                || lower.contains("release")
                || lower.contains("target")
            {
                "endpoint"
            } else if lower.contains("dns")
                || lower.contains("connect")
                || lower.contains("timed out")
                || lower.contains("network")
                || lower.contains("resolve")
            {
                "network"
            } else {
                "other"
            };
            Ok(UpdateInfo {
                status: "error".into(),
                current,
                latest: None,
                notes: Some(raw),
                url: None,
                error_kind: Some(kind.into()),
            })
        }
    }
}

/// 一键下载并安装更新：Windows 安装器会自行退出应用，macOS/Linux 安装后自动重启。
/// 前端只需在调用前提示「下载中」，不必再引导用户去浏览器手动下载。
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<String, String> {
    let updater = app.updater().map_err(|err| err.to_string())?;
    let Some(update) = updater.check().await.map_err(|err| err.to_string())? else {
        return Err("已是最新版本，无需安装".into());
    };
    let version = update.version.clone();
    update
        .download_and_install(|_chunk, _total| {}, || {})
        .await
        .map_err(|err| err.to_string())?;
    // Windows 分支在启动安装器后由插件结束进程，这里不会继续执行；
    // macOS / Linux 需要重启才会真正用上新版本。
    #[cfg(not(windows))]
    app.restart();
    Ok(format!("已安装 {version}"))
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

/// 带偏好的通知：自动跳过免打扰时段；enabled=false 时直接不发。
fn notify_if_allowed(app: &AppHandle, settings: &AppSettings, enabled: bool, message: &str) {
    if !enabled || in_quiet_hours(settings) {
        return;
    }
    notify(app, message);
}

/// 当前是否处于免打扰时段。支持跨零点（如 22:00–08:00）。
fn in_quiet_hours(settings: &AppSettings) -> bool {
    let now_minutes = {
        let now = Local::now();
        now.hour() * 60 + now.minute()
    };
    let start = parse_hm_minutes(&settings.quiet_start);
    let end = parse_hm_minutes(&settings.quiet_end);
    match (start, end) {
        (Some(start), Some(end)) => {
            if start == end {
                false
            } else if start < end {
                now_minutes >= start && now_minutes < end
            } else {
                // 跨零点：开始之后或结束之前
                now_minutes >= start || now_minutes < end
            }
        }
        _ => false,
    }
}

/// "HH:MM" → 当天分钟数，非法格式返回 None。
fn parse_hm_minutes(value: &str) -> Option<u32> {
    let mut parts = value.split(':');
    let hour: u32 = parts.next()?.trim().parse().ok()?;
    let minute: u32 = parts.next()?.trim().parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(hour * 60 + minute)
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

    // 同时拉取时段明细：只补本地完全没有时段的日期，换机后历史也能继续编辑。
    let remote_segments = client
        .pull_segments(&settings, days)
        .await
        .unwrap_or_default();
    let segments_inserted = {
        let store = state
            .store
            .lock()
            .map_err(|_| "store lock failed".to_string())?;
        store
            .import_missing_segments(&remote_segments)
            .unwrap_or(0)
    };
    let _ = app.emit("records-updated", ());
    Ok(format!(
        "已从服务器补回 {inserted} 天记录、{segments_inserted} 条时段（仅缺失日期）"
    ))
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
        // 暂停状态持久化：重启后仍保持，避免用户以为没在记录
        let mut persisted = store.get_settings().map_err(|err| err.to_string())?;
        persisted.paused = true;
        store
            .save_settings(&persisted)
            .map_err(|err| err.to_string())?;
    } else {
        // 恢复记录：清掉持久化标记
        if let Ok(store) = state.store.lock() {
            if let Ok(mut persisted) = store.get_settings() {
                persisted.paused = false;
                let _ = store.save_settings(&persisted);
            }
        }
    }
    Ok(now_paused)
}

/// 开启会议/专注模式：minutes 分钟内即使没有键鼠输入也按“工作”计算。
/// minutes 传 0 表示提前关闭。返回截止时间的 unix 秒（0 表示已关闭）。
#[tauri::command]
fn set_focus_mode(state: State<AppState>, minutes: u32) -> Result<i64, String> {
    ensure_unlocked(&state)?;
    let deadline = if minutes == 0 {
        0
    } else {
        now_unix() + i64::from(minutes) * 60
    };
    state.focus_until.store(deadline, Ordering::Relaxed);
    Ok(deadline)
}

/// 读取会议/专注模式截止时间（unix 秒），0 表示未开启。
#[tauri::command]
fn focus_deadline(state: State<AppState>) -> i64 {
    state.focus_until.load(Ordering::Relaxed)
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
        // 暂停提醒做节流，避免每 15 秒弹一次
        let mut last_pause_notify: Option<Instant> = None;

        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;

            let now = Local::now();
            let state = app.state::<AppState>();

            if state.paused.load(Ordering::Relaxed) {
                was_paused = true;
                // 暂停超过 30 分钟提醒一次，之后每 30 分钟再提醒，防止用户忙忘整天没记录
                let pause_settings = state
                    .store
                    .lock()
                    .ok()
                    .and_then(|store| store.get_settings().ok())
                    .unwrap_or_default();
                let due = last_pause_notify
                    .map(|instant| instant.elapsed() >= Duration::from_secs(30 * 60))
                    .unwrap_or(true);
                if due {
                    last_pause_notify = Some(Instant::now());
                    notify_if_allowed(
                        &app,
                        &pause_settings,
                        pause_settings.notify_pause,
                        "记录仍处于暂停状态，别忘了恢复记录",
                    );
                }
                continue;
            }
            last_pause_notify = None;

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
                let ended_key = current_date.format("%Y-%m-%d").to_string();
                if let Ok(mut store) = state.store.lock() {
                    let _ = store.close_open_segments(&mut segments, close_ts);
                }
                // 日终小结：用刚收口的段落统计昨天的活跃时长
                let day_stats = segment_stats(&segments, &ended_key);
                let summary_settings = state
                    .store
                    .lock()
                    .ok()
                    .and_then(|store| store.get_settings().ok())
                    .unwrap_or_default();
                let target = summary_settings.daily_target_minutes;
                let verdict = if target > 0 && day_stats.active_minutes >= target {
                    "已达标"
                } else {
                    "未达目标"
                };
                notify_if_allowed(
                    &app,
                    &summary_settings,
                    summary_settings.notify_daily_summary,
                    &format!(
                        "昨日活跃 {}（{} 分钟，{}）",
                        ended_key, day_stats.active_minutes, verdict
                    ),
                );
                segments.clear();
                current_date = now.date_naive();
                away_notified = false;
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
                        logging::write("ERROR", &format!("活动采集连续失败: {err}"));
                    }
                    continue;
                }
            };

            // 超过 12 小时无任何输入视为机器关闭/休眠，不计入离开时长
            if idle_for > Duration::from_secs(12 * 3600) {
                continue;
            }

            let threshold = Duration::from_secs(u64::from(settings.idle_threshold_minutes.max(1)) * 60);
            // 会议/专注模式：截止时间之前强制算工作，开会无键鼠输入也不会被记成离开
            let focus_active = state.focus_until.load(Ordering::Relaxed) > now_unix();
            let in_work_session = focus_active || idle_for <= threshold;

            // 长时间离开提醒：累计离开达 30 分钟时弹一次系统通知（可在设置里关闭、受免打扰约束）
            if idle_for >= Duration::from_secs(30 * 60) && !focus_active {
                if !away_notified {
                    away_notified = true;
                    notify_if_allowed(
                        &app,
                        &settings,
                        settings.notify_away,
                        "已离开约 30 分钟，记得起来活动一下",
                    );
                }
            } else {
                away_notified = false;
            }

            // 专注模式下段落起点不应回溯到“最后输入时刻”，否则会把开会前的空档也算进来
            let backdate = if focus_active { None } else { Some(idle_for) };

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
                        let pushed_segments: Vec<ActivitySegment> = {
                            let state = app.state::<AppState>();
                            if let Ok(store) = state.store.lock() {
                                accepted
                                    .iter()
                                    .flat_map(|date| store.list_segments_by_date(date).unwrap_or_default())
                                    .collect()
                            } else {
                                Vec::new()
                            }
                        };
                        if !pushed_segments.is_empty() {
                            // 时段明细失败不影响日汇总的已同步状态，下个周期再试
                            let _ = client.push_segments(&settings, pushed_segments).await;
                        }
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
                    logging::write("WARN", &format!("自动同步失败: {err}"));
                }
            }
        }
    });
}

/// 后台自动备份：每 6 小时检查一次，按天产出热备并保留最近 7 份。
/// 失败只写日志，不打扰用户，也不影响采集与同步。
fn spawn_backup_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut last_backup_day: Option<chrono::NaiveDate> = None;
        loop {
            tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
            let today = Local::now().date_naive();
            if last_backup_day == Some(today) {
                continue;
            }
            let state = app.state::<AppState>();
            match state.store.lock() {
                Ok(store) => match store.auto_backup(7) {
                    Ok(path) => {
                        last_backup_day = Some(today);
                        eprintln!("[backup] 自动备份已写入 {}", path.display());
                        logging::write("INFO", &format!("自动备份已写入 {}", path.display()));
                    }
                    Err(err) => {
                        eprintln!("[backup] 自动备份失败: {err}");
                        logging::write("ERROR", &format!("自动备份失败: {err}"));
                    }
                },
                Err(err) => {
                    eprintln!("[backup] store lock failed: {err}");
                    logging::write("ERROR", &format!("备份时 store lock failed: {err}"));
                }
            }
        }
    });
}

/// 周报推送：每周一上午 9 点后把最近 7 天的工时小结发到配置的机器人 webhook。
/// 兼容企业微信/飞书/钉钉的文本机器人（同时携带三种字段，多余字段会被忽略）。
fn spawn_report_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(client) => client,
            Err(_) => return,
        };
        let mut last_report_day: Option<chrono::NaiveDate> = None;
        loop {
            tokio::time::sleep(Duration::from_secs(60 * 60)).await;
            let now = Local::now();
            let today = now.date_naive();
            if last_report_day == Some(today) || now.weekday().num_days_from_monday() != 0 || now.hour() < 9 {
                continue;
            }
            let (webhook, rows, target) = {
                let state = app.state::<AppState>();
                let store = match state.store.lock() {
                    Ok(store) => store,
                    Err(_) => continue,
                };
                let settings = store.get_settings().unwrap_or_default();
                if settings.report_webhook.trim().is_empty() {
                    // 没配置也标记当天，避免每小时重复查库
                    last_report_day = Some(today);
                    continue;
                }
                let rows = store.list_all_daily_records().unwrap_or_default();
                (
                    settings.report_webhook.trim().to_string(),
                    rows,
                    settings.daily_target_minutes,
                )
            };

            let start = today - chrono::Duration::days(7);
            let week: Vec<&DailyRecord> = rows
                .iter()
                .filter(|row| {
                    NaiveDate::parse_from_str(&row.date, "%Y-%m-%d")
                        .map(|day| day >= start && day < today)
                        .unwrap_or(false)
                })
                .collect();
            if week.is_empty() {
                last_report_day = Some(today);
                continue;
            }
            let total: i64 = week.iter().map(|row| row.active_minutes).sum();
            let avg = total / week.len() as i64;
            let hit = week
                .iter()
                .filter(|row| target > 0 && row.active_minutes >= i64::from(target))
                .count();
            let mut text = format!(
                "工时周报（{} 至 {}）\n总活跃 {} 分钟（{} 小时），日均 {} 分钟，达标 {} 天\n",
                start.format("%m-%d"),
                today.format("%m-%d"),
                total,
                total / 60,
                avg,
                hit
            );
            for row in &week {
                text.push_str(&format!("{}: {} 分钟\n", row.date, row.active_minutes));
            }
            let payload = serde_json::json!({
                "msgtype": "text",
                "msg_type": "text",
                "text": {"content": text},
                "content": {"text": text},
            });
            match client.post(&webhook).json(&payload).send().await {
                Ok(response) if response.status().is_success() => {
                    last_report_day = Some(today);
                    logging::write("INFO", "周报已推送到 webhook");
                }
                Ok(response) => {
                    logging::write("WARN", &format!("周报推送返回 {}", response.status()));
                    last_report_day = Some(today);
                }
                Err(err) => {
                    // 网络失败不标记，下个整点重试
                    logging::write("WARN", &format!("周报推送失败: {err}"));
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
                paused: AtomicBool::new(settings0.paused),
                focus_until: AtomicI64::new(0),
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
            spawn_backup_loop(app.handle().clone());
            spawn_report_loop(app.handle().clone());
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
            check_update,
            install_update,
            set_focus_mode,
            focus_deadline,
            list_backups,
            restore_named_backup,
            notify_test,
            test_webhook
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
