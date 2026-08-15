mod activity;
mod db;
mod settings;
mod sync;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use activity::InputActivityProbe;
use chrono::{Local, TimeZone};
use db::{ActivitySegment, DailyRecord, Store};
use settings::AppSettings;
use sync::HttpSyncClient;
use tauri::menu::{Menu, MenuItem, MenuItemKind};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::ManagerExt;

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

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示记录", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", "暂停记录", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &pause, &quit])?;

    TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("Activity Worklog 正在后台记录活动状态")
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
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

            // 新段落起点回溯到最后一次真实输入时刻（now - idle_for）
            let backdate = Some(idle_for);

            if let Ok(mut store) = state.store.lock() {
                let _ = store.record_activity(now, &mut segments, in_work_session, backdate);
            }
            let _ = app.emit("records-updated", ());
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
        .setup(|app| {
            setup_tray(app.handle())?;
            let store = Store::open()?;
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
            toggle_pause
        ])
        .run(tauri::generate_context!())
        .expect("failed to run app");
}
