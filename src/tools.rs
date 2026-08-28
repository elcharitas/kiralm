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
const MAX_WALK_FILES: usize = 5_000;
const MAX_SEARCH_RESULTS: usize = 200;
const MAX_GLOB_RESULTS: usize = 500;
const WEB_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const SKIP_DIR_NAMES: [&str; 4] = ["node_modules", "target", "dist", ".venv"];

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
/// `run_dsh_profile` only shows up once there's actually a profile to
/// run and a real `dsh` binary to run it with -- the one adapter where
/// "not present" is the common, expected case.
pub fn tool_defs(permission: Permission, mcp: &crate::mcp::McpRegistry, dsh_bin: &str) -> Vec<serde_json::Value> {
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
        json!({
            "type": "function",
            "function": {
                "name": "search_files",
                "description": "Search file contents for a literal substring, recursively from a directory (defaults to the project root). Returns matching 'path:line: text' entries.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" },
                        "path": { "type": "string", "description": "defaults to '.'" },
                    },
                    "required": ["pattern"],
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "glob",
                "description": "Find files by name pattern ('*' wildcards supported), recursively from a directory (defaults to the project root).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" },
                        "path": { "type": "string", "description": "defaults to '.'" },
                    },
                    "required": ["pattern"],
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git_diff",
                "description": "Show a unified diff of every uncommitted change in the project (staged and unstaged).",
                "parameters": { "type": "object", "properties": {} }
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
        defs.push(json!({
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Replace an exact snippet of text in an existing file with new text, without rewriting the whole file. 'old_string' must match exactly once unless 'replace_all' is set.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_string": { "type": "string" },
                        "new_string": { "type": "string" },
                        "replace_all": { "type": "boolean", "description": "defaults to false" },
                    },
                    "required": ["path", "old_string", "new_string"],
                }
            }
        }));
        defs.push(json!({
            "type": "function",
            "function": {
                "name": "delete_file",
                "description": "Delete a file or directory (recursively), path relative to the project root.",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                }
            }
        }));
        defs.push(json!({
            "type": "function",
            "function": {
                "name": "move_file",
                "description": "Move or rename a file or directory within the project.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "from": { "type": "string" },
                        "to": { "type": "string" },
                    },
                    "required": ["from", "to"],
                }
            }
        }));
        defs.push(json!({
            "type": "function",
            "function": {
                "name": "git_commit",
                "description": "Stage every change and create a git commit with the given message.",
                "parameters": {
                    "type": "object",
                    "properties": { "message": { "type": "string" } },
                    "required": ["message"],
                }
            }
        }));
        defs.push(json!({
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch a URL over HTTP(S) and return its text content (HTML is stripped to plain text).",
                "parameters": {
                    "type": "object",
                    "properties": { "url": { "type": "string" } },
                    "required": ["url"],
                }
            }
        }));
    }
    if !crate::dsh::list_profiles().is_empty() && crate::dsh::detect(dsh_bin).is_ok() {
        defs.push(json!({
            "type": "function",
            "function": {
                "name": "run_dsh_profile",
                "description": "Run a one-shot DeepSeek Harness (dsh) session against a configured profile. Fresh session per call, prints the final answer.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "profile": { "type": "string" },
                        "prompt": { "type": "string" },
                    },
                    "required": ["profile", "prompt"],
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

/// Recursively lists files under `root` for `search_files`/`glob`,
/// skipping hidden dirs and common heavy/irrelevant ones, capped at
/// `MAX_WALK_FILES` so one call can't turn into a multi-second scan of
/// a huge repo (or a runaway one via a symlink cycle).
fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= MAX_WALK_FILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIP_DIR_NAMES.contains(&name.as_ref()) {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
                if out.len() >= MAX_WALK_FILES {
                    break;
                }
            }
        }
    }
    out
}

/// Minimal `*`-only wildcard matcher (no `?`/`[]`/`**`) -- enough for
/// "find files named *.rs" without pulling in a glob crate.
fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !text[pos..].starts_with(part) {
                return false;
            }
            pos += part.len();
        } else if i == parts.len() - 1 {
            return text[pos..].ends_with(part);
        } else {
            match text[pos..].find(part) {
                Some(idx) => pos += idx + part.len(),
                None => return false,
            }
        }
    }
    true
}

/// Naive tag stripper for `web_fetch`'s HTML pages: drops everything
/// between `<`/`>` and collapses whitespace runs. Good enough to turn a
/// docs page into readable text without a full HTML parser dependency;
/// won't handle `<script>`/`<style>` bodies specially, so their raw
/// text still leaks through.
fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
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
    dsh_bin: &str,
) -> (String, ToolStep) {
    let args: serde_json::Value = serde_json::from_str(args_json).unwrap_or(json!({}));
    let mut file_edit = None;
    let (ok, content, summary) = match name {
        "read_file" => read_file(&args, root),
        "list_directory" => list_directory(&args, root),
        "search_files" => search_files(&args, root),
        "glob" => glob_tool(&args, root),
        "git_diff" => git_diff_tool(root).await,
        "load_skill" => load_skill(&args),
        "write_file" => {
            let (result, edit) = write_file(&args, root, permission);
            file_edit = edit;
            result
        }
        "edit_file" => {
            let (result, edit) = edit_file(&args, root, permission);
            file_edit = edit;
            result
        }
        "delete_file" => delete_file(&args, root, permission),
        "move_file" => move_file(&args, root, permission),
        "git_commit" => git_commit_tool(&args, root, permission).await,
        "web_fetch" => web_fetch(&args, permission).await,
        "run_command" => run_command(&args, root, permission).await,
        "run_dsh_profile" => run_dsh_profile(&args, dsh_bin).await,
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

fn search_files(args: &serde_json::Value, root: &Path) -> (bool, String, String) {
    let Some(pattern) = args.get("pattern").and_then(|v| v.as_str()) else {
        return err("search_files requires a 'pattern' argument");
    };
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let search_root = match resolve_in_root(root, path) {
        Ok(p) => p,
        Err(e) => return err(&e),
    };
    // `resolve_in_root` canonicalizes internally (e.g. macOS's
    // /var -> /private/var), so paths it hands back must be stripped
    // against that same canonical root, not the raw `root` argument.
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut matches = Vec::new();
    'files: for file in walk_files(&search_root) {
        let Ok(bytes) = std::fs::read(&file) else { continue };
        // A null byte in the first slice is a cheap, reliable-enough
        // binary check -- good enough to skip images/archives/etc.
        if bytes[..bytes.len().min(512)].contains(&0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let rel = file.strip_prefix(&root_canon).unwrap_or(&file).display().to_string();
        for (i, line) in text.lines().enumerate() {
            if line.contains(pattern) {
                matches.push(format!("{rel}:{}: {}", i + 1, line.trim()));
                if matches.len() >= MAX_SEARCH_RESULTS {
                    break 'files;
                }
            }
        }
    }
    let summary = format!("found {} matches for '{pattern}'", matches.len());
    (true, truncate(matches.join("\n")), summary)
}

fn glob_tool(args: &serde_json::Value, root: &Path) -> (bool, String, String) {
    let Some(pattern) = args.get("pattern").and_then(|v| v.as_str()) else {
        return err("glob requires a 'pattern' argument");
    };
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let search_root = match resolve_in_root(root, path) {
        Ok(p) => p,
        Err(e) => return err(&e),
    };
    // See `search_files`'s matching comment: strip against the same
    // canonical root `resolve_in_root` resolved paths against.
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut matches: Vec<String> = walk_files(&search_root)
        .into_iter()
        .filter_map(|file| {
            let rel = file.strip_prefix(&root_canon).ok()?.display().to_string();
            let name = file.file_name()?.to_string_lossy().into_owned();
            (glob_match(pattern, &name) || glob_match(pattern, &rel)).then_some(rel)
        })
        .take(MAX_GLOB_RESULTS)
        .collect();
    matches.sort();
    let summary = format!("found {} files matching '{pattern}'", matches.len());
    (true, truncate(matches.join("\n")), summary)
}

async fn git_diff_tool(root: &Path) -> (bool, String, String) {
    match crate::git::diff(root).await {
        Ok(out) if out.trim().is_empty() => (true, "no uncommitted changes".to_string(), "no changes".to_string()),
        Ok(out) => (true, truncate(out), "read uncommitted diff".to_string()),
        Err(e) => err(&e),
    }
}

async fn git_commit_tool(args: &serde_json::Value, root: &Path, permission: Permission) -> (bool, String, String) {
    if permission != Permission::FullAccess {
        return err("git_commit requires full-access permission (this session is read-only)");
    }
    let Some(message) = args.get("message").and_then(|v| v.as_str()) else {
        return err("git_commit requires a 'message' argument");
    };
    match crate::git::commit(root, message).await {
        Ok(result) => {
            let summary = format!("committed {} ({})", result.short_hash, result.summary);
            (true, summary.clone(), summary)
        }
        Err(e) => err(&e),
    }
}

fn edit_file(
    args: &serde_json::Value,
    root: &Path,
    permission: Permission,
) -> ((bool, String, String), Option<FileEdit>) {
    if permission != Permission::FullAccess {
        return (err("edit_file requires full-access permission (this session is read-only)"), None);
    }
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return (err("edit_file requires a 'path' argument"), None);
    };
    let Some(old_string) = args.get("old_string").and_then(|v| v.as_str()) else {
        return (err("edit_file requires an 'old_string' argument"), None);
    };
    let new_string = args.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
    let replace_all = args.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
    let resolved = match resolve_in_root(root, path) {
        Ok(p) => p,
        Err(e) => return (err(&e), None),
    };
    let before = match std::fs::read_to_string(&resolved) {
        Ok(s) => s,
        Err(e) => return (err(&format!("failed to read '{path}': {e}")), None),
    };
    let occurrences = before.matches(old_string).count();
    if occurrences == 0 {
        return (err(&format!("'old_string' not found in '{path}'")), None);
    }
    if occurrences > 1 && !replace_all {
        return (
            err(&format!(
                "'old_string' matches {occurrences} places in '{path}'; include more surrounding \
                 context to make it unique, or set replace_all"
            )),
            None,
        );
    }
    let after =
        if replace_all { before.replace(old_string, new_string) } else { before.replacen(old_string, new_string, 1) };
    match std::fs::write(&resolved, &after) {
        Ok(()) => {
            let plural = if occurrences == 1 { "" } else { "s" };
            let summary = format!("edited {path} ({occurrences} replacement{plural})");
            let edit = FileEdit { path: path.to_string(), before: Some(before), after: after.clone() };
            ((true, summary.clone(), summary), Some(edit))
        }
        Err(e) => (err(&format!("failed to write '{path}': {e}")), None),
    }
}

fn delete_file(args: &serde_json::Value, root: &Path, permission: Permission) -> (bool, String, String) {
    if permission != Permission::FullAccess {
        return err("delete_file requires full-access permission (this session is read-only)");
    }
    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
        return err("delete_file requires a 'path' argument");
    };
    let resolved = match resolve_in_root(root, path) {
        Ok(p) => p,
        Err(e) => return err(&e),
    };
    let result = if resolved.is_dir() { std::fs::remove_dir_all(&resolved) } else { std::fs::remove_file(&resolved) };
    match result {
        Ok(()) => {
            let summary = format!("deleted {path}");
            (true, summary.clone(), summary)
        }
        Err(e) => err(&format!("failed to delete '{path}': {e}")),
    }
}

fn move_file(args: &serde_json::Value, root: &Path, permission: Permission) -> (bool, String, String) {
    if permission != Permission::FullAccess {
        return err("move_file requires full-access permission (this session is read-only)");
    }
    let Some(from) = args.get("from").and_then(|v| v.as_str()) else {
        return err("move_file requires a 'from' argument");
    };
    let Some(to) = args.get("to").and_then(|v| v.as_str()) else {
        return err("move_file requires a 'to' argument");
    };
    let resolved_from = match resolve_in_root(root, from) {
        Ok(p) => p,
        Err(e) => return err(&e),
    };
    let resolved_to = match resolve_in_root(root, to) {
        Ok(p) => p,
        Err(e) => return err(&e),
    };
    if let Some(parent) = resolved_to.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return err(&format!("failed to create parent dirs for '{to}': {e}"));
        }
    }
    match std::fs::rename(&resolved_from, &resolved_to) {
        Ok(()) => {
            let summary = format!("moved {from} to {to}");
            (true, summary.clone(), summary)
        }
        Err(e) => err(&format!("failed to move '{from}' to '{to}': {e}")),
    }
}

async fn web_fetch(args: &serde_json::Value, permission: Permission) -> (bool, String, String) {
    if permission != Permission::FullAccess {
        return err("web_fetch requires full-access permission (this session is read-only)");
    }
    let Some(url) = args.get("url").and_then(|v| v.as_str()) else {
        return err("web_fetch requires a 'url' argument");
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return err("'url' must be an http(s) URL");
    }
    let client = match reqwest::Client::builder().timeout(WEB_FETCH_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return err(&format!("failed to build http client: {e}")),
    };
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => return err(&format!("request to '{url}' failed: {e}")),
    };
    let status = resp.status();
    let is_html = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("html"));
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => return err(&format!("failed to read response body: {e}")),
    };
    if !status.is_success() {
        return err(&format!("'{url}' returned {status}"));
    }
    let body = if is_html { strip_html_tags(&body) } else { body };
    let summary = format!("fetched {url} ({status})");
    (true, truncate(body), summary)
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

async fn run_dsh_profile(args: &serde_json::Value, dsh_bin: &str) -> (bool, String, String) {
    let Some(profile) = args.get("profile").and_then(|v| v.as_str()) else {
        return err("run_dsh_profile requires a 'profile' argument");
    };
    let Some(prompt) = args.get("prompt").and_then(|v| v.as_str()) else {
        return err("run_dsh_profile requires a 'prompt' argument");
    };
    match crate::dsh::run_profile(dsh_bin, profile, prompt).await {
        Ok(stdout) => {
            let summary = format!("ran dsh profile '{profile}'");
            (true, truncate(stdout), summary)
        }
        Err(e) => err(&e),
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
        let (content, step) = execute("write_file", r#"{"path":"out.txt","content":"hello"}"#, &root, Permission::FullAccess, &crate::mcp::McpRegistry::new(), "dsh").await;
        assert!(step.ok, "{content}");
        assert!(std::fs::read_to_string(root.join("out.txt")).unwrap() == "hello");

        let (content, step) = execute("read_file", r#"{"path":"out.txt"}"#, &root, Permission::FullAccess, &crate::mcp::McpRegistry::new(), "dsh").await;
        assert!(step.ok);
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn write_file_captures_real_before_and_after() {
        let root = tmp_root();
        std::fs::write(root.join("out.txt"), "old").unwrap();
        let (_content, step) =
            execute("write_file", r#"{"path":"out.txt","content":"new"}"#, &root, Permission::FullAccess, &crate::mcp::McpRegistry::new(), "dsh").await;
        let edit = step.file_edit.expect("write_file should attach a FileEdit");
        assert_eq!(edit.before.as_deref(), Some("old"));
        assert_eq!(edit.after, "new");

        // A brand-new file has no prior content.
        let (_content, step) =
            execute("write_file", r#"{"path":"fresh.txt","content":"hi"}"#, &root, Permission::FullAccess, &crate::mcp::McpRegistry::new(), "dsh").await;
        let edit = step.file_edit.expect("write_file should attach a FileEdit");
        assert_eq!(edit.before, None);
        assert_eq!(edit.after, "hi");
    }

    #[tokio::test]
    async fn write_file_rejected_in_read_only_mode() {
        let root = tmp_root();
        let (content, step) = execute("write_file", r#"{"path":"out.txt","content":"x"}"#, &root, Permission::ReadOnly, &crate::mcp::McpRegistry::new(), "dsh").await;
        assert!(!step.ok);
        assert!(content.contains("full-access"));
        assert!(!root.join("out.txt").exists());
    }

    #[tokio::test]
    async fn read_outside_root_is_rejected() {
        let root = tmp_root();
        let (content, step) = execute("read_file", r#"{"path":"../../../etc/passwd"}"#, &root, Permission::FullAccess, &crate::mcp::McpRegistry::new(), "dsh").await;
        assert!(!step.ok);
        assert!(content.contains("escapes"));
    }

    #[tokio::test]
    async fn run_command_rejected_in_read_only_mode() {
        let root = tmp_root();
        let (content, step) = execute("run_command", r#"{"command":"echo hi"}"#, &root, Permission::ReadOnly, &crate::mcp::McpRegistry::new(), "dsh").await;
        assert!(!step.ok);
        assert!(content.contains("full-access"));
    }

    #[tokio::test]
    async fn run_command_executes_for_real_in_full_access() {
        let root = tmp_root();
        let (content, step) = execute("run_command", r#"{"command":"echo hi"}"#, &root, Permission::FullAccess, &crate::mcp::McpRegistry::new(), "dsh").await;
        assert!(step.ok);
        assert!(content.contains("hi"));
    }

    #[tokio::test]
    async fn mcp_prefixed_calls_route_to_the_mcp_registry_not_unknown_tool() {
        let root = tmp_root();
        let (content, step) = execute("mcp__nope__echo", "{}", &root, Permission::ReadOnly, &crate::mcp::McpRegistry::new(), "dsh").await;
        // Dispatched to the registry (real "no such server" error), not
        // this module's own generic "unknown tool" fallback.
        assert!(!step.ok);
        assert!(content.contains("no mcp tool matches"), "{content}");
    }

    struct TempHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
        dir: PathBuf,
    }

    impl TempHome {
        fn new() -> TempHome {
            let guard = crate::config::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("kiralm-tools-dsh-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let prev = std::env::var("HOME").ok();
            std::env::set_var("HOME", &dir);
            TempHome { _guard: guard, prev, dir }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn run_dsh_profile_absent_without_a_profile_even_if_dsh_is_on_path() {
        let _home = TempHome::new();
        // "sh" always exists, standing in for a "detected" dsh binary --
        // still shouldn't show up with zero profiles configured.
        let defs = tool_defs(Permission::FullAccess, &crate::mcp::McpRegistry::new(), "sh");
        assert!(!defs.iter().any(|d| d["function"]["name"] == "run_dsh_profile"));
    }

    #[test]
    fn run_dsh_profile_absent_with_a_profile_but_no_real_dsh_binary() {
        let _home = TempHome::new();
        crate::dsh::create_profile("p1").unwrap();
        let defs = tool_defs(Permission::FullAccess, &crate::mcp::McpRegistry::new(), "/definitely/not/a/real/dsh");
        assert!(!defs.iter().any(|d| d["function"]["name"] == "run_dsh_profile"));
    }

    #[test]
    fn run_dsh_profile_present_once_a_profile_exists_and_dsh_is_detected() {
        let _home = TempHome::new();
        crate::dsh::create_profile("p1").unwrap();
        let defs = tool_defs(Permission::FullAccess, &crate::mcp::McpRegistry::new(), "sh");
        assert!(defs.iter().any(|d| d["function"]["name"] == "run_dsh_profile"));
    }

    fn mcp() -> crate::mcp::McpRegistry {
        crate::mcp::McpRegistry::new()
    }

    #[tokio::test]
    async fn edit_file_replaces_a_unique_match() {
        let root = tmp_root();
        std::fs::write(root.join("a.txt"), "hello world").unwrap();
        let (content, step) = execute(
            "edit_file",
            r#"{"path":"a.txt","old_string":"world","new_string":"there"}"#,
            &root,
            Permission::FullAccess,
            &mcp(),
            "dsh",
        )
        .await;
        assert!(step.ok, "{content}");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "hello there");
        let edit = step.file_edit.unwrap();
        assert_eq!(edit.before.as_deref(), Some("hello world"));
        assert_eq!(edit.after, "hello there");
    }

    #[tokio::test]
    async fn edit_file_rejects_an_ambiguous_match_without_replace_all() {
        let root = tmp_root();
        std::fs::write(root.join("a.txt"), "x x x").unwrap();
        let (content, step) =
            execute("edit_file", r#"{"path":"a.txt","old_string":"x","new_string":"y"}"#, &root, Permission::FullAccess, &mcp(), "dsh")
                .await;
        assert!(!step.ok);
        assert!(content.contains("matches 3 places"));
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "x x x");
    }

    #[tokio::test]
    async fn edit_file_replace_all_replaces_every_match() {
        let root = tmp_root();
        std::fs::write(root.join("a.txt"), "x x x").unwrap();
        let (_content, step) = execute(
            "edit_file",
            r#"{"path":"a.txt","old_string":"x","new_string":"y","replace_all":true}"#,
            &root,
            Permission::FullAccess,
            &mcp(),
            "dsh",
        )
        .await;
        assert!(step.ok);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "y y y");
    }

    #[tokio::test]
    async fn edit_file_reports_missing_old_string() {
        let root = tmp_root();
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        let (content, step) =
            execute("edit_file", r#"{"path":"a.txt","old_string":"nope","new_string":"y"}"#, &root, Permission::FullAccess, &mcp(), "dsh")
                .await;
        assert!(!step.ok);
        assert!(content.contains("not found"));
    }

    #[tokio::test]
    async fn edit_file_rejected_in_read_only_mode() {
        let root = tmp_root();
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        let (content, step) =
            execute("edit_file", r#"{"path":"a.txt","old_string":"hello","new_string":"hi"}"#, &root, Permission::ReadOnly, &mcp(), "dsh")
                .await;
        assert!(!step.ok);
        assert!(content.contains("full-access"));
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "hello");
    }

    #[tokio::test]
    async fn delete_file_removes_a_real_file() {
        let root = tmp_root();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let (_content, step) = execute("delete_file", r#"{"path":"a.txt"}"#, &root, Permission::FullAccess, &mcp(), "dsh").await;
        assert!(step.ok);
        assert!(!root.join("a.txt").exists());
    }

    #[tokio::test]
    async fn delete_file_removes_a_directory_recursively() {
        let root = tmp_root();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/a.txt"), "x").unwrap();
        let (_content, step) = execute("delete_file", r#"{"path":"sub"}"#, &root, Permission::FullAccess, &mcp(), "dsh").await;
        assert!(step.ok);
        assert!(!root.join("sub").exists());
    }

    #[tokio::test]
    async fn delete_file_rejected_in_read_only_mode() {
        let root = tmp_root();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let (content, step) = execute("delete_file", r#"{"path":"a.txt"}"#, &root, Permission::ReadOnly, &mcp(), "dsh").await;
        assert!(!step.ok);
        assert!(content.contains("full-access"));
        assert!(root.join("a.txt").exists());
    }

    #[tokio::test]
    async fn move_file_renames_within_root() {
        let root = tmp_root();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let (_content, step) =
            execute("move_file", r#"{"from":"a.txt","to":"nested/b.txt"}"#, &root, Permission::FullAccess, &mcp(), "dsh").await;
        assert!(step.ok);
        assert!(!root.join("a.txt").exists());
        assert_eq!(std::fs::read_to_string(root.join("nested/b.txt")).unwrap(), "x");
    }

    #[tokio::test]
    async fn move_file_rejected_in_read_only_mode() {
        let root = tmp_root();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let (content, step) =
            execute("move_file", r#"{"from":"a.txt","to":"b.txt"}"#, &root, Permission::ReadOnly, &mcp(), "dsh").await;
        assert!(!step.ok);
        assert!(content.contains("full-access"));
    }

    #[tokio::test]
    async fn search_files_finds_a_match_with_line_number() {
        let root = tmp_root();
        std::fs::write(root.join("a.txt"), "one\ntwo needle\nthree").unwrap();
        let (content, step) = execute("search_files", r#"{"pattern":"needle"}"#, &root, Permission::ReadOnly, &mcp(), "dsh").await;
        assert!(step.ok);
        assert!(content.contains("a.txt:2: two needle"), "{content}");
    }

    #[tokio::test]
    async fn search_files_skips_binary_looking_files() {
        let root = tmp_root();
        std::fs::write(root.join("bin.dat"), [0u8, 1, 2, b'n', b'e', b'e', b'd', b'l', b'e']).unwrap();
        let (content, step) = execute("search_files", r#"{"pattern":"needle"}"#, &root, Permission::ReadOnly, &mcp(), "dsh").await;
        assert!(step.ok);
        assert!(!content.contains("bin.dat"));
    }

    #[tokio::test]
    async fn glob_matches_a_wildcard_pattern() {
        let root = tmp_root();
        std::fs::write(root.join("a.rs"), "").unwrap();
        std::fs::write(root.join("a.txt"), "").unwrap();
        let (content, step) = execute("glob", r#"{"pattern":"*.rs"}"#, &root, Permission::ReadOnly, &mcp(), "dsh").await;
        assert!(step.ok);
        assert_eq!(content, "a.rs");
    }

    #[tokio::test]
    async fn git_diff_reports_not_a_repo_cleanly() {
        let root = tmp_root();
        let (content, step) = execute("git_diff", "{}", &root, Permission::ReadOnly, &mcp(), "dsh").await;
        assert!(!step.ok);
        assert!(content.contains("not a git repository"));
    }

    async fn git(root: &Path, args: &[&str]) {
        let status = tokio::process::Command::new("git").args(args).current_dir(root).status().await.unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[tokio::test]
    async fn git_diff_and_commit_round_trip_for_real() {
        let root = tmp_root();
        git(&root, &["init", "--quiet"]).await;
        git(&root, &["config", "user.email", "test@example.com"]).await;
        git(&root, &["config", "user.name", "Test"]).await;
        std::fs::write(root.join("a.txt"), "one").unwrap();
        git(&root, &["add", "-A"]).await;
        git(&root, &["commit", "--quiet", "-m", "seed"]).await;

        std::fs::write(root.join("a.txt"), "two").unwrap();
        let (content, step) = execute("git_diff", "{}", &root, Permission::ReadOnly, &mcp(), "dsh").await;
        assert!(step.ok);
        assert!(content.contains("-one") && content.contains("+two"), "{content}");

        let (content, step) =
            execute("git_commit", r#"{"message":"second"}"#, &root, Permission::FullAccess, &mcp(), "dsh").await;
        assert!(step.ok, "{content}");

        let (content, step) = execute("git_diff", "{}", &root, Permission::ReadOnly, &mcp(), "dsh").await;
        assert!(step.ok);
        assert_eq!(content, "no uncommitted changes");
    }

    #[tokio::test]
    async fn git_commit_rejected_in_read_only_mode() {
        let root = tmp_root();
        git(&root, &["init", "--quiet"]).await;
        let (content, step) =
            execute("git_commit", r#"{"message":"x"}"#, &root, Permission::ReadOnly, &mcp(), "dsh").await;
        assert!(!step.ok);
        assert!(content.contains("full-access"));
    }

    #[tokio::test]
    async fn web_fetch_rejected_in_read_only_mode() {
        let root = tmp_root();
        let (content, step) =
            execute("web_fetch", r#"{"url":"http://example.com"}"#, &root, Permission::ReadOnly, &mcp(), "dsh").await;
        assert!(!step.ok);
        assert!(content.contains("full-access"));
    }

    #[tokio::test]
    async fn web_fetch_rejects_a_non_http_url() {
        let root = tmp_root();
        let (content, step) =
            execute("web_fetch", r#"{"url":"file:///etc/passwd"}"#, &root, Permission::FullAccess, &mcp(), "dsh").await;
        assert!(!step.ok);
        assert!(content.contains("http(s)"));
    }

    #[test]
    fn glob_match_supports_leading_trailing_and_middle_wildcards() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "main.txt"));
        assert!(glob_match("test_*", "test_foo"));
        assert!(glob_match("a*b*c", "a123b456c"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exactish"));
    }

    #[test]
    fn strip_html_tags_drops_markup_and_collapses_whitespace() {
        let html = "<html>\n<body>  <p>Hello   <b>World</b></p>\n</body></html>";
        assert_eq!(strip_html_tags(html), "Hello World");
    }

    #[test]
    fn read_only_sessions_are_not_offered_mutating_tools() {
        let defs = tool_defs(Permission::ReadOnly, &mcp(), "dsh");
        let names: Vec<&str> = defs.iter().filter_map(|d| d["function"]["name"].as_str()).collect();
        for mutating in ["write_file", "run_command", "edit_file", "delete_file", "move_file", "git_commit", "web_fetch"] {
            assert!(!names.contains(&mutating), "read-only session should not see '{mutating}'");
        }
        for readable in ["read_file", "list_directory", "search_files", "glob", "git_diff", "load_skill"] {
            assert!(names.contains(&readable), "'{readable}' should always be offered");
        }
    }
}
