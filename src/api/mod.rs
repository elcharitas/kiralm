//! Shared app state + router assembly for both listeners. Both the
//! Unix socket and the TCP listener route to the same handlers.

pub mod agent;
pub mod agent_plugin;
pub mod anthropic;
pub mod dsh;
pub mod internal;
pub mod marketplace;
pub mod mcp;
pub mod openai;
pub mod tunnel;

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::Router;
use parking_lot::RwLock;
use tokio::sync::broadcast;

use crate::chats::Chats;
use crate::cloud::LoginProgressTx;
use crate::config::Config;
use crate::engine;
use crate::pull::{PullProgress, ProgressTx};
use crate::registry::{self, ModelRegistry, ModelRoute, ModelSource, ModelStatus};
use crate::residency;
use crate::settings::Settings;
use crate::state::{ModelManifest, State};

/// A chat session's server-side generation record: written by the
/// background task draining the upstream stream (openai.rs), read by
/// both the live response (via `tap`, while still running) and the
/// reconnect/poll endpoint (via `state`, at any time, even after the
/// task that wrote it is long gone).
pub struct SessionGeneration {
    pub state: RwLock<GenerationState>,
    /// Live raw-SSE tap for whoever made the original request. `Some`
    /// only while generation is in flight; the background task drops
    /// its sender on completion, which closes this for any still-
    /// attached response stream (recv() -> Closed) without touching
    /// `state`, which stays readable forever after.
    pub tap: parking_lot::Mutex<Option<broadcast::Sender<Bytes>>>,
    /// The background task draining upstream, so an explicit stop (not
    /// just a dropped response) can actually cancel the upstream
    /// connection -- aborting it is what tells llama-server to stop
    /// generating, same as dropping the connection always did before.
    pub task: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Clone, Default, serde::Serialize)]
pub struct GenerationState {
    pub model: String,
    pub text: String,
    pub running: bool,
    pub error: Option<String>,
    /// Real tool calls made this turn, in order --
    /// what the UI's "Worked for Xm Ys" collapsible actually expands to.
    #[serde(default)]
    pub steps: Vec<crate::tools::ToolStep>,
}

pub struct AppState {
    pub config: Config,
    pub registry: Arc<ModelRegistry>,
    pub state: RwLock<State>,
    /// UI preferences (~/.kiralm/settings.json), edited from Settings >
    /// General -- distinct from `state` (daemon-managed) and `config`
    /// (process-level, config.toml).
    pub settings: RwLock<Settings>,
    /// Persisted chat history (~/.kiralm/chats.json), replacing what
    /// used to live only in the browser's localStorage.
    pub chats: RwLock<Chats>,
    /// In-flight pulls, keyed by canonical id, for SSE progress fan-out.
    pub pulls: RwLock<HashMap<String, ProgressTx>>,
    /// In-flight cloud logins, keyed by device_code, for SSE progress
    /// fan-out, same pattern as `pulls` above.
    pub logins: RwLock<HashMap<String, LoginProgressTx>>,
    /// The supervised `cloudflared` child, if the tunnel is currently
    /// running, in-memory only, like
    /// `ensure_loaded`'s reaper task; `state.tunnel` is the persisted
    /// on/off record that survives a restart.
    pub tunnel_child: parking_lot::Mutex<Option<tokio::process::Child>>,
    /// Chat generations, keyed by the UI's session id, decoupled from
    /// any one HTTP connection so generation keeps running/buffering
    /// server-side even once the UI stops watching it.
    // Entries never expire, unbounded growth over a very long daemon
    // uptime with many sessions -- add a size/age-based sweep if that
    // ever shows up in practice.
    pub generations: RwLock<HashMap<String, Arc<SessionGeneration>>>,
    /// Live stdio MCP server connections, keyed by name -- in-memory
    /// only, `mcp_servers.json` is the on-disk record of what *should*
    /// be connected.
    pub mcp: crate::mcp::McpRegistry,
}

impl AppState {
    pub fn new(config: Config, registry: Arc<ModelRegistry>, state: State, settings: Settings, chats: Chats) -> AppState {
        AppState {
            config,
            registry,
            state: RwLock::new(state),
            settings: RwLock::new(settings),
            chats: RwLock::new(chats),
            pulls: RwLock::new(HashMap::new()),
            logins: RwLock::new(HashMap::new()),
            tunnel_child: parking_lot::Mutex::new(None),
            generations: RwLock::new(HashMap::new()),
            mcp: crate::mcp::McpRegistry::new(),
        }
    }

    /// Starts a fresh generation record for `session_id`, replacing any
    /// prior one (a session only ever has one generation in flight at a
    /// time). Returns it plus the subscriber for the initiating
    /// response, subscribed before the background task starts sending
    /// so no early tokens are missed.
    pub fn start_generation(
        &self,
        session_id: &str,
        model: &str,
    ) -> (Arc<SessionGeneration>, broadcast::Receiver<Bytes>) {
        let (tx, rx) = broadcast::channel(256);
        let sg = Arc::new(SessionGeneration {
            state: RwLock::new(GenerationState {
                model: model.to_string(),
                text: String::new(),
                running: true,
                error: None,
                steps: Vec::new(),
            }),
            tap: parking_lot::Mutex::new(Some(tx)),
            task: parking_lot::Mutex::new(None),
        });
        self.generations
            .write()
            .insert(session_id.to_string(), sg.clone());
        (sg, rx)
    }

    /// Stops a session's in-flight generation, if any: aborts the
    /// background task, which drops its upstream connection and so
    /// actually tells llama-server to stop rather than just detaching
    /// the UI's view of it.
    pub fn stop_generation(&self, session_id: &str) -> bool {
        let Some(sg) = self.generations.read().get(session_id).cloned() else {
            return false;
        };
        if let Some(handle) = sg.task.lock().take() {
            handle.abort();
        }
        *sg.tap.lock() = None;
        let mut st = sg.state.write();
        let was_running = st.running;
        st.running = false;
        was_running
    }

    /// Buffered-so-far text + running flag for a session, for a client
    /// that reconnects or polls instead of watching the live stream.
    /// Empty/not-running for a session that never generated anything.
    pub fn generation_snapshot(&self, session_id: &str) -> GenerationState {
        self.generations
            .read()
            .get(session_id)
            .map(|sg| sg.state.read().clone())
            .unwrap_or_default()
    }

    /// Session ids with generation currently in flight, for the
    /// sidebar's status dots to poll cheaply regardless of which
    /// session is focused.
    pub fn running_generation_ids(&self) -> Vec<String> {
        self.generations
            .read()
            .iter()
            .filter(|(_, sg)| sg.state.read().running)
            .map(|(id, _)| id.clone())
            .collect()
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

    /// Gets (or lazily creates) the broadcast channel used to fan out
    /// progress for a given device_code's in-flight login.
    pub fn login_channel(&self, device_code: &str) -> LoginProgressTx {
        if let Some(tx) = self.logins.read().get(device_code) {
            return tx.clone();
        }
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        self.logins
            .write()
            .insert(device_code.to_string(), tx.clone());
        tx
    }
}

pub type SharedState = Arc<AppState>;

/// (Re)builds a manifest's `ModelState` and inserts it into the live
/// registry. Used both at `serve` startup (one pass over every manifest
/// on disk) and right after a pull completes, so a freshly-pulled
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
/// and internal `/api/*` routes.
pub fn build_router(state: SharedState) -> Router {
    Router::new()
        .merge(openai::routes())
        .merge(anthropic::routes())
        .merge(internal::routes())
        .merge(tunnel::routes())
        .merge(mcp::routes())
        .merge(marketplace::routes())
        .merge(agent_plugin::routes())
        .merge(dsh::routes())
        .fallback(internal::static_handler)
        .with_state(state)
}

/// Resolves a name (alias or canonical) via the registry + state.json's
/// alias map.
pub fn resolve_model(app: &SharedState, name: &str) -> Option<String> {
    let aliases = app.state.read().aliases.clone();
    app.registry.resolve(name, &aliases)
}

pub enum GatewayError {
    /// Cached-unsupported architecture. Permanent, 4xx on
    /// both surfaces, carries the real cached llama.cpp error text.
    CachedUnsupported(String),
    /// Won't fit even after evicting eligible cold models.
    InsufficientRam(String),
    /// Spawn/health-check/tiny-completion failed transiently.
    LoadFailed(String),
    /// No stored token, an expired one, or a backend 401 on an actual
    /// cloud chat call. Sibling to the three arms above, same 4xx treatment.
    AuthRequired(String),
}

/// Ensures `canonical_id` has a live `llama-server` child, spawning one
/// synchronously if cold (admission control applies), and
/// returns its port. Reuses an already-loaded child on repeat calls,
/// this is what keeps the model resident across requests.
///
/// STUB NOTE: no LRU eviction of cold models to make room, and no
/// automatic-flag matrix beyond the always-on set. See
/// engine.rs's module note. The owning residency agent should extend
/// this rather than replace it wholesale; the admission math and
/// registry bookkeeping here are real.
pub async fn ensure_loaded(app: &SharedState, canonical_id: &str) -> Result<ModelRoute, GatewayError> {
    let Some(existing) = app.registry.get_clone(canonical_id) else {
        return Err(GatewayError::LoadFailed(format!(
            "model '{canonical_id}' not in registry"
        )));
    };

    // Cloud entries never spawn a local child. This is the one and
    // only place "is this model spawnable locally" is decided. No
    // admission check, no health poll: kiralm-web owns that model's
    // residency, not us.
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
    // Cloud entries are always `Loaded` but occupy zero local RAM. The
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
    // to a reaper task that just waits (never kills). The registry only
    // tracks the pid, which is enough for `DELETE`'s graceful-kill path.
    // No supervising/restart task yet. See module STUB NOTE.
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
