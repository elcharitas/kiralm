//! kiralm-web's plugin catalog: fetch/cache `GET /api/plugins`, and the
//! install/uninstall dispatch that unifies marketplace.rs, agent_plugin.rs
//! and dsh.rs behind one browsable list. Same fetch/cache shape as
//! cloud.rs's `sync_cloud_models` -- see that module's doc comment.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::config::kiralm_home;
use crate::{agent_plugin, dsh, marketplace};

/// The single `dsh` profile every catalog-sourced plugin installs into.
/// Profiles aren't plugins to browse, so a dedicated hand-managed one
/// would be overkill -- this one is created lazily on first install.
const DSH_DEFAULT_PROFILE: &str = "default";

/// `GET /api/plugins`'s per-row shape, verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudPlugin {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub source_kind: String,
    pub source: String,
    pub plugin_name: Option<String>,
    pub enabled: bool,
    pub sort_order: i64,
}

#[derive(Debug, Deserialize)]
struct CloudPluginsResponse {
    data: Vec<CloudPlugin>,
}

fn cache_path() -> PathBuf {
    kiralm_home().join("plugin_catalog_cache.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedCatalog {
    fetched_at: i64,
    plugins: Vec<CloudPlugin>,
}

fn load_cache() -> Option<CachedCatalog> {
    let raw = std::fs::read_to_string(cache_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_cache(plugins: &[CloudPlugin]) -> anyhow::Result<()> {
    let cache = CachedCatalog {
        fetched_at: chrono::Utc::now().timestamp(),
        plugins: plugins.to_vec(),
    };
    let path = cache_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&cache)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The cached rows as of the last successful fetch, empty if there's
/// never been one -- never triggers a network call itself.
pub fn cached_plugins() -> Vec<CloudPlugin> {
    load_cache().map(|c| c.plugins).unwrap_or_default()
}

static LAST_FETCH_FAILED: AtomicBool = AtomicBool::new(false);
static SYNC_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Fetches `GET {api_base_url}/api/plugins` and caches it to disk. Never
/// blocks or breaks anything on a network failure -- same posture as
/// `cloud::sync_cloud_models`. Meant to be run inside `tokio::spawn`.
pub async fn sync_cloud_plugins(api_base_url: &str) {
    if SYNC_IN_FLIGHT.swap(true, Ordering::Relaxed) {
        return;
    }
    sync_cloud_plugins_inner(api_base_url).await;
    SYNC_IN_FLIGHT.store(false, Ordering::Relaxed);
}

async fn sync_cloud_plugins_inner(api_base_url: &str) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("plugin catalog sync: failed to build http client: {e}");
            return;
        }
    };

    let url = format!("{}/api/plugins", api_base_url.trim_end_matches('/'));
    let result = async {
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("GET /api/plugins returned {}", resp.status());
        }
        let parsed: CloudPluginsResponse = resp.json().await?;
        Ok::<_, anyhow::Error>(parsed.data)
    }
    .await;

    match result {
        Ok(plugins) => {
            if LAST_FETCH_FAILED.swap(false, Ordering::Relaxed) {
                tracing::warn!("plugin catalog sync recovered");
            }
            if let Err(e) = save_cache(&plugins) {
                tracing::debug!("plugin catalog sync: failed to write cache: {e}");
            }
        }
        Err(e) => {
            if !LAST_FETCH_FAILED.swap(true, Ordering::Relaxed) {
                tracing::warn!("plugin catalog sync: backend unreachable: {e}");
            } else {
                tracing::debug!("plugin catalog sync: backend unreachable: {e}");
            }
            // Keep serving whatever's already cached, same as the model
            // catalog sync.
        }
    }
}

/// Re-derives the same identifiers the three adapters already use and
/// checks their real live state, rather than a separate persisted
/// install-registry that could drift from what's actually on disk.
pub fn is_installed(row: &CloudPlugin) -> bool {
    match row.source_kind.as_str() {
        "claude_marketplace" => {
            let Some(plugin_name) = &row.plugin_name else { return false };
            let Ok(slug) = marketplace::slug_from_url(&row.source) else { return false };
            marketplace::get(&slug).is_ok() && marketplace::is_installed(&slug, plugin_name)
        }
        // agent_plugin's own registry records the literal `source` string
        // given at install time -- the only stable handle before install,
        // since the plugin's real name only exists inside its manifest.
        "agent_plugin" => agent_plugin::list_installed().iter().any(|p| p.source == row.source),
        "dsh" => dsh::list_plugins(DSH_DEFAULT_PROFILE)
            .unwrap_or_default()
            .contains(&row.source),
        _ => false,
    }
}

fn ensure_default_dsh_profile() -> Result<(), String> {
    if dsh::list_profiles().iter().any(|p| p == DSH_DEFAULT_PROFILE) {
        return Ok(());
    }
    dsh::create_profile(DSH_DEFAULT_PROFILE)
}

/// Outcome of dispatching an install by `source_kind`, carrying enough
/// per-kind detail for the API layer to wire live mcp connections and
/// serialize a real response, not just an ack.
pub enum CatalogInstallOutcome {
    Marketplace(marketplace::InstallOutcome),
    AgentPlugin(agent_plugin::InstallResult),
    Dsh(String),
}

/// `POST /api/plugins/catalog/:id/install`'s dispatch: adds the
/// marketplace / installs the agent plugin / adds the dsh package,
/// using the source baked into the catalog row -- no user-typed URL,
/// path, or package name anywhere in this path.
pub async fn install(dsh_bin: &str, row: &CloudPlugin) -> Result<CatalogInstallOutcome, String> {
    match row.source_kind.as_str() {
        "claude_marketplace" => {
            let plugin_name = row
                .plugin_name
                .as_deref()
                .ok_or_else(|| "catalog row missing plugin_name for claude_marketplace source".to_string())?;
            let added = marketplace::add(&row.source).await?;
            let outcome = marketplace::install_plugin(&added.slug, plugin_name)?;
            Ok(CatalogInstallOutcome::Marketplace(outcome))
        }
        "agent_plugin" => {
            let result = agent_plugin::install(&row.source).await?;
            Ok(CatalogInstallOutcome::AgentPlugin(result))
        }
        "dsh" => {
            ensure_default_dsh_profile()?;
            let output = dsh::install_plugin(dsh_bin, DSH_DEFAULT_PROFILE, &row.source).await?;
            Ok(CatalogInstallOutcome::Dsh(output))
        }
        other => Err(format!("unknown source_kind '{other}'")),
    }
}

/// Both `claude_marketplace` and `agent_plugin` removals resolve to the
/// same skills/mcp-servers-removed shape, marketplace.rs's `UninstallOutcome`
/// -- `Registry` covers either.
pub enum CatalogUninstallOutcome {
    Registry(marketplace::UninstallOutcome),
    Dsh(String),
}

/// `DELETE /api/plugins/catalog/:id`'s dispatch: re-derives the same
/// identifiers `install` used and calls the matching uninstall.
pub async fn uninstall(dsh_bin: &str, row: &CloudPlugin) -> Result<CatalogUninstallOutcome, String> {
    match row.source_kind.as_str() {
        "claude_marketplace" => {
            let plugin_name = row
                .plugin_name
                .as_deref()
                .ok_or_else(|| "catalog row missing plugin_name for claude_marketplace source".to_string())?;
            let slug = marketplace::slug_from_url(&row.source)?;
            Ok(CatalogUninstallOutcome::Registry(marketplace::uninstall_plugin(&slug, plugin_name)))
        }
        "agent_plugin" => {
            let installed_name = agent_plugin::list_installed()
                .into_iter()
                .find(|p| p.source == row.source)
                .map(|p| p.name)
                .ok_or_else(|| format!("agent plugin from '{}' not installed", row.source))?;
            Ok(CatalogUninstallOutcome::Registry(agent_plugin::uninstall(&installed_name)?))
        }
        "dsh" => {
            let output = dsh::remove_plugin(dsh_bin, DSH_DEFAULT_PROFILE, &row.source).await?;
            Ok(CatalogUninstallOutcome::Dsh(output))
        }
        other => Err(format!("unknown source_kind '{other}'")),
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
            let dir = std::env::temp_dir().join(format!("kiralm-plugin-catalog-test-{}", uuid::Uuid::new_v4()));
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

    fn row(id: &str, kind: &str, source: &str, plugin_name: Option<&str>) -> CloudPlugin {
        CloudPlugin {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            source_kind: kind.to_string(),
            source: source.to_string(),
            plugin_name: plugin_name.map(|s| s.to_string()),
            enabled: true,
            sort_order: 0,
        }
    }

    #[test]
    fn cache_write_read_round_trips() {
        let _home = TempHome::new();
        assert!(cached_plugins().is_empty());

        let plugins = vec![row("p1", "dsh", "some-pkg", None)];
        save_cache(&plugins).unwrap();

        let loaded = cached_plugins();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "p1");
        assert_eq!(loaded[0].source, "some-pkg");
    }

    #[tokio::test]
    async fn network_failure_never_crashes_and_keeps_prior_cache() {
        let _home = TempHome::new();
        save_cache(&[row("p1", "dsh", "some-pkg", None)]).unwrap();

        // Nothing listens on this port -- a real, immediate connection
        // failure, not a mock.
        sync_cloud_plugins("http://127.0.0.1:1").await;

        // The stale cache survives a failed sync untouched.
        let loaded = cached_plugins();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "p1");
    }

    #[test]
    fn deserializes_get_plugins_shape() {
        let raw = r#"{"data":[{"id":"a","name":"A","description":null,
            "source_kind":"claude_marketplace","source":"https://github.com/org/repo",
            "plugin_name":"plug","enabled":true,"sort_order":0}]}"#;
        let parsed: CloudPluginsResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.data.len(), 1);
        assert_eq!(parsed.data[0].plugin_name.as_deref(), Some("plug"));
    }

    #[test]
    fn claude_marketplace_installed_state_matches_real_marketplace_and_skill() {
        let _home = TempHome::new();
        let slug = "repo";
        let dir = marketplace::marketplaces_dir().join(slug);
        std::fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
        std::fs::write(
            dir.join(".claude-plugin").join("marketplace.json"),
            r#"{"name":"Demo","plugins":[{"name":"plug","source":"./plug"}]}"#,
        )
        .unwrap();
        let skill_dir = dir.join("plug").join("skills").join("one");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: one\ndescription: test skill.\n---\nBody\n",
        )
        .unwrap();

        let r = row("c1", "claude_marketplace", "https://github.com/org/repo", Some("plug"));
        assert!(!is_installed(&r), "not installed until install_plugin runs");

        marketplace::install_plugin(slug, "plug").unwrap();
        assert!(is_installed(&r));
    }

    #[tokio::test]
    async fn agent_plugin_installed_state_matches_by_literal_source() {
        let _home = TempHome::new();
        let src = std::env::temp_dir().join(format!("kiralm-catalog-agent-plugin-src-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("plugin.json"),
            r#"{"$schema":"https://agent-plugins.org/schema/v1.json","name":"demo-plugin"}"#,
        )
        .unwrap();

        let r = row("a1", "agent_plugin", src.to_str().unwrap(), None);
        assert!(!is_installed(&r));

        agent_plugin::install(src.to_str().unwrap()).await.unwrap();
        assert!(is_installed(&r));

        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn dsh_installed_state_checks_default_profile_dependencies() {
        let _home = TempHome::new();
        let r = row("d1", "dsh", "some-npm-pkg", None);
        assert!(!is_installed(&r), "no default profile yet -> not installed");

        ensure_default_dsh_profile().unwrap();
        assert!(!is_installed(&r), "profile exists but package not added yet");
    }

    #[tokio::test]
    async fn install_dispatch_routes_dsh_kind_to_default_profile() {
        let _home = TempHome::new();
        // No real `dsh` binary in test env -> install fails past the
        // profile-creation step, which is exactly the routing this test
        // proves: the default profile got created for a "dsh" row.
        let r = row("d1", "dsh", "some-npm-pkg", None);
        let _ = install("/definitely/not/a/real/dsh/binary", &r).await;
        assert!(dsh::list_profiles().iter().any(|p| p == DSH_DEFAULT_PROFILE));
    }

    #[tokio::test]
    async fn install_dispatch_rejects_unknown_source_kind() {
        let _home = TempHome::new();
        let r = row("x1", "carrier-pigeon", "nowhere", None);
        let Err(err) = install("dsh", &r).await else { panic!("expected an error") };
        assert!(err.contains("unknown source_kind"));
    }

    #[tokio::test]
    async fn install_dispatch_rejects_claude_marketplace_row_missing_plugin_name() {
        let _home = TempHome::new();
        let r = row("c1", "claude_marketplace", "https://github.com/org/repo", None);
        let Err(err) = install("dsh", &r).await else { panic!("expected an error") };
        assert!(err.contains("plugin_name"));
    }
}
