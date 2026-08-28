//! `~/.kiralm/mcp_servers.json` -- manually-registered stdio MCP
//! servers, plus the live in-memory registry of connected child
//! processes: config CRUD, connect/disconnect, tools/list caching, and
//! `mcp__<server>__<tool>` dispatch. `rmcp` owns the wire protocol
//! (init handshake, tools/list, tools/call); this module only owns
//! naming, config, and lifecycle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use rmcp::model::{CallToolRequestParams, ContentBlock, Tool};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::kiralm_home;
use crate::marketplace::PluginRef;

pub fn mcp_servers_path() -> PathBuf {
    kiralm_home().join("mcp_servers.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Which marketplace plugin installed this server, if any -- `None`
    /// for one added by hand. What uninstall filters on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_by: Option<PluginRef>,
}

fn default_enabled() -> bool {
    true
}

pub fn load_configs() -> Vec<McpServerConfig> {
    load_configs_from(&mcp_servers_path())
}

// Same lenient posture as SkillsState/Settings: a missing or malformed
// file degrades to "no servers configured", never a daemon crash.
fn load_configs_from(path: &Path) -> Vec<McpServerConfig> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn save_configs(configs: &[McpServerConfig]) -> anyhow::Result<()> {
    save_configs_to(configs, &mcp_servers_path())
}

fn save_configs_to(configs: &[McpServerConfig], path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(configs)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn get_config(name: &str) -> Option<McpServerConfig> {
    load_configs().into_iter().find(|c| c.name == name)
}

pub fn add_config(cfg: McpServerConfig) -> Result<McpServerConfig, String> {
    if cfg.name.trim().is_empty() {
        return Err("'name' is required".to_string());
    }
    let mut configs = load_configs();
    if configs.iter().any(|c| c.name == cfg.name) {
        return Err(format!("mcp server '{}' already exists", cfg.name));
    }
    configs.push(cfg.clone());
    save_configs(&configs).map_err(|e| e.to_string())?;
    Ok(cfg)
}

pub fn set_config_enabled(name: &str, on: bool) -> Result<McpServerConfig, String> {
    let mut configs = load_configs();
    let Some(cfg) = configs.iter_mut().find(|c| c.name == name) else {
        return Err(format!("mcp server '{name}' not found"));
    };
    cfg.enabled = on;
    let updated = cfg.clone();
    save_configs(&configs).map_err(|e| e.to_string())?;
    Ok(updated)
}

pub fn delete_config(name: &str) -> Result<(), String> {
    let mut configs = load_configs();
    let before = configs.len();
    configs.retain(|c| c.name != name);
    if configs.len() == before {
        return Err(format!("mcp server '{name}' not found"));
    }
    save_configs(&configs).map_err(|e| e.to_string())
}

/// Deletes every server tagged as installed by this plugin, and
/// nothing else. Returns the names actually removed -- callers still
/// need to `McpRegistry::disconnect` each one themselves.
pub fn delete_by_plugin(marketplace: &str, plugin: &str) -> Vec<String> {
    let configs = load_configs();
    let (removed, kept): (Vec<_>, Vec<_>) = configs.into_iter().partition(|c| {
        c.installed_by
            .as_ref()
            .is_some_and(|p| p.marketplace == marketplace && p.plugin == plugin)
    });
    if !removed.is_empty() {
        let _ = save_configs(&kept);
    }
    removed.into_iter().map(|c| c.name).collect()
}

// ---------------------------------------------------------------------
// Live connections
// ---------------------------------------------------------------------

/// One connected server: the live `rmcp` client plus its cached
/// `tools/list`, refreshed on connect and by the `/refresh` endpoint.
pub struct McpConnection {
    client: RunningService<RoleClient, ()>,
    tools: RwLock<Vec<Tool>>,
}

#[derive(Clone)]
enum McpEntry {
    Connected(Arc<McpConnection>),
    Failed(String),
}

pub struct McpServerStatus {
    pub connected: bool,
    pub tool_count: usize,
    pub error: Option<String>,
}

/// In-memory only: which configured servers are actually spawned right
/// now, keyed by name. A server absent here is just not connected
/// (disabled, or never attempted) -- config on disk is the source of
/// truth for what *should* be running.
#[derive(Default)]
pub struct McpRegistry {
    conns: RwLock<HashMap<String, McpEntry>>,
}

fn namespaced(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// Splits `mcp__<server>__<tool>` back apart for a known `server`.
/// `None` if `full_name` doesn't belong to that server -- safe even if
/// server or tool names themselves contain `__`, since it only ever
/// checks against the exact configured server name.
fn strip_namespace<'a>(full_name: &'a str, server: &str) -> Option<&'a str> {
    full_name
        .strip_prefix("mcp__")?
        .strip_prefix(server)?
        .strip_prefix("__")
}

fn build_tool_defs(server: &str, tools: &[Tool]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": namespaced(server, &t.name),
                    "description": t.description.clone().unwrap_or_default(),
                    "parameters": serde_json::Value::Object((*t.input_schema).clone()),
                }
            })
        })
        .collect()
}

/// Hard ceiling on how many MCP tool defs get merged into a single
/// request -- a handful of heavily-tooled servers is fine, but nothing
/// else stops dozens of servers from bloating every turn's request with
/// tool schemas.
const MAX_TOOL_DEFS: usize = 60;

fn cap_tool_defs(all: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    if all.len() > MAX_TOOL_DEFS {
        tracing::warn!("mcp: {} tools available across connected servers, capping at {MAX_TOOL_DEFS}", all.len());
        all.into_iter().take(MAX_TOOL_DEFS).collect()
    } else {
        all
    }
}

fn render_content(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .map(|b| match b.as_text() {
            Some(t) => t.text.clone(),
            None => serde_json::to_string(b).unwrap_or_default(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl McpRegistry {
    pub fn new() -> McpRegistry {
        McpRegistry::default()
    }

    /// Spawns + handshakes + lists tools for every enabled server,
    /// best-effort: one server's failure is logged and leaves it out of
    /// `tool_defs()`, never aborts the rest or the caller.
    pub async fn connect_all_enabled(&self, configs: &[McpServerConfig]) {
        for cfg in configs.iter().filter(|c| c.enabled) {
            self.connect(cfg).await;
        }
    }

    /// Spawns the server's process and handshakes over stdio. Replaces
    /// any prior entry for this name (the old `Arc` is dropped, which
    /// closes its connection asynchronously via `rmcp`'s own drop guard).
    pub async fn connect(&self, cfg: &McpServerConfig) {
        let mut cmd = tokio::process::Command::new(&cfg.command);
        cmd.args(&cfg.args).envs(&cfg.env);
        let entry = match TokioChildProcess::new(cmd) {
            Ok(transport) => match ().serve(transport).await {
                Ok(client) => match client.list_all_tools().await {
                    Ok(tools) => McpEntry::Connected(Arc::new(McpConnection {
                        client,
                        tools: RwLock::new(tools),
                    })),
                    Err(e) => McpEntry::Failed(format!("tools/list failed: {e}")),
                },
                Err(e) => McpEntry::Failed(format!("mcp handshake failed: {e}")),
            },
            Err(e) => McpEntry::Failed(format!("failed to spawn '{}': {e}", cfg.command)),
        };
        if let McpEntry::Failed(msg) = &entry {
            tracing::warn!("mcp server '{}': {msg}", cfg.name);
        }
        self.conns.write().insert(cfg.name.clone(), entry);
    }

    pub fn disconnect(&self, name: &str) {
        self.conns.write().remove(name);
    }

    /// Refreshes a server's `tools/list` cache. Implemented as a full
    /// reconnect rather than a bare re-list: also recovers a server
    /// whose process died since the last connect.
    pub async fn refresh(&self, cfg: &McpServerConfig) {
        self.disconnect(&cfg.name);
        self.connect(cfg).await;
    }

    pub fn status(&self, name: &str) -> Option<McpServerStatus> {
        self.conns.read().get(name).map(|entry| match entry {
            McpEntry::Connected(conn) => McpServerStatus {
                connected: true,
                tool_count: conn.tools.read().len(),
                error: None,
            },
            McpEntry::Failed(e) => McpServerStatus {
                connected: false,
                tool_count: 0,
                error: Some(e.clone()),
            },
        })
    }

    /// Every connected server's tools, namespaced and merge-ready for
    /// `tools::tool_defs()`. Failed servers contribute nothing.
    pub fn tool_defs(&self) -> Vec<serde_json::Value> {
        let all: Vec<_> = self
            .conns
            .read()
            .iter()
            .flat_map(|(server, entry)| match entry {
                McpEntry::Connected(conn) => build_tool_defs(server, &conn.tools.read()),
                McpEntry::Failed(_) => Vec::new(),
            })
            .collect();
        cap_tool_defs(all)
    }

    /// Routes one `mcp__<server>__<tool>` call to its server's real
    /// `tools/call`. `None` only for a name that isn't namespaced at
    /// all, so `tools::execute` can fall through to its own "unknown
    /// tool" error; anything under the `mcp__` prefix always gets a
    /// real (possibly error) result here.
    pub async fn dispatch(&self, full_name: &str, args_json: &str) -> Option<(bool, String, String)> {
        if !full_name.starts_with("mcp__") {
            return None;
        }
        let found = {
            let conns = self.conns.read();
            conns.iter().find_map(|(server, entry)| {
                strip_namespace(full_name, server).map(|tool| (server.clone(), tool.to_string(), entry.clone()))
            })
        };
        let Some((server, tool_name, entry)) = found else {
            return Some((
                false,
                format!("Error: no mcp tool matches '{full_name}'"),
                format!("unknown mcp tool '{full_name}'"),
            ));
        };
        let McpEntry::Connected(conn) = entry else {
            return Some((
                false,
                format!("Error: mcp server '{server}' is not connected"),
                format!("mcp server '{server}' not connected"),
            ));
        };
        let args: serde_json::Value = serde_json::from_str(args_json).unwrap_or(json!({}));
        let params = CallToolRequestParams::new(tool_name.clone())
            .with_arguments(args.as_object().cloned().unwrap_or_default());
        match conn.client.call_tool(params).await {
            Ok(result) => {
                let ok = !result.is_error.unwrap_or(false);
                let summary = format!("called mcp tool '{tool_name}' on '{server}'");
                Some((ok, render_content(&result.content), summary))
            }
            Err(e) => Some((
                false,
                format!("Error: mcp tools/call to '{server}' failed: {e}"),
                format!("mcp tools/call to '{server}' failed"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path() -> PathBuf {
        std::env::temp_dir().join(format!("kiralm-mcp-test-{}.json", uuid::Uuid::new_v4()))
    }

    #[test]
    fn config_round_trips_through_save_and_load() {
        let path = tmp_path();
        let configs = vec![McpServerConfig {
            name: "everything".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@modelcontextprotocol/server-everything".to_string()],
            env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
            enabled: true,
            installed_by: None,
        }];
        save_configs_to(&configs, &path).unwrap();
        let loaded = load_configs_from(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "everything");
        assert_eq!(loaded[0].args, configs[0].args);
        assert_eq!(loaded[0].env.get("FOO"), Some(&"bar".to_string()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_config_file_does_not_crash() {
        let path = tmp_path();
        std::fs::write(&path, "not json at all {{{").unwrap();
        let loaded = load_configs_from(&path);
        assert!(loaded.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_config_file_is_empty_not_an_error() {
        let path = tmp_path();
        assert!(load_configs_from(&path).is_empty());
    }

    #[test]
    fn cap_tool_defs_leaves_a_small_list_untouched() {
        let defs: Vec<serde_json::Value> = (0..10).map(|i| json!({"i": i})).collect();
        assert_eq!(cap_tool_defs(defs.clone()), defs);
    }

    #[test]
    fn cap_tool_defs_truncates_past_the_max() {
        let defs: Vec<serde_json::Value> = (0..80).map(|i| json!({"i": i})).collect();
        let capped = cap_tool_defs(defs);
        assert_eq!(capped.len(), 60);
        assert_eq!(capped[0]["i"], 0);
    }

    #[test]
    fn tool_names_are_namespaced_with_server_and_verbatim_schema() {
        let schema = json!({"type": "object", "properties": {"x": {"type": "number"}}})
            .as_object()
            .unwrap()
            .clone();
        let tools = vec![Tool::new("echo", "Echoes input", schema.clone())];
        let defs = build_tool_defs("everything", &tools);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0]["function"]["name"], "mcp__everything__echo");
        assert_eq!(defs[0]["function"]["parameters"], serde_json::Value::Object(schema));
    }

    #[test]
    fn strip_namespace_recovers_the_real_tool_name() {
        assert_eq!(strip_namespace("mcp__everything__echo", "everything"), Some("echo"));
        // A tool name that itself contains `__` still round-trips.
        assert_eq!(strip_namespace("mcp__everything__list__all", "everything"), Some("list__all"));
        // Doesn't match a different server.
        assert_eq!(strip_namespace("mcp__everything__echo", "other"), None);
        // Not namespaced at all.
        assert_eq!(strip_namespace("read_file", "everything"), None);
    }

    #[tokio::test]
    async fn dispatch_ignores_non_mcp_names() {
        let registry = McpRegistry::new();
        assert!(registry.dispatch("read_file", "{}").await.is_none());
    }

    #[tokio::test]
    async fn dispatch_reports_unknown_server_or_tool() {
        let registry = McpRegistry::new();
        let (ok, content, _) = registry.dispatch("mcp__nope__echo", "{}").await.unwrap();
        assert!(!ok);
        assert!(content.contains("no mcp tool matches"));
    }

    #[tokio::test]
    async fn dispatch_reports_a_failed_servers_tools_as_unreachable() {
        let registry = McpRegistry::new();
        registry
            .conns
            .write()
            .insert("everything".to_string(), McpEntry::Failed("boom".to_string()));
        let (ok, content, _) = registry.dispatch("mcp__everything__echo", "{}").await.unwrap();
        assert!(!ok);
        assert!(content.contains("not connected"));

        let status = registry.status("everything").unwrap();
        assert!(!status.connected);
        assert_eq!(status.tool_count, 0);
        assert_eq!(status.error.as_deref(), Some("boom"));
    }

    #[test]
    fn crud_round_trips_add_toggle_delete() {
        let _guard = crate::config::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kiralm-mcp-crud-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var("HOME").ok();
        std::env::set_var("HOME", &dir);

        let cfg = McpServerConfig {
            name: "everything".to_string(),
            command: "npx".to_string(),
            args: vec![],
            env: HashMap::new(),
            enabled: true,
            installed_by: None,
        };
        add_config(cfg.clone()).unwrap();
        assert!(add_config(cfg.clone()).is_err(), "duplicate name should be rejected");

        let updated = set_config_enabled("everything", false).unwrap();
        assert!(!updated.enabled);
        assert!(!get_config("everything").unwrap().enabled);

        delete_config("everything").unwrap();
        assert!(get_config("everything").is_none());
        assert!(delete_config("everything").is_err());

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
