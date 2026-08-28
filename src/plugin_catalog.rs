//! kiralm-web's plugin catalog: a live, per-request proxy to
//! `GET /api/plugins`/`GET /api/plugins/:id`, and the install/uninstall
//! dispatch that unifies marketplace.rs, agent_plugin.rs and dsh.rs
//! behind one browsable list. No local cache of catalog content -- every
//! browse is a real kiralm-web round trip. The one thing kept locally is
//! `plugin_catalog_installed_ids.json`, the set of catalog ids installed
//! through this flow -- local state, not a content cache, same footing
//! as skills.json/mcp_servers.json.

use std::path::PathBuf;

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
struct CatalogPageResponse {
    data: Vec<CloudPlugin>,
    total: usize,
}

pub const DEFAULT_CATALOG_LIMIT: usize = 50;
pub const MAX_CATALOG_LIMIT: usize = 200;

// ---------------------------------------------------------------------
// Locally-installed catalog id set
// ---------------------------------------------------------------------

fn installed_ids_path() -> PathBuf {
    kiralm_home().join("plugin_catalog_installed_ids.json")
}

fn installed_ids() -> std::collections::HashSet<String> {
    std::fs::read_to_string(installed_ids_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_installed_ids(ids: &std::collections::HashSet<String>) -> anyhow::Result<()> {
    let path = installed_ids_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(ids)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Called by `POST /api/plugins/catalog/:id/install` once dispatch
/// actually succeeds.
pub fn mark_installed(id: &str) {
    let mut ids = installed_ids();
    if ids.insert(id.to_string()) {
        if let Err(e) = save_installed_ids(&ids) {
            tracing::warn!("plugin catalog: failed to persist installed id: {e}");
        }
    }
}

/// Called by `DELETE /api/plugins/catalog/:id` once dispatch actually
/// succeeds.
pub fn mark_uninstalled(id: &str) {
    let mut ids = installed_ids();
    if ids.remove(id) {
        if let Err(e) = save_installed_ids(&ids) {
            tracing::warn!("plugin catalog: failed to persist uninstalled id: {e}");
        }
    }
}

// ---------------------------------------------------------------------
// Live kiralm-web proxy
// ---------------------------------------------------------------------

/// A request may chain a few backfill round trips (see
/// `list_catalog_page`), so this is a little longer than the old
/// background sync's fixed 3s.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build().map_err(|e| e.to_string())
}

/// One live `GET {api_base_url}/api/plugins` call, `q`/`offset`/`limit`
/// forwarded as-is.
async fn fetch_page(
    client: &reqwest::Client,
    api_base_url: &str,
    q: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<(Vec<CloudPlugin>, usize), String> {
    let url = format!("{}/api/plugins", api_base_url.trim_end_matches('/'));
    let mut req = client.get(&url).query(&[("offset", offset.to_string()), ("limit", limit.to_string())]);
    if let Some(q) = q.map(str::trim).filter(|s| !s.is_empty()) {
        req = req.query(&[("q", q)]);
    }
    let resp = req.send().await.map_err(|e| format!("kiralm-web unreachable: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET /api/plugins returned {}", resp.status()));
    }
    let parsed: CatalogPageResponse =
        resp.json().await.map_err(|e| format!("kiralm-web returned an unexpected shape: {e}"))?;
    Ok((parsed.data, parsed.total))
}

/// One live `GET {api_base_url}/api/plugins?ids=...` call -- the
/// canonical display data for a known, locally-tracked set of installed
/// ids, one round trip no matter how many ids.
async fn fetch_by_ids(
    client: &reqwest::Client,
    api_base_url: &str,
    ids: &std::collections::HashSet<String>,
    q: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<(Vec<CloudPlugin>, usize), String> {
    let url = format!("{}/api/plugins", api_base_url.trim_end_matches('/'));
    let ids_param = ids.iter().cloned().collect::<Vec<_>>().join(",");
    let mut req =
        client.get(&url).query(&[("ids", ids_param), ("offset", offset.to_string()), ("limit", limit.to_string())]);
    if let Some(q) = q.map(str::trim).filter(|s| !s.is_empty()) {
        req = req.query(&[("q", q)]);
    }
    let resp = req.send().await.map_err(|e| format!("kiralm-web unreachable: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET /api/plugins?ids=... returned {}", resp.status()));
    }
    let parsed: CatalogPageResponse =
        resp.json().await.map_err(|e| format!("kiralm-web returned an unexpected shape: {e}"))?;
    Ok((parsed.data, parsed.total))
}

/// A request can't spiral into fetching the whole catalog just to fill
/// one short page.
const MAX_BACKFILL_ROUNDS: usize = 3;

/// `GET /api/plugins/catalog`'s live dispatch. `installed == Some(true)`
/// makes at most one kiralm-web call, scoped to the locally-tracked
/// installed id set (none at all if that set is empty) -- kiralm-web
/// never sees a per-row lookup for this branch. Everything else forwards
/// `q`/`offset`/`limit` to kiralm-web's own SQL-backed search/paging;
/// `installed == Some(false)` additionally backfills later pages when
/// local filtering drops a page short.
pub async fn list_catalog_page(
    client: &reqwest::Client,
    api_base_url: &str,
    q: Option<&str>,
    installed: Option<bool>,
    offset: usize,
    limit: usize,
) -> Result<(Vec<(CloudPlugin, bool)>, usize), String> {
    let ids = installed_ids();

    if installed == Some(true) {
        if ids.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let (page, total) = fetch_by_ids(client, api_base_url, &ids, q, offset, limit).await?;
        return Ok((page.into_iter().map(|p| (p, true)).collect(), total));
    }

    let mut collected: Vec<(CloudPlugin, bool)> = Vec::new();
    let mut cursor = offset;
    let mut rounds = 0usize;
    let total = loop {
        let (page, page_total) = fetch_page(client, api_base_url, q, cursor, limit).await?;
        let page_len = page.len();
        cursor += page_len;
        rounds += 1;

        for p in page {
            let is_installed = ids.contains(&p.id);
            if installed == Some(false) && is_installed {
                continue;
            }
            collected.push((p, is_installed));
        }

        let short = installed == Some(false) && collected.len() < limit;
        if !short || page_len == 0 || cursor >= page_total || rounds >= MAX_BACKFILL_ROUNDS {
            break page_total;
        }
    };
    collected.truncate(limit);
    Ok((collected, total))
}

/// Resolves a catalog row by id for install/uninstall dispatch.
pub async fn find_row(client: &reqwest::Client, api_base_url: &str, id: &str) -> Result<Option<CloudPlugin>, String> {
    let url = format!("{}/api/plugins/{}", api_base_url.trim_end_matches('/'), id);
    let resp = client.get(&url).send().await.map_err(|e| format!("kiralm-web unreachable: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("GET /api/plugins/{id} returned {}", resp.status()));
    }
    resp.json::<CloudPlugin>().await.map(Some).map_err(|e| format!("kiralm-web returned an unexpected shape: {e}"))
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
            let outcome = marketplace::install_plugin(&added.slug, plugin_name).await?;
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
    use std::path::PathBuf as TestPathBuf;

    struct TempHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
        dir: TestPathBuf,
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

    fn plugin_json(id: &str, source_kind: &str, source: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "name": id, "description": null,
            "source_kind": source_kind, "source": source,
            "plugin_name": null, "enabled": true, "sort_order": 0,
        })
    }

    #[test]
    fn deserializes_get_plugins_shape() {
        let raw = r#"{"data":[{"id":"a","name":"A","description":null,
            "source_kind":"claude_marketplace","source":"https://github.com/org/repo",
            "plugin_name":"plug","enabled":true,"sort_order":0}],"total":1}"#;
        let parsed: CatalogPageResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.total, 1);
        assert_eq!(parsed.data[0].plugin_name.as_deref(), Some("plug"));
    }

    #[test]
    fn mark_installed_and_uninstalled_round_trip() {
        let _home = TempHome::new();
        assert!(installed_ids().is_empty());

        mark_installed("p1");
        mark_installed("p2");
        let ids = installed_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("p1") && ids.contains("p2"));

        mark_uninstalled("p1");
        let ids = installed_ids();
        assert_eq!(ids.len(), 1);
        assert!(ids.contains("p2"));
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

    #[tokio::test]
    async fn find_row_resolves_via_live_kiralm_web_lookup() {
        let upstream = axum::Router::new().route(
            "/api/plugins/:id",
            axum::routing::get(|axum::extract::Path(id): axum::extract::Path<String>| async move {
                use axum::response::IntoResponse;
                if id == "real1" {
                    (axum::http::StatusCode::OK, axum::Json(plugin_json("real1", "dsh", "pkg-x"))).into_response()
                } else {
                    axum::http::StatusCode::NOT_FOUND.into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let api_base_url = format!("http://127.0.0.1:{port}");
        let client = build_client().unwrap();

        let found = find_row(&client, &api_base_url, "real1").await.unwrap();
        assert_eq!(found.unwrap().id, "real1");

        let missing = find_row(&client, &api_base_url, "nope").await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn list_catalog_page_installed_true_with_empty_set_never_calls_kiralm_web() {
        let _home = TempHome::new();
        let client = build_client().unwrap();
        // Nothing listens here -- a real error if this branch dialed out.
        let (rows, total) = list_catalog_page(&client, "http://127.0.0.1:1", None, Some(true), 0, 10).await.unwrap();
        assert_eq!(rows.len(), 0);
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn list_catalog_page_installed_true_makes_one_batched_ids_call() {
        let _home = TempHome::new();
        mark_installed("i1");
        mark_installed("i2");

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_for_handler = calls.clone();
        let upstream = axum::Router::new().route(
            "/api/plugins",
            axum::routing::get(
                move |axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>| {
                    let calls = calls_for_handler.clone();
                    async move {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let ids_param = params.get("ids").cloned().unwrap_or_default();
                        let mut ids: Vec<&str> = ids_param.split(',').collect();
                        ids.sort();
                        let page: Vec<_> = ids.iter().map(|id| plugin_json(id, "dsh", id)).collect();
                        axum::Json(serde_json::json!({"data": page, "total": page.len()}))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let api_base_url = format!("http://127.0.0.1:{port}");
        let client = build_client().unwrap();

        let (rows, total) = list_catalog_page(&client, &api_base_url, None, Some(true), 0, 10).await.unwrap();
        assert_eq!(total, 2);
        assert_eq!(rows.iter().map(|(p, _)| p.id.as_str()).collect::<Vec<_>>(), vec!["i1", "i2"]);
        assert!(rows.iter().all(|(_, installed)| *installed));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1, "exactly one batched call, not one per id");
    }

    #[tokio::test]
    async fn list_catalog_page_uninstalled_backfills_a_short_page() {
        let _home = TempHome::new();
        mark_installed("a1");

        let upstream = axum::Router::new().route(
            "/api/plugins",
            axum::routing::get(
                move |axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                    let offset: usize = params.get("offset").and_then(|s| s.parse().ok()).unwrap_or(0);
                    let page = if offset == 0 {
                        vec![
                            plugin_json("a1", "agent_plugin", "src-a1"),
                            plugin_json("u1", "dsh", "pkg-1"),
                            plugin_json("u2", "dsh", "pkg-2"),
                        ]
                    } else {
                        vec![
                            plugin_json("u3", "dsh", "pkg-3"),
                            plugin_json("u4", "dsh", "pkg-4"),
                            plugin_json("a2", "agent_plugin", "src-a2"),
                        ]
                    };
                    axum::Json(serde_json::json!({"data": page, "total": 6}))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let client = build_client().unwrap();
        let api_base_url = format!("http://127.0.0.1:{port}");
        let (rows, total) = list_catalog_page(&client, &api_base_url, None, Some(false), 0, 3).await.unwrap();

        assert_eq!(total, 6);
        assert_eq!(rows.iter().map(|(p, _)| p.id.as_str()).collect::<Vec<_>>(), vec!["u1", "u2", "u3"]);
        assert!(rows.iter().all(|(_, installed)| !installed));
    }

    #[tokio::test]
    async fn list_catalog_page_upstream_failure_is_a_real_error_not_empty() {
        let client = build_client().unwrap();
        let err = list_catalog_page(&client, "http://127.0.0.1:1", None, None, 0, 10).await.unwrap_err();
        assert!(err.contains("unreachable"));
    }
}
