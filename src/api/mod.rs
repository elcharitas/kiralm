//! Shared app state + router assembly for both listeners (§2). Both the
//! Unix socket and the TCP listener route to the same handlers.

pub mod anthropic;
pub mod internal;
pub mod openai;
pub mod tunnel;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use parking_lot::RwLock;

use crate::config::Config;
use crate::engine;
use crate::pull::{PullProgress, ProgressTx};
use crate::registry::{self, ModelRegistry, ModelRoute, ModelSource, ModelStatus};
use crate::residency;
use crate::state::{ModelManifest, State};

pub struct AppState {
    pub config: Config,
    pub registry: Arc<ModelRegistry>,
    pub state: RwLock<State>,
    /// In-flight pulls, keyed by canonical id, for SSE progress fan-out.
    pub pulls: RwLock<HashMap<String, ProgressTx>>,
    /// The supervised `cloudflared` child, if the tunnel is currently
    /// running (§"Tunnel client (daemon)") — in-memory only, like
    /// `ensure_loaded`'s reaper task; `state.tunnel` is the persisted
    /// on/off record that survives a restart.
    pub tunnel_child: parking_lot::Mutex<Option<tokio::process::Child>>,
}

impl AppState {
    pub fn new(config: Config, registry: Arc<ModelRegistry>, state: State) -> AppState {
        AppState {
            config,
            registry,
            state: RwLock::new(state),
            pulls: RwLock::new(HashMap::new()),
            tunnel_child: parking_lot::Mutex::new(None),
        }
    }

    /// Gets (or lazily creates) the broadcast channel used to fan out
    /// progress for a given canonical id's in-flight pull.
    pub fn pull_channel(&self, canonical_id: &str) -> ProgressTx {
        if let Some(tx) = self.pulls.read().get(canonical_id) {
            return tx.clone();
        }
        let (tx, _rx) = tokio::sync::broadcast::channel::<PullProgress>(256);
        self.pulls
            .write()
            .insert(canonical_id.to_string(), tx.clone());
        tx
    }
}

pub type SharedState = Arc<AppState>;

/// (Re)builds a manifest's `ModelState` and inserts it into the live
/// registry — used both at `serve` startup (one pass over every manifest
/// on disk) and right after a pull completes (§8), so a freshly-pulled
/// model is loadable without a daemon restart.
pub fn register_model(app: &SharedState, manifest: &ModelManifest) {
    let ram = residency::read_ram_info();
    let headroom = residency::headroom_bytes(ram.total_bytes, app.config.ram_headroom_gb);
    let budget = ram.total_bytes.saturating_sub(headroom);
    let engine_version = engine::detect_version(&app.config.llama_server_bin);

    let state = app.state.read();
    let entry = state.models.get(&manifest.canonical_id);
    let hot = entry.map(|e| e.hot).unwrap_or(false);

    let cached_unsupported = entry
        .and_then(|e| e.verdict_cache.as_ref())
        .filter(|vc| vc.status == "unsupported" && vc.engine_version == engine_version);
    let verdict = if let Some(vc) = cached_unsupported {
        registry::Verdict {
            tier: registry::VerdictTier::UnsupportedArchitecture,
            reason: vc.error.clone().unwrap_or_default(),
            tok_per_sec_multiplier: None,
        }
    } else {
        registry::compute_ratio_verdict(manifest.is_moe, manifest.size_bytes, budget)
    };
    drop(state);

    let mut model_state = registry::ModelState::from_manifest(manifest, hot);
    model_state.gguf_path = app.config.model_dir.join(&manifest.filename);
    model_state.verdict = Some(verdict);
    app.registry.insert(model_state);
}

/// Assembles the full route table: OpenAI-compatible, Anthropic-compatible,
/// and internal `/api/*` routes (§2.1, §2.2, §2.5).
pub fn build_router(state: SharedState) -> Router {
    Router::new()
        .merge(openai::routes())
        .merge(anthropic::routes())
        .merge(internal::routes())
        .merge(tunnel::routes())
        .fallback(internal::static_handler)
        .with_state(state)
}

/// Resolves a name (alias or canonical) via the registry + state.json's
/// alias map (§6).
pub fn resolve_model(app: &SharedState, name: &str) -> Option<String> {
    let aliases = app.state.read().aliases.clone();
    app.registry.resolve(name, &aliases)
}

pub enum GatewayError {
    /// Cached-unsupported architecture (§5.2/§5.3) — permanent, 4xx on
    /// both surfaces, carries the real cached llama.cpp error text.
    CachedUnsupported(String),
    /// Won't fit even after evicting eligible cold models (§5.4).
    InsufficientRam(String),
    /// Spawn/health-check/tiny-completion failed transiently (§4.4).
    LoadFailed(String),
    /// No stored token, an expired one, or a backend 401 on an actual
    /// cloud chat call (Cloud integration §"Logged-out / invalid-token
    /// behavior"). Sibling to the three arms above, same 4xx treatment.
    AuthRequired(String),
}

/// Ensures `canonical_id` has a live `llama-server` child, spawning one
/// synchronously if cold (admission control per §5.4 applies), and
/// returns its port. Reuses an already-loaded child on repeat calls —
/// this is what keeps the model resident across requests (goal #2).
///
/// STUB NOTE: no LRU eviction of cold models to make room, and no
/// automatic-flag matrix beyond the always-on set (§4.3) — see
/// engine.rs's module note. The owning residency agent should extend
/// this rather than replace it wholesale; the admission math and
/// registry bookkeeping here are real.
pub async fn ensure_loaded(app: &SharedState, canonical_id: &str) -> Result<ModelRoute, GatewayError> {
    let Some(existing) = app.registry.get_clone(canonical_id) else {
        return Err(GatewayError::LoadFailed(format!(
            "model '{canonical_id}' not in registry"
        )));
    };

    // Cloud entries never spawn a local child — this is the one and
    // only place "is this model spawnable locally" is decided (Cloud
    // integration §"Routing choke point"). No admission check, no
    // health poll: nodelm-web owns that model's residency, not us.
    if existing.source == ModelSource::Cloud {
        return Ok(ModelRoute::Cloud);
    }

    if let ModelStatus::Loaded = existing.status {
        if let Some(port) = existing.port {
            return Ok(ModelRoute::Local(port));
        }
    }

    if let Some(entry) = app.state.read().models.get(canonical_id) {
        if let Some(vc) = &entry.verdict_cache {
            if vc.status == "unsupported" {
                return Err(GatewayError::CachedUnsupported(
                    vc.error.clone().unwrap_or_default(),
                ));
            }
        }
    }

    let ram = residency::read_ram_info();
    let headroom = residency::headroom_bytes(ram.total_bytes, app.config.ram_headroom_gb);
    // Cloud entries are always `Loaded` but occupy zero local RAM — the
    // admission budget only ever tracks locally-spawned children.
    let loaded_bytes: u64 = app
        .registry
        .list()
        .into_iter()
        .filter(|m| {
            m.source == ModelSource::Local
                && matches!(m.status, ModelStatus::Loaded)
                && m.canonical_id != canonical_id
        })
        .map(|m| m.size_bytes)
        .sum();
    if let Err(shortfall) =
        residency::check_admission(loaded_bytes, existing.size_bytes, ram.total_bytes, headroom)
    {
        return Err(GatewayError::InsufficientRam(format!(
            "need {shortfall} more bytes free to load '{canonical_id}'"
        )));
    }

    let mut loading = existing.clone();
    loading.status = ModelStatus::Loading;
    app.registry.insert(loading);

    let port = match engine::allocate_port(app.config.llama_server_port_range) {
        Ok(p) => p,
        Err(e) => {
            let mut failed = existing.clone();
            failed.status = ModelStatus::Failed(e.to_string());
            app.registry.insert(failed);
            return Err(GatewayError::LoadFailed(e.to_string()));
        }
    };

    let spawn_result = engine::spawn(
        &app.config.llama_server_bin,
        &existing.gguf_path,
        canonical_id,
        port,
        &[],
    )
    .await;

    let mut child = match spawn_result {
        Ok(c) => c,
        Err(e) => {
            let mut failed = existing.clone();
            failed.status = ModelStatus::Failed(e.to_string());
            app.registry.insert(failed);
            return Err(GatewayError::LoadFailed(e.to_string()));
        }
    };
    let pid = child.id();

    if let Err(e) = engine::wait_healthy(port, std::time::Duration::from_secs(30)).await {
        engine::shutdown(child).await;
        let msg = format!("{e}: {}", engine::tail_log(canonical_id, 50));
        let mut failed = existing.clone();
        failed.status = ModelStatus::Failed(msg.clone());
        app.registry.insert(failed);
        return Err(GatewayError::LoadFailed(msg));
    }

    // Detach: `kill_on_drop(true)` means simply letting `child` fall out
    // of scope here would kill the server we just started, and
    // `mem::forget` would leak a zombie once it exits. Instead hand it
    // to a reaper task that just waits (never kills) — the registry only
    // tracks the pid, which is enough for `DELETE`'s graceful-kill path.
    // No supervising/restart task yet — see module STUB NOTE.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    let mut loaded = existing;
    loaded.status = ModelStatus::Loaded;
    loaded.pid = pid;
    loaded.port = Some(port);
    loaded.last_used = std::time::Instant::now();
    app.registry.insert(loaded);

    Ok(ModelRoute::Local(port))
}
