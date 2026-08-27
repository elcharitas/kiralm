//! Agent Plugins 1.0 (agent-plugins.org): a single plugin per install,
//! sourced from a local folder or a git URL pointing at the plugin's
//! own root -- no marketplace-listing wrapper. Reuses marketplace.rs's
//! `PluginRef` tagging and `InstallOutcome`/`UninstallOutcome` shapes,
//! and skills.rs/mcp.rs's own stores; `~/.kiralm/agent_plugins.json`
//! is the one thing those stores can't recover on their own: which
//! plugin.json a given install's tagged items came from.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::kiralm_home;
use crate::marketplace::{self, InstallOutcome, PluginRef, UninstallOutcome};
use crate::mcp::{self, McpServerConfig};
use crate::skills;

pub fn agent_plugins_path() -> PathBuf {
    kiralm_home().join("agent_plugins.json")
}

/// plugin.json's closed 10-field schema. `extensions`' inner keys are
/// never interpreted, just stored and returned verbatim.
#[derive(Debug, Clone, Deserialize)]
struct PluginManifest {
    #[serde(rename = "$schema")]
    schema: String,
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    author: Option<serde_json::Value>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    extensions: Option<serde_json::Value>,
}

/// 1-64 chars, lowercase alphanumeric + hyphens/dots, the whole allowed range.
fn valid_plugin_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
}

fn parse_plugin_json(raw: &str) -> Result<PluginManifest, String> {
    let manifest: PluginManifest =
        serde_json::from_str(raw).map_err(|e| format!("invalid plugin.json: {e}"))?;
    if manifest.schema.trim().is_empty() {
        return Err("plugin.json '$schema' is required".to_string());
    }
    if !valid_plugin_name(&manifest.name) {
        return Err(format!(
            "plugin.json 'name' must be 1-64 chars of lowercase alphanumeric, hyphens, dots (got '{}')",
            manifest.name
        ));
    }
    Ok(manifest)
}

/// mcp.json's per-server entry. `command` present means stdio, the only
/// transport the daemon's `mcp.rs` client actually speaks; anything
/// else (a bare `url`, or an explicit `type`) is a Streamable HTTP or
/// legacy HTTP+SSE entry, skipped with a warning rather than dropped
/// silently or failing the whole install.
#[derive(Debug, Deserialize)]
struct RawMcpEntry {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default, rename = "type")]
    transport_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McpJsonFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: HashMap<String, RawMcpEntry>,
}

/// One installed Agent Plugin, as returned by list/install -- plugin.json's
/// fields plus the `PluginRef` tag that skills.rs/mcp.rs's own entries
/// carry, and the literal source string the user gave at install time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledAgentPlugin {
    pub name: String,
    pub source: String,
    pub plugin_ref: PluginRef,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<serde_json::Value>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub extensions: Option<serde_json::Value>,
    pub installed_at: chrono::DateTime<chrono::Utc>,
}

fn load_registry() -> Vec<InstalledAgentPlugin> {
    load_registry_from(&agent_plugins_path())
}

// Same lenient posture as skills.json/mcp_servers.json: missing or
// malformed degrades to "nothing installed", never a daemon crash.
fn load_registry_from(path: &Path) -> Vec<InstalledAgentPlugin> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn save_registry(registry: &[InstalledAgentPlugin]) -> Result<(), String> {
    save_registry_to(registry, &agent_plugins_path())
}

fn save_registry_to(registry: &[InstalledAgentPlugin], path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(registry).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

pub fn list_installed() -> Vec<InstalledAgentPlugin> {
    let mut installed = load_registry();
    installed.sort_by(|a, b| a.name.cmp(&b.name));
    installed
}

pub struct InstallResult {
    pub plugin: InstalledAgentPlugin,
    pub outcome: InstallOutcome,
}

/// Installs a single plugin from a local folder path or a git URL
/// pointing directly at the plugin's own root. A git source is cloned
/// into a throwaway temp dir and discarded after -- installing is a
/// point-in-time copy into skills.rs/mcp.rs's own stores, same as
/// marketplace.rs's plugin install, not a live link back to the source.
pub async fn install(source: &str) -> Result<InstallResult, String> {
    let source = source.trim();
    if source.is_empty() {
        return Err("'source' is required".to_string());
    }
    let is_git = source.contains("://") || source.starts_with("git@");

    if is_git {
        let tmp = kiralm_home().join("tmp").join(format!("agent-plugin-{}", uuid::Uuid::new_v4()));
        crate::git::shallow_clone(source, &tmp).await?;
        let result = install_from_dir(&tmp, source, true);
        let _ = std::fs::remove_dir_all(&tmp);
        result
    } else {
        let root = PathBuf::from(source);
        if !root.is_dir() {
            return Err(format!("'{}' is not a directory", root.display()));
        }
        install_from_dir(&root, source, false)
    }
}

fn install_from_dir(root: &Path, source: &str, is_git: bool) -> Result<InstallResult, String> {
    let raw = std::fs::read_to_string(root.join("plugin.json"))
        .map_err(|e| format!("no plugin.json at '{}': {e}", root.display()))?;
    let manifest = parse_plugin_json(&raw)?;

    let mut registry = load_registry();
    if registry.iter().any(|p| p.name == manifest.name) {
        return Err(format!("agent plugin '{}' already installed", manifest.name));
    }

    let plugin_ref = PluginRef {
        marketplace: if is_git { source.to_string() } else { "local".to_string() },
        plugin: manifest.name.clone(),
    };

    let mut outcome = InstallOutcome::default();

    let skills_root = root.join("skills");
    if let Ok(entries) = std::fs::read_dir(&skills_root) {
        for entry in entries.flatten().filter(|e| e.path().is_dir()) {
            match skills::create_from_folder(&entry.path()) {
                Ok(skill) => {
                    let _ = skills::mark_installed(&skill.name, plugin_ref.clone());
                    outcome.skills.push(skill.name);
                }
                Err(e) => outcome.warnings.push(format!("skill '{}': {e}", entry.path().display())),
            }
        }
    }

    if let Ok(raw_mcp) = std::fs::read_to_string(root.join("mcp.json")) {
        match serde_json::from_str::<McpJsonFile>(&raw_mcp) {
            Ok(parsed) => {
                for (name, entry) in parsed.mcp_servers {
                    match entry.command {
                        Some(command) => {
                            let cfg = McpServerConfig {
                                name: name.clone(),
                                command,
                                args: entry.args,
                                env: entry.env,
                                enabled: true,
                                installed_by: Some(plugin_ref.clone()),
                            };
                            match mcp::add_config(cfg) {
                                Ok(_) => outcome.mcp_servers.push(name),
                                Err(e) => outcome.warnings.push(format!("mcp server '{name}': {e}")),
                            }
                        }
                        None => {
                            let transport = entry.transport_type.unwrap_or_else(|| "http".to_string());
                            outcome.warnings.push(format!(
                                "mcp server '{name}': {transport} transport not supported (stdio only), skipped"
                            ));
                        }
                    }
                }
            }
            Err(e) => outcome.warnings.push(format!("mcp.json: {e}")),
        }
    }

    let installed = InstalledAgentPlugin {
        name: manifest.name,
        source: source.to_string(),
        plugin_ref,
        version: manifest.version,
        description: manifest.description,
        author: manifest.author,
        homepage: manifest.homepage,
        repository: manifest.repository,
        license: manifest.license,
        keywords: manifest.keywords,
        extensions: manifest.extensions,
        installed_at: chrono::Utc::now(),
    };
    registry.push(installed.clone());
    save_registry(&registry)?;

    Ok(InstallResult { plugin: installed, outcome })
}

/// Removes everything tagged with this plugin (via marketplace.rs's own
/// `uninstall_plugin`), and the registry entry itself.
pub fn uninstall(name: &str) -> Result<UninstallOutcome, String> {
    let mut registry = load_registry();
    let Some(idx) = registry.iter().position(|p| p.name == name) else {
        return Err(format!("agent plugin '{name}' not installed"));
    };
    let entry = registry.remove(idx);
    save_registry(&registry)?;
    Ok(marketplace::uninstall_plugin(&entry.plugin_ref.marketplace, &entry.plugin_ref.plugin))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
        dir: PathBuf,
    }

    impl TempHome {
        fn new() -> TempHome {
            let guard = crate::config::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("kiralm-agent-plugin-test-{}", uuid::Uuid::new_v4()));
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

    const VALID_MANIFEST: &str = r#"{
        "$schema": "https://agent-plugins.org/schema/v1.json",
        "name": "demo-plugin",
        "version": "1.0.0",
        "description": "A demo plugin."
    }"#;

    #[test]
    fn parses_valid_plugin_json() {
        let m = parse_plugin_json(VALID_MANIFEST).unwrap();
        assert_eq!(m.name, "demo-plugin");
        assert_eq!(m.version.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn rejects_missing_name() {
        let raw = r#"{"$schema":"https://agent-plugins.org/schema/v1.json"}"#;
        assert!(parse_plugin_json(raw).is_err());
    }

    #[test]
    fn rejects_missing_schema() {
        let raw = r#"{"name":"demo-plugin"}"#;
        assert!(parse_plugin_json(raw).is_err());
    }

    #[test]
    fn rejects_invalid_name_chars() {
        let raw = r#"{"$schema":"s","name":"Not Valid!"}"#;
        assert!(parse_plugin_json(raw).is_err());
    }

    #[test]
    fn tolerates_unknown_extensions_keys() {
        let raw = r#"{
            "$schema": "s",
            "name": "demo-plugin",
            "extensions": {"com.example.foo": {"anything": [1, 2, 3]}}
        }"#;
        let m = parse_plugin_json(raw).unwrap();
        assert!(m.extensions.is_some());
    }

    fn seed_plugin_dir(root: &Path, skill_name: &str, stdio_name: &str, http_name: &str) {
        std::fs::write(root.join("plugin.json"), VALID_MANIFEST).unwrap();

        let skill_dir = root.join("skills").join(skill_name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {skill_name}\ndescription: test skill.\n---\nBody\n"),
        )
        .unwrap();

        std::fs::write(
            root.join("mcp.json"),
            format!(
                r#"{{"mcpServers":{{
                    "{stdio_name}": {{"command":"echo","args":["hi"]}},
                    "{http_name}": {{"type":"http","url":"https://example.com/mcp"}}
                }}}}"#
            ),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn install_from_local_path_tags_skills_and_mcp_and_skips_http() {
        let _home = TempHome::new();
        let src = std::env::temp_dir().join(format!("kiralm-agent-plugin-src-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&src).unwrap();
        seed_plugin_dir(&src, "one", "srv", "remote-srv");

        let result = install(src.to_str().unwrap()).await.unwrap();
        assert_eq!(result.outcome.skills, vec!["one".to_string()]);
        assert_eq!(result.outcome.mcp_servers, vec!["srv".to_string()]);
        assert_eq!(result.outcome.warnings.len(), 1, "the http entry should warn, not error or vanish");
        assert!(result.outcome.warnings[0].contains("remote-srv"));
        assert!(result.outcome.warnings[0].contains("http"));

        let want_ref = PluginRef { marketplace: "local".to_string(), plugin: "demo-plugin".to_string() };
        assert_eq!(skills::get("one").unwrap().installed_by, Some(want_ref.clone()));
        assert_eq!(mcp::get_config("srv").unwrap().installed_by, Some(want_ref));
        assert!(mcp::get_config("remote-srv").is_none());

        let listed = list_installed();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "demo-plugin");

        let _ = std::fs::remove_dir_all(&src);
    }

    #[tokio::test]
    async fn uninstall_removes_exactly_the_tagged_items() {
        let _home = TempHome::new();
        let src = std::env::temp_dir().join(format!("kiralm-agent-plugin-src-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&src).unwrap();
        seed_plugin_dir(&src, "one", "srv", "remote-srv");
        install(src.to_str().unwrap()).await.unwrap();

        skills::create_from_markdown("---\nname: other\ndescription: hand-added.\n---\nBody\n").unwrap();

        let removed = uninstall("demo-plugin").unwrap();
        assert_eq!(removed.skills, vec!["one".to_string()]);
        assert_eq!(removed.mcp_servers, vec!["srv".to_string()]);
        assert!(skills::get("one").is_none());
        assert!(skills::get("other").is_some(), "unrelated skill must survive");
        assert!(list_installed().is_empty());
        assert!(uninstall("demo-plugin").is_err());

        let _ = std::fs::remove_dir_all(&src);
    }

    #[tokio::test]
    async fn install_rejects_missing_plugin_json() {
        let _home = TempHome::new();
        let src = std::env::temp_dir().join(format!("kiralm-agent-plugin-src-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&src).unwrap();
        assert!(install(src.to_str().unwrap()).await.is_err());
        let _ = std::fs::remove_dir_all(&src);
    }

    #[tokio::test]
    async fn install_rejects_duplicate_name() {
        let _home = TempHome::new();
        let src = std::env::temp_dir().join(format!("kiralm-agent-plugin-src-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&src).unwrap();
        seed_plugin_dir(&src, "one", "srv", "remote-srv");
        install(src.to_str().unwrap()).await.unwrap();
        assert!(install(src.to_str().unwrap()).await.is_err());
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn registry_round_trips_through_save_and_load() {
        let path = std::env::temp_dir().join(format!("kiralm-agent-plugin-reg-{}.json", uuid::Uuid::new_v4()));
        let entry = InstalledAgentPlugin {
            name: "demo-plugin".to_string(),
            source: "local".to_string(),
            plugin_ref: PluginRef { marketplace: "local".to_string(), plugin: "demo-plugin".to_string() },
            version: None,
            description: None,
            author: None,
            homepage: None,
            repository: None,
            license: None,
            keywords: vec![],
            extensions: None,
            installed_at: chrono::Utc::now(),
        };
        save_registry_to(&[entry], &path).unwrap();
        let loaded = load_registry_from(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "demo-plugin");
        let _ = std::fs::remove_file(&path);
    }
}
