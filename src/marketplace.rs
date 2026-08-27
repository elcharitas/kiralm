//! Claude Code plugin marketplaces: git repos with a
//! `.claude-plugin/marketplace.json`, cloned under
//! `~/.kiralm/marketplaces/<slug>/`. Installing a plugin is a
//! point-in-time copy of its `skills/` and `.mcp.json` into
//! skills.rs's and mcp.rs's own stores, tagged with `PluginRef` so
//! uninstall removes exactly what it added.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::kiralm_home;
use crate::mcp::{self, McpServerConfig};
use crate::skills;

pub fn marketplaces_dir() -> PathBuf {
    kiralm_home().join("marketplaces")
}

/// Which marketplace plugin installed a skill or mcp server -- lives as
/// an optional field on those structs themselves rather than a
/// separate tracking table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginRef {
    pub marketplace: String,
    pub plugin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplacePlugin {
    pub name: String,
    /// A relative path within the marketplace repo. A separate git url
    /// here (the other form the real format allows) isn't fetched --
    /// install reports a clear error for it instead of guessing.
    pub source: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Marketplace {
    pub slug: String,
    pub name: String,
    pub owner: Option<serde_json::Value>,
    pub plugins: Vec<MarketplacePlugin>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawMarketplace {
    name: String,
    #[serde(default)]
    owner: Option<serde_json::Value>,
    #[serde(default)]
    plugins: Vec<MarketplacePlugin>,
}

fn sanitize_slug(raw: &str) -> String {
    let s: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    s.trim_matches('-').to_string()
}

/// The repo name from a git URL (`https://.../owner/repo(.git)` or the
/// scp-like `git@host:owner/repo.git`), used as the marketplace's slug.
/// `pub(crate)` so plugin_catalog.rs can derive the same slug without
/// re-adding the marketplace, for installed-state checks.
pub(crate) fn slug_from_url(url: &str) -> Result<String, String> {
    let trimmed = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or("");
    let slug = sanitize_slug(last);
    if slug.is_empty() {
        return Err(format!("can't derive a marketplace name from '{url}'"));
    }
    Ok(slug)
}

fn marketplace_json_path(dir: &Path) -> PathBuf {
    dir.join(".claude-plugin").join("marketplace.json")
}

fn parse_marketplace_json(raw: &str) -> Result<RawMarketplace, String> {
    let parsed: RawMarketplace =
        serde_json::from_str(raw).map_err(|e| format!("invalid marketplace.json: {e}"))?;
    if parsed.name.trim().is_empty() {
        return Err("marketplace.json 'name' is required".to_string());
    }
    Ok(parsed)
}

fn load(slug: &str) -> Result<Marketplace, String> {
    let dir = marketplaces_dir().join(slug);
    let raw = std::fs::read_to_string(marketplace_json_path(&dir))
        .map_err(|e| format!("no marketplace.json for '{slug}': {e}"))?;
    let parsed = parse_marketplace_json(&raw)?;
    Ok(Marketplace {
        slug: slug.to_string(),
        name: parsed.name,
        owner: parsed.owner,
        plugins: parsed.plugins,
    })
}

/// Every marketplace already cloned locally. A dir whose
/// marketplace.json is missing or malformed is silently skipped, same
/// lenient posture as skills::list().
pub fn list() -> Vec<Marketplace> {
    let mut out: Vec<Marketplace> = match std::fs::read_dir(marketplaces_dir()) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|slug| load(&slug).ok())
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    out
}

pub fn get(slug: &str) -> Result<Marketplace, String> {
    load(slug)
}

/// Shallow-clones a not-yet-known marketplace, or fetches+fast-forwards
/// one already on disk. A slug collision between two different repos
/// (same repo name, different owner) isn't detected.
pub async fn add(url: &str) -> Result<Marketplace, String> {
    let slug = slug_from_url(url)?;
    let dir = marketplaces_dir().join(&slug);
    if dir.is_dir() {
        crate::git::pull(&dir).await?;
    } else {
        crate::git::shallow_clone(url, &dir).await?;
    }
    load(&slug)
}

/// Re-fetches a known marketplace's repo and re-parses its
/// marketplace.json. Never touches already-installed plugins' copied
/// content -- installing is a point-in-time snapshot, not a live link.
pub async fn refresh(slug: &str) -> Result<Marketplace, String> {
    let dir = marketplaces_dir().join(slug);
    if !dir.is_dir() {
        return Err(format!("marketplace '{slug}' not found"));
    }
    crate::git::pull(&dir).await?;
    load(slug)
}

/// Deletes the local clone only. Already-installed plugins' skills/mcp
/// servers stay installed -- they were copied out at install time.
pub fn remove(slug: &str) -> Result<(), String> {
    let dir = marketplaces_dir().join(slug);
    if !dir.is_dir() {
        return Err(format!("marketplace '{slug}' not found"));
    }
    std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())
}

/// Whether a marketplace's plugin has anything tagged as installed
/// right now, for the plugin list's install/uninstall button state.
pub fn is_installed(marketplace: &str, plugin: &str) -> bool {
    let tagged = |m: &str, p: &str| m == marketplace && p == plugin;
    skills::list()
        .into_iter()
        .any(|s| s.installed_by.is_some_and(|r| tagged(&r.marketplace, &r.plugin)))
        || mcp::load_configs()
            .into_iter()
            .any(|c| c.installed_by.is_some_and(|r| tagged(&r.marketplace, &r.plugin)))
}

fn plugin_root(marketplace_dir: &Path, plugin: &MarketplacePlugin) -> PathBuf {
    marketplace_dir.join(plugin.source.trim_start_matches("./"))
}

#[derive(Debug, Deserialize)]
struct McpJsonFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: HashMap<String, McpJsonServer>,
}

#[derive(Debug, Deserialize)]
struct McpJsonServer {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Default, Serialize)]
pub struct InstallOutcome {
    pub skills: Vec<String>,
    pub mcp_servers: Vec<String>,
    /// Non-fatal per-item failures (a duplicate name, a malformed
    /// SKILL.md) -- the rest of the plugin still installs.
    pub warnings: Vec<String>,
}

/// Copies a plugin's `skills/*/SKILL.md` (+ siblings) into skills.rs's
/// store and its `.mcp.json` servers into mcp.rs's, tagging both with
/// this plugin. `commands/`, `agents/`, `hooks/` are out of scope and
/// simply ignored, not errored.
pub fn install_plugin(marketplace_slug: &str, plugin_name: &str) -> Result<InstallOutcome, String> {
    let marketplace = load(marketplace_slug)?;
    let plugin = marketplace
        .plugins
        .iter()
        .find(|p| p.name == plugin_name)
        .ok_or_else(|| format!("plugin '{plugin_name}' not found in marketplace '{marketplace_slug}'"))?;
    if plugin.source.contains("://") || plugin.source.starts_with("git@") {
        return Err(format!("plugin '{plugin_name}' source is a separate git url, not supported"));
    }
    let root = plugin_root(&marketplaces_dir().join(marketplace_slug), plugin);
    if !root.is_dir() {
        return Err(format!("plugin '{plugin_name}' source '{}' not found", plugin.source));
    }
    let plugin_ref = PluginRef { marketplace: marketplace_slug.to_string(), plugin: plugin_name.to_string() };

    let mut outcome = InstallOutcome::default();

    let skills_dir = root.join("skills");
    if let Ok(entries) = std::fs::read_dir(&skills_dir) {
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

    if let Ok(raw) = std::fs::read_to_string(root.join(".mcp.json")) {
        match serde_json::from_str::<McpJsonFile>(&raw) {
            Ok(parsed) => {
                for (name, server) in parsed.mcp_servers {
                    let cfg = McpServerConfig {
                        name: name.clone(),
                        command: server.command,
                        args: server.args,
                        env: server.env,
                        enabled: true,
                        installed_by: Some(plugin_ref.clone()),
                    };
                    match mcp::add_config(cfg) {
                        Ok(_) => outcome.mcp_servers.push(name),
                        Err(e) => outcome.warnings.push(format!("mcp server '{name}': {e}")),
                    }
                }
            }
            Err(e) => outcome.warnings.push(format!(".mcp.json: {e}")),
        }
    }

    if outcome.skills.is_empty() && outcome.mcp_servers.is_empty() && !outcome.warnings.is_empty() {
        return Err(outcome.warnings.join("; "));
    }
    Ok(outcome)
}

#[derive(Debug, Default, Serialize)]
pub struct UninstallOutcome {
    pub skills: Vec<String>,
    pub mcp_servers: Vec<String>,
}

/// Removes everything tagged with this plugin, and nothing else.
/// Doesn't require the marketplace or plugin to still exist -- the
/// tag on each item is the only source of truth.
pub fn uninstall_plugin(marketplace_slug: &str, plugin_name: &str) -> UninstallOutcome {
    UninstallOutcome {
        skills: skills::delete_by_plugin(marketplace_slug, plugin_name),
        mcp_servers: mcp::delete_by_plugin(marketplace_slug, plugin_name),
    }
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
            let dir = std::env::temp_dir().join(format!("kiralm-marketplace-test-{}", uuid::Uuid::new_v4()));
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
    fn parses_valid_marketplace_json() {
        let raw = r#"{"name":"demo","owner":{"name":"me"},"plugins":[{"name":"a","source":"./plugins/a","description":"d"}]}"#;
        let parsed = parse_marketplace_json(raw).unwrap();
        assert_eq!(parsed.name, "demo");
        assert_eq!(parsed.plugins.len(), 1);
        assert_eq!(parsed.plugins[0].name, "a");
    }

    #[test]
    fn rejects_malformed_marketplace_json() {
        assert!(parse_marketplace_json("not json at all").is_err());
        assert!(parse_marketplace_json(r#"{"owner":{}}"#).is_err(), "missing required name");
        assert!(parse_marketplace_json(r#"{"name":""}"#).is_err(), "blank name");
    }

    fn seed_plugin(marketplace_dir: &Path, plugin_name: &str, skill_name: &str, mcp_name: &str) {
        std::fs::create_dir_all(marketplace_dir.join(".claude-plugin")).unwrap();
        std::fs::write(
            marketplace_dir.join(".claude-plugin").join("marketplace.json"),
            format!(r#"{{"name":"Demo","plugins":[{{"name":"{plugin_name}","source":"./{plugin_name}"}}]}}"#),
        )
        .unwrap();

        let skill_dir = marketplace_dir.join(plugin_name).join("skills").join(skill_name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {skill_name}\ndescription: test skill.\n---\nBody\n"),
        )
        .unwrap();

        std::fs::write(
            marketplace_dir.join(plugin_name).join(".mcp.json"),
            format!(r#"{{"mcpServers":{{"{mcp_name}":{{"command":"echo","args":["hi"]}}}}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn install_writes_into_real_skills_and_mcp_stores_with_tagging() {
        let _home = TempHome::new();
        let slug = "demo-mp";
        let dir = marketplaces_dir().join(slug);
        seed_plugin(&dir, "plugin-a", "one", "srv");

        let outcome = install_plugin(slug, "plugin-a").unwrap();
        assert_eq!(outcome.skills, vec!["one".to_string()]);
        assert_eq!(outcome.mcp_servers, vec!["srv".to_string()]);
        assert!(outcome.warnings.is_empty());

        let want_ref = PluginRef { marketplace: slug.to_string(), plugin: "plugin-a".to_string() };
        assert_eq!(skills::get("one").unwrap().installed_by, Some(want_ref.clone()));
        assert_eq!(mcp::get_config("srv").unwrap().installed_by, Some(want_ref));
        assert!(is_installed(slug, "plugin-a"));
    }

    #[test]
    fn uninstall_removes_exactly_the_tagged_items_and_nothing_else() {
        let _home = TempHome::new();
        let slug = "demo-mp";
        let dir = marketplaces_dir().join(slug);
        seed_plugin(&dir, "plugin-a", "one", "srv");
        install_plugin(slug, "plugin-a").unwrap();

        skills::create_from_markdown("---\nname: other\ndescription: hand-added.\n---\nBody\n").unwrap();
        mcp::add_config(McpServerConfig {
            name: "other-server".to_string(),
            command: "echo".to_string(),
            args: vec![],
            env: HashMap::new(),
            enabled: true,
            installed_by: None,
        })
        .unwrap();

        let removed = uninstall_plugin(slug, "plugin-a");
        assert_eq!(removed.skills, vec!["one".to_string()]);
        assert_eq!(removed.mcp_servers, vec!["srv".to_string()]);

        assert!(skills::get("one").is_none());
        assert!(mcp::get_config("srv").is_none());
        assert!(skills::get("other").is_some(), "unrelated skill must survive");
        assert!(mcp::get_config("other-server").is_some(), "unrelated server must survive");
        assert!(!is_installed(slug, "plugin-a"));
    }

    #[test]
    fn slug_from_url_handles_https_and_scp_forms() {
        assert_eq!(slug_from_url("https://github.com/org/repo").unwrap(), "repo");
        assert_eq!(slug_from_url("https://github.com/org/repo.git").unwrap(), "repo");
        assert_eq!(slug_from_url("git@github.com:org/repo.git").unwrap(), "repo");
    }
}
