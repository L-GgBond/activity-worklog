mod activity;
mod db;
mod settings;
mod sync;

use std::sync::Mutex;
use std::time::Duration;

use activity::InputActivityProbe;
use chrono::{Local, NaiveDate};
use db::{DailyRecord, Store};
use settings::AppSettings;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_autostart::ManagerExt;

struct AppState {
    store: Mutex<Store>,
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

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示记录", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("Activity Worklog 正在后台记录活动状态")
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
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
        let mut last_recorded_activity: Option<chrono::DateTime<Local>> = None;
        let mut current_date: NaiveDate = Local::now().date_naive();

        loop {
            let now = Local::now();
            if now.date_naive() != current_date {
                current_date = now.date_naive();
                last_recorded_activity = None;
            }

            {
                let state = app.state::<AppState>();
                let settings = state
                    .store
                    .lock()
                    .ok()
                    .and_then(|store| store.get_settings().ok())
                    .unwrap_or_default();

                if let Ok(idle_for) = probe.idle_duration() {
                    if idle_for <= Duration::from_secs(60) {
                        if let Ok(store) = state.store.lock() {
                            let _ = store.record_activity(now, settings.idle_threshold_minutes);
                        }
                        last_recorded_activity = Some(now);
                        let _ = app.emit("activity-recorded", now.to_rfc3339());
                    } else if let Some(last) = last_recorded_activity {
                        if now.signed_duration_since(last).num_minutes()
                            >= i64::from(settings.idle_threshold_minutes)
                        {
                            last_recorded_activity = None;
                        }
                    }
                }
            }

            tokio::time::sleep(Duration::from_secs(15)).await;
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
            });
            spawn_activity_loop(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_daily_records,
            get_settings,
            save_settings,
            export_records_csv
        ])
        .run(tauri::generate_context!())
        .expect("failed to run app");
}
