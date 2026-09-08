use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use base64::Engine;
use parking_lot::{Mutex, RwLock};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::events::{EventBus, EventTopic};

const OUTPUT_HISTORY: usize = 2_048;
const OUTPUT_CHANNEL: usize = 512;
const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_TERMINALS_PER_PROJECT: usize = 8;

#[derive(Debug, Clone, Serialize)]
pub struct TerminalInfo {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub cwd: String,
    pub shell: String,
    pub cols: u16,
    pub rows: u16,
    pub running: bool,
    pub exit_code: Option<u32>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub detected_urls: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TerminalOutput {
    pub sequence: u64,
    pub data: String,
}

struct TerminalSession {
    info: RwLock<TerminalInfo>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    output: RwLock<VecDeque<TerminalOutput>>,
    next_sequence: AtomicU64,
    sender: broadcast::Sender<TerminalOutput>,
    events: Arc<EventBus>,
}

impl TerminalSession {
    fn push_output(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let chunk = TerminalOutput {
            sequence: self.next_sequence.fetch_add(1, Ordering::Relaxed),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        };
        {
            let mut history = self.output.write();
            history.push_back(chunk.clone());
            while history.len() > OUTPUT_HISTORY {
                history.pop_front();
            }
        }
        self.detect_urls(&String::from_utf8_lossy(bytes));
        let _ = self.sender.send(chunk);
    }

    fn detect_urls(&self, text: &str) {
        let mut found = Vec::new();
        for word in text.split_whitespace() {
            let candidate = word.trim_matches(|character: char| {
                matches!(
                    character,
                    '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '\'' | '"'
                )
            });
            if let Some(url) = normalize_local_url(candidate) {
                found.push(url);
            }
        }
        if found.is_empty() {
            return;
        }
        let mut info = self.info.write();
        for url in found {
            if !info.detected_urls.contains(&url) {
                info.detected_urls.push(url.clone());
                self.events.publish(
                    EventTopic::Preview,
                    "url_detected",
                    Some(info.project_id.clone()),
                    serde_json::json!({
                        "terminal_id": info.id,
                        "url": url,
                    }),
                );
            }
        }
    }

    fn snapshot(&self) -> TerminalInfo {
        let mut info = self.info.write();
        if info.running {
            if let Ok(Some(status)) = self.child.lock().try_wait() {
                info.running = false;
                info.exit_code = Some(status.exit_code());
            }
        }
        info.clone()
    }

    fn close(&self) {
        let mut info = self.info.write();
        if info.running {
            let _ = self.child.lock().kill();
            info.running = false;
        }
        self.events.publish(
            EventTopic::Terminal,
            "closed",
            Some(info.id.clone()),
            serde_json::json!({ "project_id": info.project_id }),
        );
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.child.get_mut().kill();
    }
}

pub struct TerminalManager {
    sessions: RwLock<HashMap<String, Arc<TerminalSession>>>,
    events: Arc<EventBus>,
}

impl TerminalManager {
    pub fn new(events: Arc<EventBus>) -> TerminalManager {
        TerminalManager {
            sessions: RwLock::new(HashMap::new()),
            events,
        }
    }

    pub fn create(
        &self,
        project_id: &str,
        project_root: &Path,
        title: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> Result<TerminalInfo, String> {
        let count = self
            .sessions
            .read()
            .values()
            .filter(|session| session.info.read().project_id == project_id)
            .count();
        if count >= MAX_TERMINALS_PER_PROJECT {
            return Err(format!(
                "a workspace can have at most {MAX_TERMINALS_PER_PROJECT} terminals"
            ));
        }
        let cols = cols.clamp(20, 500);
        let rows = rows.clamp(4, 200);
        let root = project_root
            .canonicalize()
            .map_err(|error| format!("failed to open workspace: {error}"))?;
        if !root.is_dir() {
            return Err("workspace path is not a directory".to_string());
        }
        let shell = default_shell();
        let pty = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| format!("failed to open terminal: {error}"))?;
        let mut command = CommandBuilder::new(&shell);
        command.cwd(&root);
        command.env("TERM", "xterm-256color");
        let child = pty
            .slave
            .spawn_command(command)
            .map_err(|error| format!("failed to start shell: {error}"))?;
        let reader = pty
            .master
            .try_clone_reader()
            .map_err(|error| format!("failed to read terminal: {error}"))?;
        let writer = pty
            .master
            .take_writer()
            .map_err(|error| format!("failed to write terminal: {error}"))?;
        let id = uuid::Uuid::new_v4().to_string();
        let (sender, _) = broadcast::channel(OUTPUT_CHANNEL);
        let session = Arc::new(TerminalSession {
            info: RwLock::new(TerminalInfo {
                id: id.clone(),
                project_id: project_id.to_string(),
                title: terminal_title(title, &shell),
                cwd: root.display().to_string(),
                shell: shell.clone(),
                cols,
                rows,
                running: true,
                exit_code: None,
                created_at: chrono::Utc::now(),
                detected_urls: Vec::new(),
            }),
            master: Mutex::new(pty.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            output: RwLock::new(VecDeque::new()),
            next_sequence: AtomicU64::new(1),
            sender,
            events: self.events.clone(),
        });
        self.sessions.write().insert(id.clone(), session.clone());
        self.events.publish(
            EventTopic::Terminal,
            "started",
            Some(id),
            serde_json::json!({
                "project_id": project_id,
                "cwd": root,
                "shell": shell,
            }),
        );
        spawn_reader(session.clone(), reader);
        Ok(session.snapshot())
    }

    pub fn list(&self, project_id: Option<&str>) -> Vec<TerminalInfo> {
        let mut terminals: Vec<_> = self
            .sessions
            .read()
            .values()
            .filter(|session| project_id.is_none_or(|id| session.info.read().project_id == id))
            .map(|session| session.snapshot())
            .collect();
        terminals.sort_by_key(|terminal| terminal.created_at);
        terminals
    }

    pub fn get(&self, id: &str) -> Option<TerminalInfo> {
        self.sessions
            .read()
            .get(id)
            .map(|session| session.snapshot())
    }

    pub fn input(&self, id: &str, data: &[u8]) -> Result<(), String> {
        if data.len() > MAX_INPUT_BYTES {
            return Err(format!("terminal input exceeds {MAX_INPUT_BYTES} bytes"));
        }
        let session = self.session(id)?;
        if !session.info.read().running {
            return Err("terminal is not running".to_string());
        }
        let mut writer = session.writer.lock();
        writer
            .write_all(data)
            .and_then(|_| writer.flush())
            .map_err(|error| format!("failed to write terminal: {error}"))
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<TerminalInfo, String> {
        let session = self.session(id)?;
        let cols = cols.clamp(20, 500);
        let rows = rows.clamp(4, 200);
        session
            .master
            .lock()
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| format!("failed to resize terminal: {error}"))?;
        {
            let mut info = session.info.write();
            info.cols = cols;
            info.rows = rows;
        }
        Ok(session.snapshot())
    }

    pub fn close(&self, id: &str) -> Result<(), String> {
        let session = self
            .sessions
            .write()
            .remove(id)
            .ok_or_else(|| format!("terminal '{id}' not found"))?;
        session.close();
        Ok(())
    }

    pub fn close_project(&self, project_id: &str) {
        let ids: Vec<_> = self
            .sessions
            .read()
            .iter()
            .filter(|(_, session)| session.info.read().project_id == project_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let _ = self.close(&id);
        }
    }

    pub fn replay(&self, id: &str, after: u64) -> Result<Vec<TerminalOutput>, String> {
        let session = self.session(id)?;
        let chunks = session
            .output
            .read()
            .iter()
            .filter(|chunk| chunk.sequence > after)
            .cloned()
            .collect();
        Ok(chunks)
    }

    pub fn subscribe(&self, id: &str) -> Result<broadcast::Receiver<TerminalOutput>, String> {
        Ok(self.session(id)?.sender.subscribe())
    }

    fn session(&self, id: &str) -> Result<Arc<TerminalSession>, String> {
        self.sessions
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| format!("terminal '{id}' not found"))
    }
}

fn spawn_reader(session: Arc<TerminalSession>, mut reader: Box<dyn Read + Send>) {
    std::thread::Builder::new()
        .name(format!("terminal-{}", session.info.read().id))
        .spawn(move || {
            let mut buffer = [0_u8; 8 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => session.push_output(&buffer[..read]),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let mut info = session.info.write();
            if let Ok(Some(status)) = session.child.lock().try_wait() {
                info.exit_code = Some(status.exit_code());
            }
            info.running = false;
            session.events.publish(
                EventTopic::Terminal,
                "exited",
                Some(info.id.clone()),
                serde_json::json!({
                    "project_id": info.project_id,
                    "exit_code": info.exit_code,
                }),
            );
        })
        .expect("terminal reader thread must start");
}

fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

fn terminal_title(title: Option<&str>, shell: &str) -> String {
    let requested = title.unwrap_or_default().trim();
    if !requested.is_empty() {
        return requested.chars().take(80).collect();
    }
    PathBuf::from(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Terminal")
        .to_string()
}

fn normalize_local_url(value: &str) -> Option<String> {
    let trimmed = value.trim_end_matches(|character: char| {
        matches!(character, '.' | ':' | '!' | '?' | '/' | '\\')
    });
    let rest = trimmed
        .strip_prefix("http://localhost:")
        .or_else(|| trimmed.strip_prefix("http://127.0.0.1:"))
        .or_else(|| trimmed.strip_prefix("https://localhost:"))?;
    let port = rest
        .split(|character: char| !character.is_ascii_digit())
        .next()?;
    port.parse::<u16>().ok().filter(|port| *port > 0)?;
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_local_preview_urls() {
        assert_eq!(
            normalize_local_url("http://localhost:5173/"),
            Some("http://localhost:5173".to_string())
        );
        assert_eq!(
            normalize_local_url("http://127.0.0.1:3000/app"),
            Some("http://127.0.0.1:3000/app".to_string())
        );
        assert_eq!(normalize_local_url("https://example.com"), None);
    }

    #[tokio::test]
    async fn pty_accepts_input_and_replays_output() {
        let root = std::env::temp_dir().join(format!("kiralm-terminal-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let manager = TerminalManager::new(Arc::new(EventBus::new(32)));
        let terminal = manager
            .create("project", &root, Some("test"), 80, 24)
            .unwrap();
        manager
            .input(&terminal.id, b"printf 'KIRALM_PTY_OK\\n'\n")
            .unwrap();
        let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let output = manager.replay(&terminal.id, 0).unwrap();
                let bytes: Vec<u8> = output
                    .iter()
                    .flat_map(|chunk| {
                        base64::engine::general_purpose::STANDARD
                            .decode(&chunk.data)
                            .unwrap_or_default()
                    })
                    .collect();
                if String::from_utf8_lossy(&bytes).contains("KIRALM_PTY_OK") {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or(false);
        assert!(found);
        manager.close(&terminal.id).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
