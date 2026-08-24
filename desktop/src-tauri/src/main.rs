// prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Deserialize;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_shell::process::CommandChild;
use tauri_plugin_shell::ShellExt;

/// Same shape/defaults as the daemon's own `~/.kiralm/config.toml`
/// (src/config.rs) -- only the one field this shell needs.
#[derive(Deserialize, Default)]
struct RawConfig {
    http_listen: Option<String>,
}

fn kiralm_home() -> PathBuf {
    dirs::home_dir().expect("no home directory found").join(".kiralm")
}

/// Reads the daemon's configured address, same default it uses.
fn read_http_listen() -> String {
    let path = kiralm_home().join("config.toml");
    let raw: RawConfig = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    raw.http_listen.unwrap_or_else(|| "127.0.0.1:11535".to_string())
}

fn is_listening(addr: &SocketAddr) -> bool {
    TcpStream::connect_timeout(addr, Duration::from_millis(300)).is_ok()
}

/// Tracks whether *this* app spawned the daemon, so Quit only stops a
/// daemon it owns -- never one it merely found already running.
struct AppState {
    child: Option<CommandChild>,
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(Mutex::new(AppState { child: None }))
        .setup(|app| {
            let addr_str = read_http_listen();
            let addr: SocketAddr = addr_str
                .parse()
                .unwrap_or_else(|_| "127.0.0.1:11535".parse().unwrap());

            if !is_listening(&addr) {
                let (_rx, child) = app
                    .shell()
                    .sidecar("kiralm")
                    .expect("failed to resolve kiralm sidecar")
                    .args(["serve"])
                    .spawn()
                    .expect("failed to spawn kiralm daemon");
                app.state::<Mutex<AppState>>().lock().unwrap().child = Some(child);

                // Poll for readiness, same 50x100ms budget as the CLI's
                // own ensure_daemon_running (src/cli.rs).
                for _ in 0..50 {
                    if is_listening(&addr) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                // ponytail: if still not up, open the window anyway --
                // the webview shows its own connection error, honest enough.
            }

            let url = format!("http://{addr}");
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.parse().unwrap()))
                .title("KiraLM")
                .inner_size(1100.0, 760.0)
                .build()?;

            build_tray(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing hides, matching Ollama.app -- the daemon and app
            // stay alive in the menu bar until Quit.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                window.hide().ok();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running kiralm desktop shell");
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show KiraLM", true, None::<&str>)?;
    let hide = MenuItem::with_id(app, "hide", "Hide KiraLM", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &hide, &quit])?;

    TrayIconBuilder::new()
        .icon(app.default_window_icon().unwrap().clone())
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(w) = app.get_webview_window("main") {
                    w.show().ok();
                    w.set_focus().ok();
                }
            }
            "hide" => {
                if let Some(w) = app.get_webview_window("main") {
                    w.hide().ok();
                }
            }
            "quit" => quit_app(app),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// Graceful shutdown of a daemon this app spawned: SIGTERM, wait up to
/// 5s, SIGKILL if still alive -- same shape as engine.rs::shutdown.
fn quit_app(app: &AppHandle) {
    let state = app.state::<Mutex<AppState>>();
    let mut state = state.lock().unwrap();
    if let Some(child) = state.child.take() {
        let pid = child.pid();
        #[cfg(unix)]
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        for _ in 0..50 {
            #[cfg(unix)]
            let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
            #[cfg(not(unix))]
            let alive = false;
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
    }
    app.exit(0);
}
