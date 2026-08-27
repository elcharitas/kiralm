//! The harness's tool set (`read_file`, `write_file`, `list_directory`,
//! `run_command`): every call is sandboxed to one session's project root
//! and gated by that session's permission mode.
//! This is what lets a model actually touch the filesystem, so both the
//! sandbox and the permission check are load-bearing, not decoration.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::json;

/// A session's tool-access level. Defaults to `ReadOnly` (new sessions
/// start locked down); the UI's "Full access" chip is what flips this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    ReadOnly,
    FullAccess,
}

impl Permission {
    pub fn parse(s: &str) -> Permission {
        if s.eq_ignore_ascii_case("full-access") {
            Permission::FullAccess
        } else {
            Permission::ReadOnly
        }
    }
}

/// One executed tool call, real enough to drive the UI's "Worked for Xm
/// Ys" collapsible: what was called, with what, and a one-line result.
#[derive(Debug, Clone, Serialize)]
pub struct ToolStep {
    pub name: String,
    pub args_summary: String,
    pub result_summary: String,
    pub ok: bool,
    /// Real before/after content for a successful `write_file` call, so
    /// the UI's Diff tab can render a genuine line-level diff instead of
    /// parsing fenced code blocks out of the chat reply. `None` for
    /// every other tool, or a write that was rejected/failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_edit: Option<FileEdit>,
}

/// A real file mutation from one `write_file` call: `before` is the
/// file's content immediately prior (`None` when the write created a
/// new file), `after` is exactly what was written.
///
/// Full content, no truncation -- unlike every other tool's
/// output there's no `truncate()` here, so a turn that writes a huge
/// file ships its full before/after over the wire. Cap it if a real
/// multi-MB write ever makes a turn's payload a problem in practice.
#[derive(Debug, Clone, Serialize)]
pub struct FileEdit {
    pub path: String,
    pub before: Option<String>,
    pub after: String,
}

const MAX_OUTPUT_CHARS: usize = 8_000;
const RUN_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn truncate(mut s: String) -> String {
    if s.len() > MAX_OUTPUT_CHARS {
        s.truncate(MAX_OUTPUT_CHARS);
        s.push_str("\n… [truncated]");
    }
    s
}

/// `load_skill`'s description is rebuilt from the currently-enabled
/// skills on every call (not cached), so the model sees what's
/// available without a separate discovery round trip.
fn load_skill_def() -> serde_json::Value {
    let skills = crate::skills::enabled();
    let description = if skills.is_empty() {
        "Load a reusable skill's full instructions by name. No skills are currently enabled.".to_string()
    } else {
        let mut lines = vec!["Load a reusable skill's full instructions by name. Available skills:".to_string()];
        for s in &skills {
            lines.push(format!("- {}: {}", s.name, s.description));
        }
        lines.join("\n")
    };
    json!({
        "type": "function",
        "function": {
            "name": "load_skill",
            "description": description,
            "parameters": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"],
            }
        }
    })
}

/// The tool definitions sent to the model, OpenAI function-calling shape.
/// `read_file`/`list_directory`/`load_skill` are always offered (all
/// read-only, like the sandboxed filesystem tools);
/// `write_file`/`run_command` only in full-access, so a read-only session
/// never even sees them as an option, on top of the server-side reject
/// below (defense in depth: a model can name a tool it wasn't offered).
/// Connected MCP servers' tools (`mcp__<server>__<tool>`) are always
/// appended too -- they're whatever access the user configured that
/// server with, not gated by this session's own permission mode.
pub fn tool_defs(permission: Permission, mcp: &crate::mcp::McpRegistry) -> Vec<serde_json::Value> {
    let mut defs = vec![
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a text file's contents, path relative to the project root.",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_directory",
                "description": "List entries of a directory, path relative to the project root.",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string", "description": "defaults to '.'" } },
                }
            }
        }),
        load_skill_def(),
    ];
    if permission == Permission::FullAccess {
        defs.push(json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write (overwrite) a text file, path relative to the project root.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" },
                    },
                    "required": ["path", "content"],
                }
            }
        }));
        defs.push(json!({
            "type": "function",
            "function": {
                "name": "run_command",
                "description": "Run a shell command with the project root as its working directory.",
                "parameters": {
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"],
                }
            }
        }));
    }
    defs.extend(mcp.tool_defs());
    defs
}

/// Resolves `rel` against `root`, purely lexically (no filesystem
/// access, so it works for paths that don't exist yet, e.g. a
/// `write_file` target). Rejects anything that normalizes outside
/// `root`, whether via `../` or a bare absolute path elsewhere.
///
/// Lexical only, not symlink-resistant -- a symlink inside
/// root pointing outside it would still be followed by the actual
/// read/write syscall. Add a canonicalize-and-recheck-after-resolve if
/// that ever matters for this threat model.
fn resolve_in_root(root: &Path, rel: &str) -> Result<PathBuf, String> {
    // Canonicalize root *before* joining (e.g. macOS's /var -> /private/var
    // symlink), so a legitimate in-root path doesn't get lexically
    // normalized against a different spelling of the same root than the
    // one it's checked against below.
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    let candidate = Path::new(rel);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root_canon.join(candidate)
    };

    let mut normalized = PathBuf::new();
    for comp in joined.components() {
        match comp {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }

    if normalized != root_canon && !normalized.starts_with(&root_canon) {
        return Err(format!("path '{rel}' escapes the project root"));
    }
    Ok(normalized)
}

/// Executes one tool call for real. Returns the content fed back to the
/// model plus a `ToolStep` for logging/UI, on both success and failure --
/// a rejected call is a normal tool result (the model gets told why),
/// not a hard error that aborts the whole turn.
pub async fn execute(
    name: &str,
    args_json: &str,
    root: &Path,
    permission: Permission,
    mcp: &crate::mcp::McpRegistry,
) -> (String, ToolStep) {
    let args: serde_json::Value = serde_json::from_str(args_json).unwrap_or(json!({}));
    let mut file_edit = None;
    let (ok, content, summary) = match name {
        "read_file" => read_file(&args, root),
        "list_directory" => list_directory(&args, root),
        "load_skill" => load_skill(&args),
        "write_file" => {
            let (result, edit) = write_file(&args, root, permission);
            file_edit = edit;
            result
        }
        "run_command" => run_command(&args, root, permission).await,
        other if other.starts_with("mcp__") => match mcp.dispatch(other, args_json).await {
            Some((ok, content, summary)) => (ok, truncate(content), summary),
            None => (
                false,
                format!("Error: unknown tool '{other}'"),
                format!("unknown tool '{other}'"),
            ),
        },
        other => (
            false,
            format!("Error: unknown tool '{other}'"),
            format!("unknown tool '{other}'"),
        ),
    };
    let step = ToolStep {
        name: name.to_string(),
        args_summary: truncate(args_json.to_string()),
        result_summary: summary,
        ok,
        file_edit,
    };
    (content, step)
}

fn read_file(args: &serde_json::Value, root: &Path) -> (bool, String, String) {
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return err("read_file requires a 'path' argument");
    };
    let resolved = match resolve_in_root(root, path) {
        Ok(p) => p,
        Err(e) => return err(&e),
    };
    match std::fs::read_to_string(&resolved) {
        Ok(body) => {
            let summary = format!("read {} bytes from {path}", body.len());
            (true, truncate(body), summary)
        }
        Err(e) => err(&format!("failed to read '{path}': {e}")),
    }
}

fn list_directory(args: &serde_json::Value, root: &Path) -> (bool, String, String) {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let resolved = match resolve_in_root(root, path) {
        Ok(p) => p,
        Err(e) => return err(&e),
    };
    let entries = match std::fs::read_dir(&resolved) {
        Ok(e) => e,
        Err(e) => return err(&format!("failed to list '{path}': {e}")),
    };
    let mut lines = Vec::new();
    for entry in entries.flatten() {
        let kind = if entry.path().is_dir() { "dir" } else { "file" };
        lines.push(format!("{kind}\t{}", entry.file_name().to_string_lossy()));
    }
    lines.sort();
    let summary = format!("listed {} entries in {path}", lines.len());
    (true, truncate(lines.join("\n")), summary)
}

fn load_skill(args: &serde_json::Value) -> (bool, String, String) {
    let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
        return err("load_skill requires a 'name' argument");
    };
    match crate::skills::get(name) {
        Some(skill) if skill.enabled => {
            let summary = format!("loaded skill '{name}'");
            (true, truncate(skill.body), summary)
        }
        Some(_) => err(&format!("skill '{name}' is disabled")),
        None => err(&format!("skill '{name}' not found")),
    }
}

fn write_file(
    args: &serde_json::Value,
    root: &Path,
    permission: Permission,
) -> ((bool, String, String), Option<FileEdit>) {
    if permission != Permission::FullAccess {
        return (err("write_file requires full-access permission (this session is read-only)"), None);
    }
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return (err("write_file requires a 'path' argument"), None);
    };
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let resolved = match resolve_in_root(root, path) {
        Ok(p) => p,
        Err(e) => return (err(&e), None),
    };
    // Captured before the write actually lands, so this is genuinely
    // the prior content (`None` means the file didn't exist yet).
    let before = std::fs::read_to_string(&resolved).ok();
    if let Some(parent) = resolved.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return (err(&format!("failed to create parent dirs for '{path}': {e}")), None);
        }
    }
    match std::fs::write(&resolved, content) {
        Ok(()) => {
            let summary = format!("wrote {} bytes to {path}", content.len());
            let edit = FileEdit {
                path: path.to_string(),
                before,
                after: content.to_string(),
            };
            ((true, summary.clone(), summary), Some(edit))
        }
        Err(e) => (err(&format!("failed to write '{path}': {e}")), None),
    }
}

async fn run_command(args: &serde_json::Value, root: &Path, permission: Permission) -> (bool, String, String) {
    if permission != Permission::FullAccess {
        return err("run_command requires full-access permission (this session is read-only)");
    }
    let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
        return err("run_command requires a 'command' argument");
    };
    let root = root.to_path_buf();
    let child = match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return err(&format!("failed to spawn command: {e}")),
    };

    let output = tokio::time::timeout(RUN_COMMAND_TIMEOUT, child.wait_with_output()).await;
    match output {
        Ok(Ok(out)) => {
            let mut body = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.is_empty() {
                body.push_str("\n[stderr]\n");
                body.push_str(&stderr);
            }
            let code = out.status.code().unwrap_or(-1);
            let summary = format!("ran '{command}' (exit {code})");
            (out.status.success(), truncate(body), summary)
        }
        Ok(Err(e)) => err(&format!("command failed: {e}")),
        Err(_) => err(&format!("command timed out after {}s", RUN_COMMAND_TIMEOUT.as_secs())),
    }
}

fn err(msg: &str) -> (bool, String, String) {
    (false, format!("Error: {msg}"), msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kiralm-tools-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_in_root_allows_nested_path() {
        let root = tmp_root();
        let resolved = resolve_in_root(&root, "notes/todo.txt").unwrap();
        assert!(resolved.starts_with(root.canonicalize().unwrap()));
    }

    #[test]
    fn resolve_in_root_rejects_dotdot_escape() {
        let root = tmp_root();
        assert!(resolve_in_root(&root, "../../etc/passwd").is_err());
    }

    #[test]
    fn resolve_in_root_rejects_absolute_outside() {
        let root = tmp_root();
        assert!(resolve_in_root(&root, "/etc/passwd").is_err());
    }

    #[tokio::test]
    async fn write_then_read_round_trips_for_real() {
        let root = tmp_root();
        let (content, step) = execute("write_file", r#"{"path":"out.txt","content":"hello"}"#, &root, Permission::FullAccess, &crate::mcp::McpRegistry::new()).await;
        assert!(step.ok, "{content}");
        assert!(std::fs::read_to_string(root.join("out.txt")).unwrap() == "hello");

        let (content, step) = execute("read_file", r#"{"path":"out.txt"}"#, &root, Permission::FullAccess, &crate::mcp::McpRegistry::new()).await;
        assert!(step.ok);
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn write_file_captures_real_before_and_after() {
        let root = tmp_root();
        std::fs::write(root.join("out.txt"), "old").unwrap();
        let (_content, step) =
            execute("write_file", r#"{"path":"out.txt","content":"new"}"#, &root, Permission::FullAccess, &crate::mcp::McpRegistry::new()).await;
        let edit = step.file_edit.expect("write_file should attach a FileEdit");
        assert_eq!(edit.before.as_deref(), Some("old"));
        assert_eq!(edit.after, "new");

        // A brand-new file has no prior content.
        let (_content, step) =
            execute("write_file", r#"{"path":"fresh.txt","content":"hi"}"#, &root, Permission::FullAccess, &crate::mcp::McpRegistry::new()).await;
        let edit = step.file_edit.expect("write_file should attach a FileEdit");
        assert_eq!(edit.before, None);
        assert_eq!(edit.after, "hi");
    }

    #[tokio::test]
    async fn write_file_rejected_in_read_only_mode() {
        let root = tmp_root();
        let (content, step) = execute("write_file", r#"{"path":"out.txt","content":"x"}"#, &root, Permission::ReadOnly, &crate::mcp::McpRegistry::new()).await;
        assert!(!step.ok);
        assert!(content.contains("full-access"));
        assert!(!root.join("out.txt").exists());
    }

    #[tokio::test]
    async fn read_outside_root_is_rejected() {
        let root = tmp_root();
        let (content, step) = execute("read_file", r#"{"path":"../../../etc/passwd"}"#, &root, Permission::FullAccess, &crate::mcp::McpRegistry::new()).await;
        assert!(!step.ok);
        assert!(content.contains("escapes"));
    }

    #[tokio::test]
    async fn run_command_rejected_in_read_only_mode() {
        let root = tmp_root();
        let (content, step) = execute("run_command", r#"{"command":"echo hi"}"#, &root, Permission::ReadOnly, &crate::mcp::McpRegistry::new()).await;
        assert!(!step.ok);
        assert!(content.contains("full-access"));
    }

    #[tokio::test]
    async fn run_command_executes_for_real_in_full_access() {
        let root = tmp_root();
        let (content, step) = execute("run_command", r#"{"command":"echo hi"}"#, &root, Permission::FullAccess, &crate::mcp::McpRegistry::new()).await;
        assert!(step.ok);
        assert!(content.contains("hi"));
    }

    #[tokio::test]
    async fn mcp_prefixed_calls_route_to_the_mcp_registry_not_unknown_tool() {
        let root = tmp_root();
        let (content, step) = execute("mcp__nope__echo", "{}", &root, Permission::ReadOnly, &crate::mcp::McpRegistry::new()).await;
        // Dispatched to the registry (real "no such server" error), not
        // this module's own generic "unknown tool" fallback.
        assert!(!step.ok);
        assert!(content.contains("no mcp tool matches"), "{content}");
    }
}
