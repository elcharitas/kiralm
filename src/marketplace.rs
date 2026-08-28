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

/// The real marketplace.json source forms: a relative path, or an
/// object naming one of git's shapes. `npm`/`archive`/`command` are
/// real kinds this adapter doesn't install, kept only so they still
/// list -- `Unsupported` carries the kind name for that error message.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PluginSource {
    Path {
        path: String,
    },
    Github {
        repo: String,
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        git_ref: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha: Option<String>,
    },
    Url {
        url: String,
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        git_ref: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha: Option<String>,
    },
    GitSubdir {
        url: String,
        path: String,
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        git_ref: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha: Option<String>,
    },
    Unsupported {
        #[serde(rename = "unsupported_kind")]
        kind: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct MarketplacePlugin {
    pub name: String,
    pub source: PluginSource,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Marketplace {
    pub slug: String,
    pub name: String,
    pub owner: Option<serde_json::Value>,
    pub plugins: Vec<MarketplacePlugin>,
    /// One plugin entry's parse failure (missing name, malformed or
    /// unrecognizable source shape) -- the rest of the list still loads.
    #[serde(default)]
    pub plugin_warnings: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawMarketplace {
    name: String,
    #[serde(default)]
    owner: Option<serde_json::Value>,
    #[serde(default)]
    plugins: Vec<serde_json::Value>,
}

fn parse_plugin_source(v: &serde_json::Value) -> Result<PluginSource, String> {
    if let Some(s) = v.as_str() {
        return Ok(PluginSource::Path { path: s.to_string() });
    }
    let Some(obj) = v.as_object() else {
        return Err("'source' must be a string or an object".to_string());
    };
    let str_field = |key: &str| obj.get(key).and_then(|f| f.as_str()).map(str::to_string);
    let kind = str_field("source").ok_or_else(|| "object source missing its own 'source' kind".to_string())?;
    match kind.as_str() {
        "github" => Ok(PluginSource::Github {
            repo: str_field("repo").ok_or_else(|| "github source missing 'repo'".to_string())?,
            git_ref: str_field("ref"),
            sha: str_field("sha"),
        }),
        "url" => Ok(PluginSource::Url {
            url: str_field("url").ok_or_else(|| "url source missing 'url'".to_string())?,
            git_ref: str_field("ref"),
            sha: str_field("sha"),
        }),
        "git-subdir" => Ok(PluginSource::GitSubdir {
            url: str_field("url").ok_or_else(|| "git-subdir source missing 'url'".to_string())?,
            path: str_field("path").ok_or_else(|| "git-subdir source missing 'path'".to_string())?,
            git_ref: str_field("ref"),
            sha: str_field("sha"),
        }),
        other => Ok(PluginSource::Unsupported { kind: other.to_string() }),
    }
}

fn parse_plugin_entry(v: &serde_json::Value) -> Result<MarketplacePlugin, String> {
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "plugin entry missing 'name'".to_string())?;
    let source_val = v.get("source").ok_or_else(|| format!("plugin '{name}': missing 'source'"))?;
    let source = parse_plugin_source(source_val).map_err(|e| format!("plugin '{name}': {e}"))?;
    let description = v.get("description").and_then(|d| d.as_str()).map(str::to_string);
    Ok(MarketplacePlugin { name: name.to_string(), source, description })
}

/// Per-entry resilient: one bad plugin entry becomes a warning, not a
/// failure of the whole array -- mirrors skills.rs/agent_plugin.rs's
/// skip-and-warn posture for content it can't parse or doesn't support.
fn parse_plugin_entries(raw: &[serde_json::Value]) -> (Vec<MarketplacePlugin>, Vec<String>) {
    let mut plugins = Vec::new();
    let mut warnings = Vec::new();
    for v in raw {
        match parse_plugin_entry(v) {
            Ok(p) => plugins.push(p),
            Err(e) => warnings.push(e),
        }
    }
    (plugins, warnings)
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

struct ParsedMarketplace {
    name: String,
    owner: Option<serde_json::Value>,
    plugins: Vec<MarketplacePlugin>,
    plugin_warnings: Vec<String>,
}

fn parse_marketplace_json(raw: &str) -> Result<ParsedMarketplace, String> {
    let parsed: RawMarketplace =
        serde_json::from_str(raw).map_err(|e| format!("invalid marketplace.json: {e}"))?;
    if parsed.name.trim().is_empty() {
        return Err("marketplace.json 'name' is required".to_string());
    }
    let (plugins, plugin_warnings) = parse_plugin_entries(&parsed.plugins);
    Ok(ParsedMarketplace { name: parsed.name, owner: parsed.owner, plugins, plugin_warnings })
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
        plugin_warnings: parsed.plugin_warnings,
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

fn plugin_root(marketplace_dir: &Path, path: &str) -> PathBuf {
    marketplace_dir.join(path.trim_start_matches("./"))
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

/// Real marketplace.json entries sometimes give `git-subdir`'s `url` as
/// bare `owner/repo` (same shorthand `github`'s `repo` field uses),
/// not always a full clone URL as its own name implies.
fn expand_repo_shorthand(url: &str) -> String {
    if url.contains("://") || url.starts_with("git@") {
        url.to_string()
    } else {
        format!("https://github.com/{url}.git")
    }
}

/// Resolves a plugin's content root for install: a relative path stays
/// under the marketplace's own clone; `github`/`url`/`git-subdir` each
/// clone their own throwaway checkout, cleaned up by the caller once
/// skills/mcp servers have been copied out of it.
async fn resolve_plugin_root(
    marketplace_slug: &str,
    plugin_name: &str,
    source: &PluginSource,
) -> Result<(PathBuf, Option<PathBuf>), String> {
    match source {
        PluginSource::Path { path } => {
            let root = plugin_root(&marketplaces_dir().join(marketplace_slug), path);
            if !root.is_dir() {
                return Err(format!("plugin '{plugin_name}' source '{path}' not found"));
            }
            Ok((root, None))
        }
        PluginSource::Github { repo, git_ref, sha } => {
            let url = expand_repo_shorthand(repo);
            let tmp = kiralm_home().join("tmp").join(format!("plugin-{}", uuid::Uuid::new_v4()));
            crate::git::clone_at(&url, &tmp, git_ref.as_deref(), sha.as_deref()).await?;
            Ok((tmp.clone(), Some(tmp)))
        }
        PluginSource::Url { url, git_ref, sha } => {
            let url = expand_repo_shorthand(url);
            let tmp = kiralm_home().join("tmp").join(format!("plugin-{}", uuid::Uuid::new_v4()));
            crate::git::clone_at(&url, &tmp, git_ref.as_deref(), sha.as_deref()).await?;
            Ok((tmp.clone(), Some(tmp)))
        }
        PluginSource::GitSubdir { url, path, git_ref, sha } => {
            let url = expand_repo_shorthand(url);
            let tmp = kiralm_home().join("tmp").join(format!("plugin-{}", uuid::Uuid::new_v4()));
            crate::git::clone_at(&url, &tmp, git_ref.as_deref(), sha.as_deref()).await?;
            let root = tmp.join(path);
            if !root.is_dir() {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(format!("plugin '{plugin_name}' git-subdir path '{path}' not found in clone"));
            }
            Ok((root, Some(tmp)))
        }
        PluginSource::Unsupported { kind } => {
            Err(format!("plugin '{plugin_name}' source kind '{kind}' not supported yet"))
        }
    }
}

/// Copies a plugin's `skills/*/SKILL.md` (+ siblings) into skills.rs's
/// store and its `.mcp.json` servers into mcp.rs's, tagging both with
/// this plugin. `commands/`, `agents/`, `hooks/` are out of scope and
/// simply ignored, not errored.
pub async fn install_plugin(marketplace_slug: &str, plugin_name: &str) -> Result<InstallOutcome, String> {
    let marketplace = load(marketplace_slug)?;
    let plugin = marketplace
        .plugins
        .iter()
        .find(|p| p.name == plugin_name)
        .ok_or_else(|| format!("plugin '{plugin_name}' not found in marketplace '{marketplace_slug}'"))?;
    let (root, tmp_clone) = resolve_plugin_root(marketplace_slug, plugin_name, &plugin.source).await?;
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

    if let Some(tmp) = tmp_clone {
        let _ = std::fs::remove_dir_all(&tmp);
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

    #[test]
    fn parses_github_url_and_git_subdir_object_sources() {
        let raw = r#"{"name":"demo","plugins":[
            {"name":"a","source":{"source":"github","repo":"owner/repo","ref":"v2.0.0"}},
            {"name":"b","source":{"source":"url","url":"https://example.com/x.git","sha":"deadbeef"}},
            {"name":"c","source":{"source":"git-subdir","url":"https://example.com/mono.git","path":"tools/p"}}
        ]}"#;
        let parsed = parse_marketplace_json(raw).unwrap();
        assert!(parsed.plugin_warnings.is_empty());
        assert_eq!(parsed.plugins.len(), 3);

        match &parsed.plugins[0].source {
            PluginSource::Github { repo, git_ref, sha } => {
                assert_eq!(repo, "owner/repo");
                assert_eq!(git_ref.as_deref(), Some("v2.0.0"));
                assert!(sha.is_none());
            }
            other => panic!("expected Github, got {other:?}"),
        }
        match &parsed.plugins[1].source {
            PluginSource::Url { url, sha, .. } => {
                assert_eq!(url, "https://example.com/x.git");
                assert_eq!(sha.as_deref(), Some("deadbeef"));
            }
            other => panic!("expected Url, got {other:?}"),
        }
        match &parsed.plugins[2].source {
            PluginSource::GitSubdir { url, path, .. } => {
                assert_eq!(url, "https://example.com/mono.git");
                assert_eq!(path, "tools/p");
            }
            other => panic!("expected GitSubdir, got {other:?}"),
        }
    }

    #[test]
    fn a_mixed_plugin_array_keeps_the_supported_and_warns_on_the_rest() {
        let raw = r#"{"name":"demo","plugins":[
            {"name":"good","source":"./plugins/good"},
            {"name":"bad-source-shape","source":123},
            {"source":"./plugins/no-name"},
            {"name":"npm-kind","source":{"source":"npm","package":"whatever"}}
        ]}"#;
        let parsed = parse_marketplace_json(raw).unwrap();
        assert_eq!(parsed.plugin_warnings.len(), 2, "the two truly malformed entries should warn");
        assert_eq!(parsed.plugins.len(), 2, "good + npm-kind still list, npm just isn't installable");
        assert_eq!(parsed.plugins[0].name, "good");
        assert_eq!(parsed.plugins[1].name, "npm-kind");
        match &parsed.plugins[1].source {
            PluginSource::Unsupported { kind } => assert_eq!(kind, "npm"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
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

    #[tokio::test]
    async fn install_writes_into_real_skills_and_mcp_stores_with_tagging() {
        let _home = TempHome::new();
        let slug = "demo-mp";
        let dir = marketplaces_dir().join(slug);
        seed_plugin(&dir, "plugin-a", "one", "srv");

        let outcome = install_plugin(slug, "plugin-a").await.unwrap();
        assert_eq!(outcome.skills, vec!["one".to_string()]);
        assert_eq!(outcome.mcp_servers, vec!["srv".to_string()]);
        assert!(outcome.warnings.is_empty());

        let want_ref = PluginRef { marketplace: slug.to_string(), plugin: "plugin-a".to_string() };
        assert_eq!(skills::get("one").unwrap().installed_by, Some(want_ref.clone()));
        assert_eq!(mcp::get_config("srv").unwrap().installed_by, Some(want_ref));
        assert!(is_installed(slug, "plugin-a"));
    }

    #[tokio::test]
    async fn uninstall_removes_exactly_the_tagged_items_and_nothing_else() {
        let _home = TempHome::new();
        let slug = "demo-mp";
        let dir = marketplaces_dir().join(slug);
        seed_plugin(&dir, "plugin-a", "one", "srv");
        install_plugin(slug, "plugin-a").await.unwrap();

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

    /// A real local git repo with a plugin at its root (or under
    /// `subdir`, for the git-subdir case), for install to actually clone.
    fn init_local_git_plugin_repo(skill_name: &str, mcp_name: &str, subdir: Option<&str>) -> PathBuf {
        let src = std::env::temp_dir().join(format!("kiralm-marketplace-git-src-{}", uuid::Uuid::new_v4()));
        let plugin_root = match subdir {
            Some(sub) => src.join(sub),
            None => src.clone(),
        };
        let skill_dir = plugin_root.join("skills").join(skill_name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {skill_name}\ndescription: test skill.\n---\nBody\n"),
        )
        .unwrap();
        std::fs::write(
            plugin_root.join(".mcp.json"),
            format!(r#"{{"mcpServers":{{"{mcp_name}":{{"command":"echo","args":["hi"]}}}}}}"#),
        )
        .unwrap();

        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(&src).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q"]);
        run_git(&["-c", "user.email=test@test.com", "-c", "user.name=test", "add", "-A"]);
        run_git(&["-c", "user.email=test@test.com", "-c", "user.name=test", "commit", "-q", "-m", "init"]);
        src
    }

    fn write_marketplace_with_source(slug: &str, plugin_name: &str, source_json: &str) {
        let dir = marketplaces_dir().join(slug);
        std::fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
        std::fs::write(
            dir.join(".claude-plugin").join("marketplace.json"),
            format!(r#"{{"name":"Demo","plugins":[{{"name":"{plugin_name}","source":{source_json}}}]}}"#),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn install_routes_a_url_source_through_a_real_clone() {
        let _home = TempHome::new();
        let src = init_local_git_plugin_repo("one", "srv", None);
        write_marketplace_with_source(
            "url-mp",
            "plugin-a",
            &format!(r#"{{"source":"url","url":"file://{}"}}"#, src.display()),
        );

        let outcome = install_plugin("url-mp", "plugin-a").await.unwrap();
        assert_eq!(outcome.skills, vec!["one".to_string()]);
        assert_eq!(outcome.mcp_servers, vec!["srv".to_string()]);
        assert!(skills::get("one").is_some());

        let _ = std::fs::remove_dir_all(&src);
    }

    #[tokio::test]
    async fn install_routes_a_git_subdir_source_through_a_real_clone() {
        let _home = TempHome::new();
        let src = init_local_git_plugin_repo("two", "srv2", Some("tools/plugin"));
        write_marketplace_with_source(
            "subdir-mp",
            "plugin-b",
            &format!(r#"{{"source":"git-subdir","url":"file://{}","path":"tools/plugin"}}"#, src.display()),
        );

        let outcome = install_plugin("subdir-mp", "plugin-b").await.unwrap();
        assert_eq!(outcome.skills, vec!["two".to_string()]);
        assert_eq!(outcome.mcp_servers, vec!["srv2".to_string()]);

        let _ = std::fs::remove_dir_all(&src);
    }

    #[tokio::test]
    async fn install_of_an_unsupported_source_kind_errors_clearly() {
        let _home = TempHome::new();
        write_marketplace_with_source("npm-mp", "plugin-c", r#"{"source":"npm","package":"whatever"}"#);

        let err = install_plugin("npm-mp", "plugin-c").await.unwrap_err();
        assert!(err.contains("npm"), "{err}");
        assert!(err.contains("not supported"), "{err}");
    }
}
