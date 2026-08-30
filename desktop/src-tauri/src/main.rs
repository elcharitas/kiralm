// prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, UnixConnector, Uri};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::ipc::Channel;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_shell::process::CommandChild;
use tauri_plugin_shell::ShellExt;

/// Same shape/defaults as the daemon's own `~/.kiralm/config.toml`
/// (src/config.rs) -- only the one field this shell needs.
#[derive(Deserialize, Default)]
struct RawConfig {
    unix_socket: Option<PathBuf>,
}

fn kiralm_home() -> PathBuf {
    if let Some(path) = std::env::var_os("KIRALM_HOME") {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .expect("no home directory found")
        .join(".kiralm")
}

/// Reads the daemon's configured address, same default it uses.
fn read_unix_socket() -> PathBuf {
    if let Some(path) = std::env::var_os("KIRALM_UNIX_SOCKET") {
        return PathBuf::from(path);
    }
    let path = kiralm_home().join("config.toml");
    let raw: RawConfig = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    raw.unix_socket
        .unwrap_or_else(|| kiralm_home().join("kiralm.sock"))
}

fn is_listening(path: &PathBuf) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Tracks whether *this* app spawned the daemon, so Quit only stops a
/// daemon it owns -- never one it merely found already running.
struct AppState {
    child: Option<CommandChild>,
    unix_socket: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaemonRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DaemonResponseHead {
    status: u16,
    status_text: String,
    headers: HashMap<String, String>,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "event", content = "data")]
enum DaemonResponseEvent {
    Chunk(Vec<u8>),
    End,
    Error(String),
}

#[tauri::command]
async fn daemon_request(
    state: tauri::State<'_, Mutex<AppState>>,
    request: DaemonRequest,
    on_event: Channel<DaemonResponseEvent>,
) -> Result<DaemonResponseHead, String> {
    if !request.path.starts_with("/api/") && !request.path.starts_with("/v1/") {
        return Err("desktop IPC only accepts KiraLM API paths".to_string());
    }
    let socket = state.lock().map_err(|e| e.to_string())?.unix_socket.clone();
    let method = Method::from_bytes(request.method.as_bytes()).map_err(|e| e.to_string())?;
    let uri: hyper::Uri = Uri::new(socket, &request.path).into();
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in request.headers {
        builder = builder.header(name, value);
    }
    let body = Full::new(Bytes::from(request.body.unwrap_or_default()));
    let response = Client::<UnixConnector, Full<Bytes>>::unix()
        .request(builder.body(body).map_err(|e| e.to_string())?)
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| Some((name.to_string(), value.to_str().ok()?.to_string())))
        .collect();
    tauri::async_runtime::spawn(async move {
        let mut body = response.into_body();
        while let Some(frame) = body.frame().await {
            match frame {
                Ok(frame) => {
                    if let Some(data) = frame.data_ref() {
                        if on_event
                            .send(DaemonResponseEvent::Chunk(data.to_vec()))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let _ = on_event.send(DaemonResponseEvent::Error(error.to_string()));
                    return;
                }
            }
        }
        let _ = on_event.send(DaemonResponseEvent::End);
    });
    Ok(DaemonResponseHead {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or_default().to_string(),
        headers,
    })
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(Mutex::new(AppState {
            child: None,
            unix_socket: read_unix_socket(),
        }))
        .invoke_handler(tauri::generate_handler![daemon_request])
        .setup(|app| {
            let socket = read_unix_socket();

            if !is_listening(&socket) {
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
                    if is_listening(&socket) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                // ponytail: if still not up, open the window anyway --
                // the webview shows its own connection error, honest enough.
            }

            let window =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                    .title("KiraLM")
                    .inner_size(1100.0, 760.0)
                    .min_inner_size(1040.0, 680.0);
            #[cfg(target_os = "macos")]
            let window = window
                .title_bar_style(tauri::TitleBarStyle::Overlay)
                .hidden_title(true);
            #[cfg(not(target_os = "macos"))]
            let window = window.decorations(false);
            window.build()?;

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

    // A template image (mark only, no background) so macOS tints it
    // to match the menu bar instead of showing the app's colored icon.
    TrayIconBuilder::new()
        .icon(tauri::include_image!("icons/tray-icon.png"))
        .icon_as_template(true)
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
