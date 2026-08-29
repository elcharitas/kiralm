//! Shared app state + router assembly for both listeners. Both the
//! Unix socket and the TCP listener route to the same handlers.

pub mod agent;
pub mod agent_plugin;
pub mod anthropic;
pub mod attachments;
pub mod dsh;
pub mod internal;
pub mod knowledge;
pub mod marketplace;
pub mod mcp;
pub mod openai;
pub mod plugin_catalog;
pub mod tunnel;
pub mod workspace_presets;

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::Router;
use parking_lot::RwLock;
use tokio::sync::broadcast;

use crate::chats::Chats;
use crate::cloud::LoginProgressTx;
use crate::config::Config;
use crate::engine::supervisor::{
    EngineSupervisor, SupervisorError, SupervisorOptions,
};
use crate::engine::optimization::OptimizationProfile;
use crate::engine::{self, EngineCapabilities};
use crate::events::{EventBus, EventTopic};
use crate::knowledge::{
    DeterministicEmbedder, IndexCancellation, IndexLimits, KnowledgeIndex,
};
use crate::pull::{ProgressTx, PullProgress};
use crate::registry::{self, ModelRegistry, ModelRoute, ModelSource};
use crate::residency;
use crate::settings::Settings;
use crate::state::{ModelManifest, State};

// ---------------------------------------------------------------------
// Upstream error sanitizing: a failed call to another service (a local
// llama-server child, kiralm-web, cloudflared's provisioning API) must
// never have its raw body or error Display forwarded verbatim into a
// user-facing message -- that body is arbitrary content from a third
// party. Log the real detail for diagnosis, return a short safe reason.
// ---------------------------------------------------------------------

/// A response came back but wasn't a success status.
pub fn upstream_error(context: &str, status: reqwest::StatusCode, body: &str) -> String {
    tracing::warn!("{context}: upstream returned {status}: {body}");
    format!("{context} failed (upstream returned {status})")
}

/// The request never got a response at all (refused, timed out, DNS).
pub fn upstream_unreachable(context: &str, e: &reqwest::Error) -> String {
    tracing::warn!("{context}: {e}");
    format!("{context} failed: could not reach upstream")
}

/// A response came back but couldn't be parsed as expected.
pub fn upstream_malformed(context: &str, e: &impl std::fmt::Display) -> String {
    tracing::warn!("{context}: malformed upstream response: {e}");
    format!("{context} failed: upstream returned an unexpected response")
}

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
    pub task: parking_lot::Mutex<Option<tokio::task::AbortHandle>>,
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
    pub tunnel_listener: tokio::sync::mpsc::Sender<crate::TunnelListenerCommand>,
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
    pub events: Arc<EventBus>,
    pub approvals: Arc<crate::approvals::ApprovalManager>,
    pub engine_capabilities: EngineCapabilities,
    pub supervisor: EngineSupervisor,
    pub knowledge: Arc<KnowledgeIndex<DeterministicEmbedder>>,
    pub knowledge_job: parking_lot::Mutex<Option<IndexCancellation>>,
    pub workspace_presets: Arc<crate::workspace_presets::WorkspacePresetStore>,
    pub attachments: Arc<crate::attachments::AttachmentStore>,
}

impl AppState {
    pub fn new(
        config: Config,
        registry: Arc<ModelRegistry>,
        state: State,
        settings: Settings,
        chats: Chats,
        tunnel_listener: tokio::sync::mpsc::Sender<crate::TunnelListenerCommand>,
    ) -> AppState {
        Self::new_with_engine_capabilities(
            config,
            registry,
            state,
            settings,
            chats,
            tunnel_listener,
            EngineCapabilities::default(),
        )
    }

    pub fn new_with_engine_capabilities(
        config: Config,
        registry: Arc<ModelRegistry>,
        state: State,
        settings: Settings,
        chats: Chats,
        tunnel_listener: tokio::sync::mpsc::Sender<crate::TunnelListenerCommand>,
        engine_capabilities: EngineCapabilities,
    ) -> AppState {
        let path = std::env::temp_dir().join(format!(
            "kiralm-knowledge-test-{}.json",
            uuid::Uuid::new_v4()
        ));
        let knowledge = Arc::new(
            KnowledgeIndex::open(
                path,
                DeterministicEmbedder::default(),
                IndexLimits::default(),
            )
            .expect("temporary knowledge index must open"),
        );
        let preset_path = std::env::temp_dir().join(format!(
            "kiralm-workspace-presets-test-{}.json",
            uuid::Uuid::new_v4()
        ));
        let workspace_presets = Arc::new(
            crate::workspace_presets::WorkspacePresetStore::open(preset_path)
                .expect("temporary workspace preset store must open"),
        );
        Self::new_with_services(
            config,
            registry,
            state,
            settings,
            chats,
            tunnel_listener,
            engine_capabilities,
            knowledge,
            workspace_presets,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_knowledge(
        config: Config,
        registry: Arc<ModelRegistry>,
        state: State,
        settings: Settings,
        chats: Chats,
        tunnel_listener: tokio::sync::mpsc::Sender<crate::TunnelListenerCommand>,
        engine_capabilities: EngineCapabilities,
        knowledge: Arc<KnowledgeIndex<DeterministicEmbedder>>,
    ) -> AppState {
        let preset_path = std::env::temp_dir().join(format!(
            "kiralm-workspace-presets-test-{}.json",
            uuid::Uuid::new_v4()
        ));
        let workspace_presets = Arc::new(
            crate::workspace_presets::WorkspacePresetStore::open(preset_path)
                .expect("temporary workspace preset store must open"),
        );
        Self::new_with_services(
            config,
            registry,
            state,
            settings,
            chats,
            tunnel_listener,
            engine_capabilities,
            knowledge,
            workspace_presets,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_services(
        config: Config,
        registry: Arc<ModelRegistry>,
        state: State,
        settings: Settings,
        chats: Chats,
        tunnel_listener: tokio::sync::mpsc::Sender<crate::TunnelListenerCommand>,
        engine_capabilities: EngineCapabilities,
        knowledge: Arc<KnowledgeIndex<DeterministicEmbedder>>,
        workspace_presets: Arc<crate::workspace_presets::WorkspacePresetStore>,
    ) -> AppState {
        let attachment_path = std::env::temp_dir().join(format!(
            "kiralm-attachments-test-{}",
            uuid::Uuid::new_v4()
        ));
        let attachments = Arc::new(
            crate::attachments::AttachmentStore::open(
                attachment_path,
                crate::attachments::AttachmentLimits::default(),
            )
            .expect("temporary attachment store must open"),
        );
        Self::new_with_stores(
            config,
            registry,
            state,
            settings,
            chats,
            tunnel_listener,
            engine_capabilities,
            knowledge,
            workspace_presets,
            attachments,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_stores(
        config: Config,
        registry: Arc<ModelRegistry>,
        state: State,
        settings: Settings,
        chats: Chats,
        tunnel_listener: tokio::sync::mpsc::Sender<crate::TunnelListenerCommand>,
        engine_capabilities: EngineCapabilities,
        knowledge: Arc<KnowledgeIndex<DeterministicEmbedder>>,
        workspace_presets: Arc<crate::workspace_presets::WorkspacePresetStore>,
        attachments: Arc<crate::attachments::AttachmentStore>,
    ) -> AppState {
        let events = Arc::new(EventBus::default());
        let approvals = Arc::new(crate::approvals::ApprovalManager::new(events.clone()));
        let supervisor = EngineSupervisor::production(
            registry.clone(),
            events.clone(),
            config.llama_server_bin.clone(),
            config.llama_server_port_range,
            SupervisorOptions {
                configured_headroom_gb: config.ram_headroom_gb,
                engine_capabilities: engine_capabilities.clone(),
                optimization_profile: OptimizationProfile::parse(
                    &config.optimization_profile,
                )
                .unwrap_or(OptimizationProfile::Balanced),
                ..SupervisorOptions::default()
            },
        );
        AppState {
            config,
            registry,
            state: RwLock::new(state),
            settings: RwLock::new(settings),
            chats: RwLock::new(chats),
            pulls: RwLock::new(HashMap::new()),
            logins: RwLock::new(HashMap::new()),
            tunnel_child: parking_lot::Mutex::new(None),
            tunnel_listener,
            generations: RwLock::new(HashMap::new()),
            mcp: crate::mcp::McpRegistry::new(),
            events,
            approvals,
            engine_capabilities,
            supervisor,
            knowledge,
            knowledge_job: parking_lot::Mutex::new(None),
            workspace_presets,
            attachments,
        }
    }

    pub fn workspace_preset_for(
        &self,
        project_id: &str,
    ) -> Option<crate::workspace_presets::WorkspacePreset> {
        let binding = self
            .state
            .read()
            .workspace_preset_bindings
            .get(project_id)
            .cloned()?;
        self.workspace_presets
            .get(&binding.preset_id)
            .filter(|preset| preset.revision == binding.preset_revision)
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
        self.stop_generation(session_id);
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
        self.approvals.cancel_chat(session_id);
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
        .merge(attachments::routes())
        .merge(internal::routes())
        .merge(knowledge::routes())
        .merge(workspace_presets::routes())
        .merge(tunnel::routes())
        .merge(mcp::routes())
        .merge(marketplace::routes())
        .merge(agent_plugin::routes())
        .merge(dsh::routes())
        .merge(plugin_catalog::routes())
        .fallback(internal::static_handler)
        .with_state(state)
}

/// Resolves a name (alias or canonical) via the registry + state.json's
/// alias map.
pub fn resolve_model(app: &SharedState, name: &str) -> Option<String> {
    let aliases = app.state.read().aliases.clone();
    app.registry.resolve(name, &aliases)
}

pub fn prune_artifact_cache(
    app: &SharedState,
) -> anyhow::Result<crate::pull::cache::PruneReport> {
    let cache = crate::pull::artifact_cache()?;
    let mut protections = crate::pull::cache::PruneProtections {
        downloading: cache.active_artifact_hashes()?,
        ..crate::pull::cache::PruneProtections::default()
    };
    for model in app.registry.list() {
        if model.source != ModelSource::Local {
            continue;
        }
        let Ok(manifest) = ModelManifest::load(&model.gguf_path) else {
            continue;
        };
        if model.hot {
            protections.hot.insert(manifest.sha256.clone());
        }
        if model.active_requests > 0 {
            protections.active.insert(manifest.sha256);
        }
    }
    let quota =
        (app.config.cache_quota_gb * 1024.0 * 1024.0 * 1024.0) as u64;
    let report = cache.prune_to_quota(quota, &protections)?;
    app.events.publish(
        EventTopic::Cache,
        "pruned",
        None,
        serde_json::json!({
            "removed": &report.removed,
            "bytes_removed": report.bytes_removed,
            "bytes_remaining": report.bytes_remaining,
            "quota_bytes": quota,
            "root": cache.root(),
        }),
    );
    Ok(report)
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

/// Returns a cloud route or a lease on a supervised local engine.
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

    if let Some(entry) = app.state.read().models.get(canonical_id) {
        if let Some(vc) = &entry.verdict_cache {
            if vc.status == "unsupported" {
                return Err(GatewayError::CachedUnsupported(
                    vc.error.clone().unwrap_or_default(),
                ));
            }
        }
    }

    app.supervisor
        .acquire(canonical_id)
        .await
        .map(ModelRoute::Local)
        .map_err(map_supervisor_error)
}

fn map_supervisor_error(error: SupervisorError) -> GatewayError {
    match error {
        SupervisorError::InsufficientRam {
            canonical_id,
            shortfall_bytes,
        } => GatewayError::InsufficientRam(format!(
            "need {shortfall_bytes} more bytes free to load '{canonical_id}'"
        )),
        other => GatewayError::LoadFailed(other.to_string()),
    }
}
