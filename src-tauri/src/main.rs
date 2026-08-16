mod activity;
mod db;
mod settings;
mod sync;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use activity::InputActivityProbe;
use chrono::{Local, TimeZone};
use db::{ActivitySegment, DailyRecord, HourBucket, Store};
use settings::AppSettings;
use sync::HttpSyncClient;
use tauri::menu::{Menu, MenuItem, MenuItemKind};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_shell::ShellExt;
use tauri_plugin_updater::UpdaterExt;

struct AppState {
    store: Mutex<Store>,
    paused: AtomicBool,
}

#[tauri::command]
fn list_daily_records(state: State<AppState>) -> Result<Vec<DailyRecord>, String> {
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .list_daily_records()
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn list_segments(state: State<AppState>, date: String) -> Result<Vec<ActivitySegment>, String> {
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
    let autostart = app.autolaunch();
    if settings.autostart_enabled {
        autostart.enable().map_err(|err| err.to_string())?;
    } else {
        autostart.disable().map_err(|err| err.to_string())?;
    }

    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .save_settings(&settings)
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn export_records_csv(state: State<AppState>) -> Result<String, String> {
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
    state
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .hourly_activity(range_days)
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn export_db(app: AppHandle) -> Result<String, String> {
    let path = app
        .state::<AppState>()
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .db_path();
    let base = dirs::download_dir().ok_or_else(|| "无法定位下载目录".to_string())?;
    let dir = base.join("Activity Worklog");
    std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    let dest = dir.join(format!(
        "worklog-backup-{}.sqlite3",
        Local::now().format("%Y%m%d")
    ));
    std::fs::copy(&path, &dest).map_err(|err| err.to_string())?;
    Ok(dest.to_string_lossy().to_string())
}

#[tauri::command]
async fn restore_db(app: AppHandle) -> Result<String, String> {
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
    let path = app
        .state::<AppState>()
        .store
        .lock()
        .map_err(|_| "store lock failed".to_string())?
        .db_path();
    std::fs::copy(&src, &path).map_err(|err| err.to_string())?;
    Ok("已恢复备份，请重启应用生效".into())
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
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &summary, &pause, &quit])?;

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

            let Ok(idle_for) = probe.idle_duration() else {
                continue;
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
                Ok(()) => {
                    let state = app.state::<AppState>();
                    if let Ok(store) = state.store.lock() {
                        let _ = store.mark_synced(&dates);
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
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            setup_tray(app.handle())?;
            let store = Store::open()?;
            // 菜单栏驻留模式：隐藏 Dock 图标，仅菜单栏常驻（需重启生效）
            if store.get_settings().map(|s| s.menubar_only).unwrap_or(false) {
                let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            }
            app.manage(AppState {
                store: Mutex::new(store),
                paused: AtomicBool::new(false),
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
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_daily_records,
            list_segments,
            get_settings,
            save_settings,
            export_records_csv,
            test_sync_connection,
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
