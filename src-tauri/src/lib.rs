pub mod commands;
pub mod db;
pub mod engine;
pub mod error;
pub mod webui;

use std::collections::HashSet;

use engine::{TorrentManager, TorrentState};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use tracing::info;

const SHOW_LABEL: &str = "Show Window";
const HIDE_LABEL: &str = "Hide Window";

/// Tray menu entry whose label mirrors the main window's visibility.
struct TrayToggleItem(MenuItem<tauri::Wry>);

fn set_main_window_visible(app: &AppHandle, visible: bool) {
    if let Some(w) = app.get_webview_window("main") {
        if visible {
            let _ = w.unminimize();
            let _ = w.show();
            let _ = w.set_focus();
        } else {
            let _ = w.hide();
        }
    }
    // The single-instance callback can fire before setup has registered the item.
    if let Some(item) = app.try_state::<TrayToggleItem>() {
        let _ = item
            .0
            .set_text(if visible { HIDE_LABEL } else { SHOW_LABEL });
    }
}

fn toggle_main_window(app: &AppHandle) {
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    // A minimized window still reports visible; treat it as hidden so a click restores it.
    let shown = w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false);
    set_main_window_visible(app, !shown);
}

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rtorrent_lib=debug,librqbit=info".into()),
        )
        .init();

    // Load persisted settings so TorrentManager starts with the correct download dir
    let settings = commands::settings::load_settings();
    let download_dir = std::path::PathBuf::from(&settings.download_path);
    let start_minimized = settings.start_minimized;

    // Collect any .torrent path passed on the command line at startup
    let startup_torrent: Option<String> = std::env::args()
        .skip(1)
        .find(|a| a.to_lowercase().ends_with(".torrent"));

    tauri::Builder::default()
        // Single-instance must be the first plugin registered — plugins run in
        // registration order, and on Windows this one only works correctly as the
        // first to see argv. When the app is already running and the user opens a
        // .torrent file, the OS launches a second process; the plugin kills that
        // second process and fires this callback in the *first* instance instead.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            set_main_window_visible(app, true);
            if let Some(w) = app.get_webview_window("main") {
                // Emit directly to the WebView window so the JS listener receives it.
                // app.emit() targets the Rust event bus; w.emit() targets the webview.
                if let Some(path) = argv.iter().find(|a| a.to_lowercase().ends_with(".torrent")) {
                    let _ = w.emit("open-torrent-file", path);
                }
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(move |app| {
            info!("Starting rtorrent v{}", app.package_info().version);

            // The engine must start here, not before the builder: the single-instance
            // plugin has already terminated a duplicate process by now, so a second
            // launch (e.g. opening a .torrent in Explorer) never touches the shared
            // DHT port or session store. block_on keeps the librqbit session's tasks on
            // the Tauri async runtime for the lifetime of the app.
            let manager = tauri::async_runtime::block_on(TorrentManager::new(download_dir))?;
            app.manage(manager);

            // ── System tray ──────────────────────────────────────────────
            let toggle_item = MenuItem::with_id(app, "toggle", HIDE_LABEL, true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&toggle_item, &quit_item])?;
            app.manage(TrayToggleItem(toggle_item));

            let icon = app
                .default_window_icon()
                .ok_or("default window icon not found")?
                .clone();
            TrayIconBuilder::new()
                .icon(icon)
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("rTorrent")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "toggle" => toggle_main_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // Fire on button release only; Down would toggle twice per click.
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_main_window(tray.app_handle());
                    }
                })
                .build(app)?;

            // ── Hide to tray on close ─────────────────────────────────────
            let main_window = app
                .get_webview_window("main")
                .ok_or("main window not found")?;
            let win_clone = main_window.clone();
            main_window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    set_main_window_visible(win_clone.app_handle(), false);
                }
            });

            // When a .torrent file is being opened at launch, keep the window
            // visible so the user sees the "already in list" notice if needed.
            if start_minimized && startup_torrent.is_none() {
                set_main_window_visible(app.handle(), false);
            }

            // ── Open .torrent passed at launch (file-association / argv) ──
            if let Some(path) = startup_torrent {
                let app_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    // Give the frontend time to mount and start listening
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let _ = app_handle.emit("open-torrent-file", path);
                });
            }

            // ── Background stats + download-complete notifications ─────────
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                // Track which torrent IDs are already known to be seeding so we
                // only fire the notification on the first transition.
                let mut seeding_ids: HashSet<String> = HashSet::new();

                loop {
                    interval.tick().await;
                    let manager = app_handle.state::<TorrentManager>();
                    let all = manager.get_all();

                    if !all.is_empty() {
                        let _ = app_handle.emit("torrent-stats", &all);
                    }

                    // Detect new completions
                    for info in &all {
                        if matches!(info.state, TorrentState::Seeding { .. }) {
                            if seeding_ids.insert(info.id.clone()) {
                                // First time we see this torrent seeding
                                let _ = app_handle
                                    .notification()
                                    .builder()
                                    .title("Download Complete")
                                    .body(&info.name)
                                    .show();
                            }
                        } else {
                            // If torrent is removed/re-added it can transition again
                            seeding_ids.remove(&info.id);
                        }
                    }
                    // Forget IDs of removed torrents
                    let current_ids: HashSet<&str> = all.iter().map(|i| i.id.as_str()).collect();
                    seeding_ids.retain(|id| current_ids.contains(id.as_str()));
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::torrent::get_torrents,
            commands::torrent::list_torrent_magnet,
            commands::torrent::list_torrent_file,
            commands::torrent::confirm_add_torrent,
            commands::torrent::cancel_torrent_listing,
            commands::torrent::pause_torrent,
            commands::torrent::resume_torrent,
            commands::torrent::remove_torrent,
            commands::torrent::remove_torrent_with_data,
            commands::torrent::get_torrent_details,
            commands::settings::get_settings,
            commands::settings::save_settings,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
