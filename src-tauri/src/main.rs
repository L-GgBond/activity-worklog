mod activity;
mod db;
mod settings;
mod sync;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use activity::InputActivityProbe;
use chrono::{Local, TimeZone};
use db::{ActivitySegment, DailyRecord, HourBucket, Store, SyncSummary};
use settings::AppSettings;
use sync::HttpSyncClient;
use tauri::menu::{Menu, MenuItem, MenuItemKind};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_shell::ShellExt;
use tauri_plugin_updater::UpdaterExt;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

struct AppState {
    store: Mutex<Store>,
    paused: AtomicBool,
    /// 记录被清空/删除后置位，采集循环据此丢弃内存缓冲，避免下一 tick 又写回
    resetting: AtomicBool,
    /// 应用锁：未解锁时所有读数据的命令都会被拒绝
    unlocked: AtomicBool,
    failed_unlocks: AtomicU32,
    cooldown_until: Mutex<Option<Instant>>,
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
fn list_daily_records(state: State<AppState>) -> Result<Vec<DailyRecord>, String> {
    ensure_unlocked(&state)?;
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .list_daily_records()
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

#[tauri::command]
fn get_settings(state: State<AppState>) -> Result<AppSettings, String> {
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .get_settings()
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn save_settings(
    settings: AppSettings,
    app: AppHandle,
    state: State<AppState>,
) -> Result<(), String> {
    ensure_unlocked(&state)?;
    let autostart = app.autolaunch();
    if settings.autostart_enabled {
        autostart.enable().map_err(|err| err.to_string())?;
    } else {
        autostart.disable().map_err(|err| err.to_string())?;
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
        store
            .save_settings(&next)
            .map_err(|err| err.to_string())?;
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
        state.resetting.store(true, Ordering::Relaxed);
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
    state.resetting.store(true, Ordering::Relaxed);
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
        let mut store = state
            .store
            .lock()
            .map_err(|_| "store lock failed".to_string())?;
        store
            .restore_from(std::path::Path::new(&src))
            .map_err(|err| err.to_string())?;
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

/// 通过 macOS 系统通知提示（无需额外通知插件，复用 shell 调用 osascript）。
fn notify_macos(app: &AppHandle, message: &str) {
    let safe = message.replace('"', "'");
    let script = format!("display notification \"{}\" with title \"Activity Worklog\"", safe);
    let _ = app.shell().command("osascript").args(["-e", &script]).spawn();
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

    let dates: Vec<String> = pending.iter().map(|record| record.date.clone()).collect();
    let client = HttpSyncClient::new();
    let accepted = client
        .push_records(&settings, pending)
        .await
        .map_err(|err| err.to_string())?;
    let synced = if accepted.is_empty() { dates } else { accepted };

    {
        let store = state
            .store
            .lock()
            .map_err(|_| "store lock failed".to_string())?;
        store
            .mark_synced(&synced)
            .map_err(|err| err.to_string())?;
    }
    let _ = app.emit("records-updated", ());
    Ok(format!("已同步 {} 天记录", synced.len()))
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
    if let Some(until) = *state
        .cooldown_until
        .lock()
        .map_err(|_| "state lock failed".to_string())?
    {
        if Instant::now() < until {
            let left = (until - Instant::now()).as_secs() + 1;
            return Err(format!("尝试次数过多，请 {left} 秒后再试"));
        }
    }

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
        state.failed_unlocks.store(0, Ordering::Relaxed);
        *state
            .cooldown_until
            .lock()
            .map_err(|_| "state lock failed".to_string())? = None;
        state.unlocked.store(true, Ordering::Relaxed);
        let _ = app.emit("unlocked", ());
        return Ok(());
    }

    let fails = state.failed_unlocks.fetch_add(1, Ordering::Relaxed) + 1;
    if fails >= 5 {
        state.failed_unlocks.store(0, Ordering::Relaxed);
        *state
            .cooldown_until
            .lock()
            .map_err(|_| "state lock failed".to_string())? =
            Some(Instant::now() + Duration::from_secs(60));
        return Err("密码连续错误 5 次，请 60 秒后再试".into());
    }
    Err(format!("密码不正确，还可尝试 {} 次", 5 - fails))
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
        return Err("当前密码不正确".into());
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
    state.unlocked.store(true, Ordering::Relaxed);
    Ok(recovery)
}

#[tauri::command]
fn disable_lock(state: State<AppState>, current: String) -> Result<(), String> {
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
        return Err("密码不正确".into());
    }
    settings.password_hash.clear();
    settings.recovery_hash.clear();
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .save_settings(&settings)
        .map_err(|err| err.to_string())?;
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
        return Err("恢复码不正确".into());
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

/// 将前端生成的周报字节写入「下载 / Activity Worklog」目录，返回完整路径。
/// ext 为扩展名（不含点），如 "png" / "pdf"。
#[tauri::command]
fn export_report(filename: String, data: Vec<u8>, ext: String) -> Result<String, String> {
    let base = dirs::download_dir().ok_or_else(|| "无法定位下载目录".to_string())?;
    let dir = base.join("Activity Worklog");
    std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    let path = dir.join(format!("{}.{}", filename, ext.trim_start_matches('.')));
    std::fs::write(&path, &data).map_err(|err| err.to_string())?;
    Ok(path.to_string_lossy().to_string())
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示记录", true, None::<&str>)?;
    let summary = MenuItem::with_id(app, "summary", "今日小结", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", "暂停记录", true, None::<&str>)?;
    let lock = MenuItem::with_id(app, "lock", "锁定并隐藏", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &summary, &pause, &lock, &quit])?;

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
                }
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            "quit" => app.exit(0),
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

            // 用户清空/删除记录后，丢弃内存里的当天段落，避免下一 tick 又写回去
            if state.resetting.swap(false, Ordering::Relaxed) {
                segments.clear();
                continue;
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
                    notify_macos(&app, "已离开约 30 分钟，记得起来活动一下");
                }
            } else {
                away_notified = false;
            }

            // 新段落起点回溯到最后一次真实输入时刻（now - idle_for）
            let backdate = Some(idle_for);

            if let Ok(mut store) = state.store.lock() {
                let _ = store.record_activity(now, &mut segments, in_work_session, backdate);
            }
            let _ = app.emit("records-updated", ());

            // 实时更新托盘 tooltip 为今日活跃占比
            if let Ok(store) = state.store.lock() {
                if let Ok(records) = store.list_daily_records() {
                    let today = Local::now().format("%Y-%m-%d").to_string();
                    if let Some(t) = records.iter().find(|r| r.date == today) {
                        let total = t.active_minutes + t.idle_minutes;
                        let pct = if total > 0 {
                            (t.active_minutes * 100 / total) as u32
                        } else {
                            0
                        };
                        if let Some(tray) = app.tray_by_id("main") {
                            let _ = tray.set_tooltip(Some(format!(
                                "Activity Worklog · 今日活跃 {}%",
                                pct
                            )));
                        }
                    }
                }
            };
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

            let dates: Vec<String> = pending.iter().map(|record| record.date.clone()).collect();
            match client.push_records(&settings, pending).await {
                Ok(accepted) => {
                    // 服务端逐条确认时只标记确认过的日期，避免“假已同步”
                    let synced_dates = if accepted.is_empty() { dates } else { accepted };
                    let state = app.state::<AppState>();
                    if let Ok(store) = state.store.lock() {
                        let _ = store.mark_synced(&synced_dates);
                    }
                    let _ = app.emit("records-updated", ());
                }
                Err(err) => {
                    let _ = app.emit("sync-error", err.to_string());
                }
            }
        }
    });
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // 重复启动只把已有窗口叫到前台（若已锁定会先显示锁屏），不再起第二个采集循环
            show_main_window(app);
        }))
        .plugin(tauri_plugin_shell::init())
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
            // 启动时若设过密码，先处于锁定态，前端只渲染锁屏
            let locked = store
                .get_settings()
                .map(|settings| !settings.password_hash.is_empty())
                .unwrap_or(false);
            // 隐藏 Dock 图标，仅菜单栏常驻（仅 macOS 有此机制；需重启生效）
            #[cfg(target_os = "macos")]
            {
                if store.get_settings().map(|s| s.menubar_only).unwrap_or(false) {
                    let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                }
            }
            app.manage(AppState {
                store: Mutex::new(store),
                paused: AtomicBool::new(false),
                resetting: AtomicBool::new(false),
                unlocked: AtomicBool::new(!locked),
                failed_unlocks: AtomicU32::new(0),
                cooldown_until: Mutex::new(None),
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
            list_segments,
            delete_day,
            clear_all_records,
            sync_summary,
            get_settings,
            save_settings,
            export_records_csv,
            test_sync_connection,
            sync_now,
            is_paused,
            toggle_pause,
            export_report,
            hourly_activity,
            export_db,
            restore_db,
            check_update
        ])
        .run(tauri::generate_context!())
        .expect("failed to run app");
}
