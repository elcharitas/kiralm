use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::{Mutex as SyncMutex, RwLock as SyncRwLock};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;

use crate::events::{EventBus, EventTopic};
use crate::engine::optimization::{
    GpuHardware, HardwareProfile, ModelOptimizationMetadata,
    OptimizationPlanner, OptimizationProfile, OptimizationRequest,
};
use crate::registry::{ModelHealth, ModelRegistry, ModelSource, ModelStatus};
use crate::residency::{self, RamInfo, ResidencyCandidate};

use super::{allocate_port, spawn, wait_healthy, EngineCapabilities};

type BackendFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub canonical_id: String,
    pub gguf_path: PathBuf,
    pub port: u16,
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExit {
    pub code: Option<i32>,
}

pub trait EngineProcess: Send {
    fn pid(&self) -> Option<u32>;

    fn try_wait(&mut self) -> Result<Option<ProcessExit>, String>;

    fn shutdown(self: Box<Self>) -> BackendFuture<()>;
}

pub trait EngineBackend: Send + Sync {
    fn allocate_port(&self) -> Result<u16, String>;

    fn spawn(&self, request: SpawnRequest)
        -> BackendFuture<Result<Box<dyn EngineProcess>, String>>;

    fn wait_healthy(&self, port: u16, timeout: Duration) -> BackendFuture<Result<(), String>>;

    fn check_health(&self, port: u16) -> BackendFuture<bool>;

    fn ram_info(&self) -> RamInfo;

    fn rss_bytes(&self, pid: u32) -> Option<u64>;
}

pub struct SystemEngineBackend {
    llama_bin: String,
    port_range: (u16, u16),
    client: reqwest::Client,
}

impl SystemEngineBackend {
    pub fn new(llama_bin: impl Into<String>, port_range: (u16, u16)) -> Self {
        Self {
            llama_bin: llama_bin.into(),
            port_range,
            client: reqwest::Client::new(),
        }
    }
}

struct TokioEngineProcess {
    child: tokio::process::Child,
}

impl EngineProcess for TokioEngineProcess {
    fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    fn try_wait(&mut self) -> Result<Option<ProcessExit>, String> {
        self.child
            .try_wait()
            .map(|status| {
                status.map(|status| ProcessExit {
                    code: status.code(),
                })
            })
            .map_err(|error| error.to_string())
    }

    fn shutdown(self: Box<Self>) -> BackendFuture<()> {
        Box::pin(async move {
            super::shutdown(self.child).await;
        })
    }
}

impl EngineBackend for SystemEngineBackend {
    fn allocate_port(&self) -> Result<u16, String> {
        allocate_port(self.port_range).map_err(|error| error.to_string())
    }

    fn spawn(
        &self,
        request: SpawnRequest,
    ) -> BackendFuture<Result<Box<dyn EngineProcess>, String>> {
        let llama_bin = self.llama_bin.clone();
        Box::pin(async move {
            let child = spawn(
                &llama_bin,
                &request.gguf_path,
                &request.canonical_id,
                request.port,
                &request.extra_args,
            )
            .await
            .map_err(|error| error.to_string())?;
            Ok(Box::new(TokioEngineProcess { child }) as Box<dyn EngineProcess>)
        })
    }

    fn wait_healthy(&self, port: u16, timeout: Duration) -> BackendFuture<Result<(), String>> {
        Box::pin(async move {
            wait_healthy(port, timeout)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn check_health(&self, port: u16) -> BackendFuture<bool> {
        let client = self.client.clone();
        Box::pin(async move {
            let url = format!("http://127.0.0.1:{port}/health");
            client
                .get(url)
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .map(|response| response.status().is_success())
                .unwrap_or(false)
        })
    }

    fn ram_info(&self) -> RamInfo {
        residency::read_ram_info()
    }

    fn rss_bytes(&self, pid: u32) -> Option<u64> {
        residency::read_rss_bytes(pid)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    Unloaded,
    Loading,
    Ready,
    Draining,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartDecision {
    DoNotRestart,
    RestartAfter(Duration),
    AttemptsExhausted,
}

#[derive(Debug, Clone)]
pub struct RestartPolicy {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub stable_after: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            stable_after: Duration::from_secs(60),
        }
    }
}

impl RestartPolicy {
    pub fn crash_decision(&self, hot: bool, prior_attempts: u32) -> RestartDecision {
        if !hot {
            return RestartDecision::DoNotRestart;
        }
        if prior_attempts >= self.max_attempts {
            return RestartDecision::AttemptsExhausted;
        }
        let exponent = prior_attempts.min(31);
        let multiplier = 1u32.checked_shl(exponent).unwrap_or(u32::MAX);
        let delay = self
            .initial_backoff
            .saturating_mul(multiplier)
            .min(self.max_backoff);
        RestartDecision::RestartAfter(delay)
    }
}

#[derive(Debug, Clone)]
pub struct SupervisorOptions {
    pub configured_headroom_gb: Option<f64>,
    pub load_timeout: Duration,
    pub poll_interval: Duration,
    pub restart_policy: RestartPolicy,
    pub extra_args: Vec<String>,
    pub engine_capabilities: EngineCapabilities,
    pub optimization_profile: OptimizationProfile,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            configured_headroom_gb: None,
            load_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_secs(5),
            restart_policy: RestartPolicy::default(),
            extra_args: Vec::new(),
            engine_capabilities: EngineCapabilities::default(),
            optimization_profile: OptimizationProfile::Balanced,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineSnapshot {
    pub canonical_id: String,
    pub lifecycle: LifecycleState,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub active_requests: usize,
    pub rss_bytes: Option<u64>,
    pub health: ModelHealth,
    pub hot: bool,
    pub last_used: Instant,
    pub restart_attempts: u32,
    pub failure: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorError {
    UnknownModel(String),
    CloudModel(String),
    InsufficientRam {
        canonical_id: String,
        shortfall_bytes: u64,
    },
    Backend(String),
    InvalidTransition {
        from: LifecycleState,
        to: LifecycleState,
    },
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownModel(id) => write!(formatter, "unknown model '{id}'"),
            Self::CloudModel(id) => {
                write!(formatter, "cloud model '{id}' has no local engine")
            }
            Self::InsufficientRam {
                canonical_id,
                shortfall_bytes,
            } => write!(
                formatter,
                "need {shortfall_bytes} more bytes to load '{canonical_id}'"
            ),
            Self::Backend(error) => formatter.write_str(error),
            Self::InvalidTransition { from, to } => {
                write!(formatter, "invalid engine transition {from:?} -> {to:?}")
            }
        }
    }
}

impl std::error::Error for SupervisorError {}

struct Activity {
    active_requests: AtomicUsize,
    last_used: SyncMutex<Instant>,
}

impl Activity {
    fn new(now: Instant) -> Self {
        Self {
            active_requests: AtomicUsize::new(0),
            last_used: SyncMutex::new(now),
        }
    }
}

struct RestartState {
    attempts: u32,
    next_attempt: Option<Instant>,
    ready_since: Option<Instant>,
}

impl RestartState {
    fn new() -> Self {
        Self {
            attempts: 0,
            next_attempt: None,
            ready_since: None,
        }
    }
}

struct ManagedEngine {
    lifecycle: LifecycleState,
    process: Option<Box<dyn EngineProcess>>,
    pid: Option<u32>,
    port: Option<u16>,
    estimated_bytes: u64,
    rss_bytes: Option<u64>,
    hot: bool,
    health: ModelHealth,
    activity: Arc<Activity>,
    restart: RestartState,
    failure: Option<String>,
}

impl ManagedEngine {
    fn unloaded(estimated_bytes: u64, hot: bool, now: Instant) -> Self {
        Self {
            lifecycle: LifecycleState::Unloaded,
            process: None,
            pid: None,
            port: None,
            estimated_bytes,
            rss_bytes: None,
            hot,
            health: ModelHealth::Unknown,
            activity: Arc::new(Activity::new(now)),
            restart: RestartState::new(),
            failure: None,
        }
    }

    fn transition(&mut self, next: LifecycleState) -> Result<(), SupervisorError> {
        if !valid_transition(self.lifecycle, next) {
            return Err(SupervisorError::InvalidTransition {
                from: self.lifecycle,
                to: next,
            });
        }
        self.lifecycle = next;
        Ok(())
    }

    fn resident_bytes(&self) -> u64 {
        self.rss_bytes.unwrap_or(0).max(self.estimated_bytes)
    }
}

fn valid_transition(from: LifecycleState, to: LifecycleState) -> bool {
    matches!(
        (from, to),
        (LifecycleState::Unloaded, LifecycleState::Loading)
            | (LifecycleState::Failed, LifecycleState::Loading)
            | (LifecycleState::Loading, LifecycleState::Ready)
            | (LifecycleState::Loading, LifecycleState::Failed)
            | (LifecycleState::Ready, LifecycleState::Draining)
            | (LifecycleState::Ready, LifecycleState::Failed)
            | (LifecycleState::Draining, LifecycleState::Unloaded)
            | (LifecycleState::Draining, LifecycleState::Failed)
            | (LifecycleState::Failed, LifecycleState::Unloaded)
    )
}

struct SupervisorInner {
    registry: Arc<ModelRegistry>,
    backend: Arc<dyn EngineBackend>,
    events: Arc<EventBus>,
    options: SupervisorOptions,
    engines: RwLock<HashMap<String, Arc<Mutex<ManagedEngine>>>>,
    model_locks: SyncMutex<HashMap<String, Arc<Mutex<()>>>>,
    profile_overrides: SyncRwLock<HashMap<String, OptimizationProfile>>,
    admission_lock: Mutex<()>,
}

#[derive(Clone)]
pub struct EngineSupervisor {
    inner: Arc<SupervisorInner>,
}

impl EngineSupervisor {
    pub fn new(
        registry: Arc<ModelRegistry>,
        backend: Arc<dyn EngineBackend>,
        events: Arc<EventBus>,
        options: SupervisorOptions,
    ) -> Self {
        Self {
            inner: Arc::new(SupervisorInner {
                registry,
                backend,
                events,
                options,
                engines: RwLock::new(HashMap::new()),
                model_locks: SyncMutex::new(HashMap::new()),
                profile_overrides: SyncRwLock::new(HashMap::new()),
                admission_lock: Mutex::new(()),
            }),
        }
    }

    pub fn production(
        registry: Arc<ModelRegistry>,
        events: Arc<EventBus>,
        llama_bin: impl Into<String>,
        port_range: (u16, u16),
        options: SupervisorOptions,
    ) -> Self {
        let backend = Arc::new(SystemEngineBackend::new(llama_bin, port_range));
        Self::new(registry, backend, events, options)
    }

    /// Keep the lease through every upstream response-body read.
    /// Dropping it makes an otherwise cold model evictable again.
    pub async fn acquire(&self, canonical_id: &str) -> Result<EngineLease, SupervisorError> {
        let model = self.model(canonical_id)?;
        if model.source == ModelSource::Cloud {
            return Err(SupervisorError::CloudModel(canonical_id.to_string()));
        }

        let model_lock = self.model_lock(canonical_id);
        let guard = model_lock.clone().lock_owned().await;
        if let Some(lease) = self.ready_lease(canonical_id).await {
            drop(guard);
            return Ok(lease);
        }
        if model.status == ModelStatus::Loaded {
            if let Some(port) = model.port {
                self.adopt_registered(canonical_id, &model, port).await;
                if let Some(lease) = self.ready_lease(canonical_id).await {
                    drop(guard);
                    return Ok(lease);
                }
            }
        }
        drop(guard);

        let admission = self.inner.admission_lock.lock().await;
        let guard = model_lock.lock_owned().await;
        if let Some(lease) = self.ready_lease(canonical_id).await {
            drop(guard);
            drop(admission);
            return Ok(lease);
        }

        self.ensure_ready(canonical_id, &model).await?;
        let lease = self.ready_lease(canonical_id).await.ok_or_else(|| {
            SupervisorError::Backend("engine became unavailable after loading".to_string())
        })?;
        drop(guard);
        drop(admission);
        Ok(lease)
    }

    pub async fn unload(&self, canonical_id: &str) -> Result<bool, SupervisorError> {
        let _admission = self.inner.admission_lock.lock().await;
        let _model = self.model_lock(canonical_id).lock_owned().await;
        self.drain(canonical_id, false).await
    }

    pub async fn set_hot(
        &self,
        canonical_id: &str,
        hot: bool,
    ) -> Result<(), SupervisorError> {
        self.model(canonical_id)?;
        self.inner.registry.update(canonical_id, |model| {
            model.hot = hot;
        });
        if let Some(runtime) = self.runtime(canonical_id).await {
            runtime.lock().await.hot = hot;
        }
        self.publish(
            canonical_id,
            "hot_changed",
            serde_json::json!({"hot": hot}),
        );
        Ok(())
    }

    pub fn set_optimization_profile(
        &self,
        canonical_id: &str,
        profile: OptimizationProfile,
    ) -> Result<(), SupervisorError> {
        self.model(canonical_id)?;
        self.inner
            .profile_overrides
            .write()
            .insert(canonical_id.to_string(), profile);
        self.publish(
            canonical_id,
            "optimization_profile_changed",
            serde_json::json!({"profile": profile.as_str()}),
        );
        Ok(())
    }

    pub async fn snapshot(&self, canonical_id: &str) -> Option<EngineSnapshot> {
        let runtime = self.runtime(canonical_id).await?;
        let state = runtime.lock().await;
        Some(snapshot_from(canonical_id, &state))
    }

    pub async fn snapshots(&self) -> Vec<EngineSnapshot> {
        let entries: Vec<_> = self
            .inner
            .engines
            .read()
            .await
            .iter()
            .map(|(id, runtime)| (id.clone(), runtime.clone()))
            .collect();
        let mut snapshots = Vec::with_capacity(entries.len());
        for (id, runtime) in entries {
            let state = runtime.lock().await;
            snapshots.push(snapshot_from(&id, &state));
        }
        snapshots.sort_by(|left, right| left.canonical_id.cmp(&right.canonical_id));
        snapshots
    }

    pub fn start_watcher(&self) -> SupervisorWatcher {
        let supervisor = self.clone();
        let interval = self.inner.options.poll_interval;
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                supervisor.watch_once().await;
            }
        });
        SupervisorWatcher { task: Some(task) }
    }

    pub async fn watch_once(&self) {
        let now = Instant::now();
        let entries: Vec<_> = self
            .inner
            .engines
            .read()
            .await
            .iter()
            .map(|(id, runtime)| (id.clone(), runtime.clone()))
            .collect();

        for (id, runtime) in entries {
            self.observe(&id, &runtime, now).await;
        }

        let due: Vec<_> = self
            .inner
            .engines
            .read()
            .await
            .iter()
            .map(|(id, runtime)| (id.clone(), runtime.clone()))
            .collect();
        for (id, runtime) in due {
            if self.take_due_restart(&runtime, now).await {
                self.restart(&id).await;
            }
        }
    }

    pub async fn shutdown_all(&self) {
        let _admission = self.inner.admission_lock.lock().await;
        let ids: Vec<_> = self.inner.engines.read().await.keys().cloned().collect();
        for id in ids {
            let _model = self.model_lock(&id).lock_owned().await;
            let _ = self.drain(&id, true).await;
        }
    }

    fn model(&self, canonical_id: &str) -> Result<crate::registry::ModelState, SupervisorError> {
        self.inner
            .registry
            .get_clone(canonical_id)
            .ok_or_else(|| SupervisorError::UnknownModel(canonical_id.to_string()))
    }

    fn model_lock(&self, canonical_id: &str) -> Arc<Mutex<()>> {
        self.inner
            .model_locks
            .lock()
            .entry(canonical_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn runtime(&self, canonical_id: &str) -> Option<Arc<Mutex<ManagedEngine>>> {
        self.inner.engines.read().await.get(canonical_id).cloned()
    }

    async fn runtime_for(
        &self,
        canonical_id: &str,
        estimated_bytes: u64,
        hot: bool,
    ) -> Arc<Mutex<ManagedEngine>> {
        if let Some(runtime) = self.runtime(canonical_id).await {
            let mut state = runtime.lock().await;
            state.estimated_bytes = estimated_bytes;
            state.hot = hot;
            drop(state);
            return runtime;
        }
        let runtime = Arc::new(Mutex::new(ManagedEngine::unloaded(
            estimated_bytes,
            hot,
            Instant::now(),
        )));
        self.inner
            .engines
            .write()
            .await
            .entry(canonical_id.to_string())
            .or_insert_with(|| runtime.clone())
            .clone()
    }

    async fn ready_lease(&self, canonical_id: &str) -> Option<EngineLease> {
        let runtime = self.runtime(canonical_id).await?;
        let state = runtime.lock().await;
        if state.lifecycle != LifecycleState::Ready {
            return None;
        }
        let port = state.port?;
        let activity = state.activity.clone();
        activity.active_requests.fetch_add(1, Ordering::AcqRel);
        *activity.last_used.lock() = Instant::now();
        let active = activity.active_requests.load(Ordering::Acquire);
        self.update_activity(canonical_id, active, Instant::now());
        Some(EngineLease {
            canonical_id: canonical_id.to_string(),
            port,
            activity,
            registry: Arc::downgrade(&self.inner.registry),
        })
    }

    async fn adopt_registered(
        &self,
        canonical_id: &str,
        model: &crate::registry::ModelState,
        port: u16,
    ) {
        let runtime = self
            .runtime_for(canonical_id, model.size_bytes, model.hot)
            .await;
        let mut state = runtime.lock().await;
        if state.lifecycle == LifecycleState::Unloaded {
            state.lifecycle = LifecycleState::Ready;
            state.pid = model.pid;
            state.port = Some(port);
            state.health = ModelHealth::Healthy;
            state.restart.ready_since = Some(Instant::now());
        }
    }

    async fn ensure_ready(
        &self,
        canonical_id: &str,
        model: &crate::registry::ModelState,
    ) -> Result<(), SupervisorError> {
        let runtime = self
            .runtime_for(canonical_id, model.size_bytes, model.hot)
            .await;
        self.make_room(canonical_id, model.size_bytes).await?;

        {
            let mut state = runtime.lock().await;
            state.transition(LifecycleState::Loading)?;
            state.failure = None;
            state.health = ModelHealth::Unknown;
        }
        self.update_registry(canonical_id, &runtime).await;
        self.publish(canonical_id, "loading", serde_json::json!({}));

        let port = match self.inner.backend.allocate_port() {
            Ok(port) => port,
            Err(error) => {
                return self.fail_load(canonical_id, &runtime, error).await;
            }
        };
        let request = SpawnRequest {
            canonical_id: canonical_id.to_string(),
            gguf_path: model.gguf_path.clone(),
            port,
            extra_args: self.spawn_args(canonical_id, model),
        };
        let process = match self.inner.backend.spawn(request).await {
            Ok(process) => process,
            Err(error) => {
                return self.fail_load(canonical_id, &runtime, error).await;
            }
        };
        let pid = process.pid();
        {
            let mut state = runtime.lock().await;
            state.pid = pid;
            state.port = Some(port);
            state.process = Some(process);
        }

        if let Err(error) = self
            .inner
            .backend
            .wait_healthy(port, self.inner.options.load_timeout)
            .await
        {
            let process = runtime.lock().await.process.take();
            if let Some(process) = process {
                process.shutdown().await;
            }
            return self.fail_load(canonical_id, &runtime, error).await;
        }

        {
            let mut state = runtime.lock().await;
            state.transition(LifecycleState::Ready)?;
            state.health = ModelHealth::Healthy;
            state.restart.ready_since = Some(Instant::now());
            state.restart.next_attempt = None;
            state.failure = None;
        }
        self.update_registry(canonical_id, &runtime).await;
        self.publish(
            canonical_id,
            "ready",
            serde_json::json!({"pid": pid, "port": port}),
        );
        Ok(())
    }

    fn spawn_args(
        &self,
        canonical_id: &str,
        model: &crate::registry::ModelState,
    ) -> Vec<String> {
        let mut args = self.inner.options.extra_args.clone();
        let Ok(metadata) = crate::gguf::read_metadata(&model.gguf_path) else {
            return args;
        };
        let ram = self.inner.backend.ram_info();
        let profile = self
            .inner
            .profile_overrides
            .read()
            .get(canonical_id)
            .copied()
            .unwrap_or(self.inner.options.optimization_profile);
        let request = OptimizationRequest {
            profile,
            hardware: hardware_profile(ram),
            model: ModelOptimizationMetadata::from_gguf(
                &metadata,
                model.size_bytes,
            ),
            ram_headroom_bytes: residency::headroom_bytes(
                ram.total_bytes,
                self.inner.options.configured_headroom_gb,
            ),
            requested_context: None,
            draft_model: None,
        };
        let planner = OptimizationPlanner::new(
            &self.inner.options.engine_capabilities,
        );
        match planner.plan(&request) {
            Ok(plan) => {
                let planned = plan.spawn_args().to_vec();
                self.publish(
                    canonical_id,
                    "optimized",
                    serde_json::json!({
                        "context_size": plan.context_size,
                        "gpu_layers": plan.gpu_layers,
                        "estimated_ram_bytes": plan.estimated_ram_bytes,
                        "memory_budget_bytes": plan.memory_budget_bytes,
                    }),
                );
                args.splice(0..0, planned);
            }
            Err(error) => {
                tracing::warn!(
                    "optimization skipped for {canonical_id}: {error}"
                );
            }
        }
        args
    }

    async fn make_room(
        &self,
        canonical_id: &str,
        new_model_bytes: u64,
    ) -> Result<(), SupervisorError> {
        let ram = self.inner.backend.ram_info();
        let headroom =
            residency::headroom_bytes(ram.total_bytes, self.inner.options.configured_headroom_gb);
        let mut resident = self.resident_bytes(canonical_id).await;
        let budget = ram.total_bytes.saturating_sub(headroom);
        if resident.saturating_add(new_model_bytes) <= budget {
            return Ok(());
        }

        let candidates = self.eviction_candidates(canonical_id).await;
        for candidate in candidates {
            let before = self.runtime_resident_bytes(&candidate.canonical_id).await;
            if self.drain(&candidate.canonical_id, false).await? {
                resident = resident.saturating_sub(before);
            }
            if resident.saturating_add(new_model_bytes) <= budget {
                return Ok(());
            }
        }

        let shortfall = resident
            .saturating_add(new_model_bytes)
            .saturating_sub(budget);
        Err(SupervisorError::InsufficientRam {
            canonical_id: canonical_id.to_string(),
            shortfall_bytes: shortfall,
        })
    }

    async fn resident_bytes(&self, excluded: &str) -> u64 {
        let entries: Vec<_> = self
            .inner
            .engines
            .read()
            .await
            .iter()
            .filter(|(id, _)| id.as_str() != excluded)
            .map(|(_, runtime)| runtime.clone())
            .collect();
        let mut bytes = 0u64;
        for runtime in entries {
            let state = runtime.lock().await;
            if matches!(
                state.lifecycle,
                LifecycleState::Loading | LifecycleState::Ready
            ) {
                bytes = bytes.saturating_add(state.resident_bytes());
            }
        }
        bytes
    }

    async fn runtime_resident_bytes(&self, canonical_id: &str) -> u64 {
        let Some(runtime) = self.runtime(canonical_id).await else {
            return 0;
        };
        let state = runtime.lock().await;
        state.resident_bytes()
    }

    async fn eviction_candidates(&self, excluded: &str) -> Vec<ResidencyCandidate> {
        let entries: Vec<_> = self
            .inner
            .engines
            .read()
            .await
            .iter()
            .filter(|(id, _)| id.as_str() != excluded)
            .map(|(id, runtime)| (id.clone(), runtime.clone()))
            .collect();
        let mut candidates = Vec::with_capacity(entries.len());
        for (id, runtime) in entries {
            let mut state = runtime.lock().await;
            state.hot = self
                .inner
                .registry
                .get_clone(&id)
                .map(|model| model.hot)
                .unwrap_or(state.hot);
            candidates.push(ResidencyCandidate {
                canonical_id: id,
                hot: state.hot,
                active_requests: state.activity.active_requests.load(Ordering::Acquire),
                ready: state.lifecycle == LifecycleState::Ready,
                last_used: *state.activity.last_used.lock(),
            });
        }
        residency::lru_eviction_order(candidates)
    }

    async fn drain(&self, canonical_id: &str, force: bool) -> Result<bool, SupervisorError> {
        let Some(runtime) = self.runtime(canonical_id).await else {
            return Ok(false);
        };
        let process = {
            let mut state = runtime.lock().await;
            if state.lifecycle != LifecycleState::Ready {
                return Ok(false);
            }
            state.hot = self
                .inner
                .registry
                .get_clone(canonical_id)
                .map(|model| model.hot)
                .unwrap_or(state.hot);
            let active = state.activity.active_requests.load(Ordering::Acquire);
            if active > 0 && !force {
                return Ok(false);
            }
            if state.hot && !force {
                return Ok(false);
            }
            state.transition(LifecycleState::Draining)?;
            state.health = ModelHealth::Unknown;
            state.process.take()
        };
        self.update_registry(canonical_id, &runtime).await;
        self.publish(canonical_id, "draining", serde_json::json!({}));
        if let Some(process) = process {
            process.shutdown().await;
        }
        {
            let mut state = runtime.lock().await;
            state.transition(LifecycleState::Unloaded)?;
            state.pid = None;
            state.port = None;
            state.rss_bytes = None;
            state.restart.ready_since = None;
            state.failure = None;
        }
        self.update_registry(canonical_id, &runtime).await;
        self.publish(canonical_id, "unloaded", serde_json::json!({}));
        Ok(true)
    }

    async fn fail_load(
        &self,
        canonical_id: &str,
        runtime: &Arc<Mutex<ManagedEngine>>,
        error: String,
    ) -> Result<(), SupervisorError> {
        {
            let mut state = runtime.lock().await;
            state.transition(LifecycleState::Failed)?;
            state.pid = None;
            state.port = None;
            state.health = ModelHealth::Unhealthy;
            state.failure = Some(error.clone());
            self.schedule_restart(&mut state, Instant::now());
        }
        self.update_registry(canonical_id, runtime).await;
        self.publish(canonical_id, "failed", serde_json::json!({"reason": error}));
        Err(SupervisorError::Backend(error))
    }

    async fn observe(&self, canonical_id: &str, runtime: &Arc<Mutex<ManagedEngine>>, now: Instant) {
        let observed = {
            let mut state = runtime.lock().await;
            if state.lifecycle != LifecycleState::Ready {
                return;
            }
            state.hot = self
                .inner
                .registry
                .get_clone(canonical_id)
                .map(|model| model.hot)
                .unwrap_or(state.hot);
            let exit = state
                .process
                .as_mut()
                .and_then(|process| process.try_wait().ok())
                .flatten();
            if let Some(exit) = exit {
                state.process = None;
                let reason = match exit.code {
                    Some(code) => format!("engine exited with code {code}"),
                    None => "engine exited without a status code".to_string(),
                };
                let _ = state.transition(LifecycleState::Failed);
                state.pid = None;
                state.port = None;
                state.rss_bytes = None;
                state.health = ModelHealth::Unhealthy;
                state.failure = Some(reason.clone());
                self.schedule_restart(&mut state, now);
                Some(Err(reason))
            } else {
                state.port.zip(state.pid).map(Ok)
            }
        };

        match observed {
            Some(Err(reason)) => {
                self.update_registry(canonical_id, runtime).await;
                self.publish(
                    canonical_id,
                    "crashed",
                    serde_json::json!({"reason": reason}),
                );
            }
            Some(Ok((port, pid))) => {
                let healthy = self.inner.backend.check_health(port).await;
                let rss = self.inner.backend.rss_bytes(pid);
                let mut state = runtime.lock().await;
                if state.lifecycle != LifecycleState::Ready || state.pid != Some(pid) {
                    return;
                }
                state.health = if healthy {
                    ModelHealth::Healthy
                } else {
                    ModelHealth::Unhealthy
                };
                state.rss_bytes = rss;
                if healthy
                    && state.restart.ready_since.is_some_and(|ready| {
                        now.saturating_duration_since(ready)
                            >= self.inner.options.restart_policy.stable_after
                    })
                {
                    state.restart = RestartState::new();
                    state.restart.ready_since = Some(now);
                }
                drop(state);
                self.update_registry(canonical_id, runtime).await;
            }
            None => {}
        }
    }

    fn schedule_restart(&self, state: &mut ManagedEngine, now: Instant) {
        match self
            .inner
            .options
            .restart_policy
            .crash_decision(state.hot, state.restart.attempts)
        {
            RestartDecision::RestartAfter(delay) => {
                state.restart.attempts += 1;
                state.restart.next_attempt = Some(now + delay);
            }
            RestartDecision::DoNotRestart | RestartDecision::AttemptsExhausted => {
                state.restart.next_attempt = None;
            }
        }
        state.restart.ready_since = None;
    }

    async fn take_due_restart(&self, runtime: &Arc<Mutex<ManagedEngine>>, now: Instant) -> bool {
        let mut state = runtime.lock().await;
        if state.lifecycle != LifecycleState::Failed {
            return false;
        }
        let due = state
            .restart
            .next_attempt
            .is_some_and(|attempt| attempt <= now);
        if due {
            state.restart.next_attempt = None;
        }
        due
    }

    async fn restart(&self, canonical_id: &str) {
        let Ok(model) = self.model(canonical_id) else {
            return;
        };
        let _admission = self.inner.admission_lock.lock().await;
        let _model = self.model_lock(canonical_id).lock_owned().await;
        self.publish(canonical_id, "restarting", serde_json::json!({}));
        let _ = self.ensure_ready(canonical_id, &model).await;
    }

    async fn update_registry(&self, canonical_id: &str, runtime: &Arc<Mutex<ManagedEngine>>) {
        let state = runtime.lock().await;
        let status = match state.lifecycle {
            LifecycleState::Unloaded => ModelStatus::Unloaded,
            LifecycleState::Loading => ModelStatus::Loading,
            LifecycleState::Ready => ModelStatus::Loaded,
            LifecycleState::Draining => ModelStatus::Draining,
            LifecycleState::Failed => {
                ModelStatus::Failed(state.failure.clone().unwrap_or_default())
            }
        };
        let active = state.activity.active_requests.load(Ordering::Acquire);
        let last_used = *state.activity.last_used.lock();
        self.inner.registry.update(canonical_id, |model| {
            model.status = status;
            model.pid = state.pid;
            model.port = state.port;
            model.rss_bytes = state.rss_bytes;
            model.health = state.health;
            model.active_requests = active;
            model.last_used = last_used;
        });
    }

    fn update_activity(&self, canonical_id: &str, active: usize, last_used: Instant) {
        self.inner.registry.update(canonical_id, |model| {
            model.active_requests = active;
            model.last_used = last_used;
        });
    }

    fn publish(&self, canonical_id: &str, action: &str, data: serde_json::Value) {
        self.inner.events.publish(
            EventTopic::Engine,
            action,
            Some(canonical_id.to_string()),
            data,
        );
    }
}

fn snapshot_from(canonical_id: &str, state: &ManagedEngine) -> EngineSnapshot {
    EngineSnapshot {
        canonical_id: canonical_id.to_string(),
        lifecycle: state.lifecycle,
        pid: state.pid,
        port: state.port,
        active_requests: state.activity.active_requests.load(Ordering::Acquire),
        rss_bytes: state.rss_bytes,
        health: state.health,
        hot: state.hot,
        last_used: *state.activity.last_used.lock(),
        restart_attempts: state.restart.attempts,
        failure: state.failure.clone(),
    }
}

fn hardware_profile(ram: RamInfo) -> HardwareProfile {
    let logical = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let system = sysinfo::System::new_all();
    let physical = system.physical_core_count().unwrap_or(logical).max(1);
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    let gpu = Some(GpuHardware {
        memory_bytes: ram.total_bytes,
        unified_memory: true,
    });
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    let gpu: Option<GpuHardware> = None;
    HardwareProfile {
        total_ram_bytes: ram.total_bytes,
        available_ram_bytes: ram.available_bytes,
        physical_cpu_cores: physical,
        logical_cpu_cores: logical,
        gpu,
    }
}

/// A model remains ineligible for eviction while this value lives.
pub struct EngineLease {
    canonical_id: String,
    port: u16,
    activity: Arc<Activity>,
    registry: Weak<ModelRegistry>,
}

impl EngineLease {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn canonical_id(&self) -> &str {
        &self.canonical_id
    }
}

impl Drop for EngineLease {
    fn drop(&mut self) {
        let prior = self.activity.active_requests.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prior > 0);
        let now = Instant::now();
        *self.activity.last_used.lock() = now;
        if let Some(registry) = self.registry.upgrade() {
            registry.update(&self.canonical_id, |model| {
                model.active_requests = prior.saturating_sub(1);
                model.last_used = now;
            });
        }
    }
}

pub struct SupervisorWatcher {
    task: Option<JoinHandle<()>>,
}

impl SupervisorWatcher {
    pub async fn stop(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for SupervisorWatcher {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32};

    use super::*;
    use crate::state::ModelManifest;

    struct FakeProcess {
        pid: u32,
        exited: Arc<AtomicBool>,
        shutdowns: Arc<AtomicUsize>,
    }

    impl EngineProcess for FakeProcess {
        fn pid(&self) -> Option<u32> {
            Some(self.pid)
        }

        fn try_wait(&mut self) -> Result<Option<ProcessExit>, String> {
            Ok(self
                .exited
                .load(Ordering::Acquire)
                .then_some(ProcessExit { code: Some(1) }))
        }

        fn shutdown(self: Box<Self>) -> BackendFuture<()> {
            Box::pin(async move {
                self.shutdowns.fetch_add(1, Ordering::AcqRel);
            })
        }
    }

    struct FakeEngineBackend {
        spawn_count: AtomicUsize,
        shutdowns: Arc<AtomicUsize>,
        next_pid: AtomicU32,
        next_port: AtomicU16,
        exited: SyncMutex<HashMap<u32, Arc<AtomicBool>>>,
        total_ram: u64,
        spawn_delay: Duration,
    }

    impl FakeEngineBackend {
        fn new(total_ram: u64) -> Self {
            Self {
                spawn_count: AtomicUsize::new(0),
                shutdowns: Arc::new(AtomicUsize::new(0)),
                next_pid: AtomicU32::new(1000),
                next_port: AtomicU16::new(8700),
                exited: SyncMutex::new(HashMap::new()),
                total_ram,
                spawn_delay: Duration::from_millis(10),
            }
        }

        fn crash(&self, pid: u32) {
            self.exited
                .lock()
                .get(&pid)
                .unwrap()
                .store(true, Ordering::Release);
        }
    }

    impl EngineBackend for FakeEngineBackend {
        fn allocate_port(&self) -> Result<u16, String> {
            Ok(self.next_port.fetch_add(1, Ordering::AcqRel))
        }

        fn spawn(
            &self,
            _request: SpawnRequest,
        ) -> BackendFuture<Result<Box<dyn EngineProcess>, String>> {
            self.spawn_count.fetch_add(1, Ordering::AcqRel);
            let pid = self.next_pid.fetch_add(1, Ordering::AcqRel);
            let exited = Arc::new(AtomicBool::new(false));
            self.exited.lock().insert(pid, exited.clone());
            let shutdowns = self.shutdowns.clone();
            let delay = self.spawn_delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                Ok(Box::new(FakeProcess {
                    pid,
                    exited,
                    shutdowns,
                }) as Box<dyn EngineProcess>)
            })
        }

        fn wait_healthy(
            &self,
            _port: u16,
            _timeout: Duration,
        ) -> BackendFuture<Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn check_health(&self, _port: u16) -> BackendFuture<bool> {
            Box::pin(async { true })
        }

        fn ram_info(&self) -> RamInfo {
            RamInfo {
                total_bytes: self.total_ram,
                available_bytes: self.total_ram,
            }
        }

        fn rss_bytes(&self, _pid: u32) -> Option<u64> {
            Some(10)
        }
    }

    fn model(id: &str, size_bytes: u64, hot: bool) -> crate::registry::ModelState {
        let manifest = ModelManifest {
            canonical_id: id.to_string(),
            filename: format!("{id}.gguf"),
            sha256: String::new(),
            size_bytes,
            architecture: "llama".to_string(),
            is_moe: false,
            expert_count: None,
            context_length_trained: 4096,
            vision_capable: false,
            source_ref: String::new(),
            pulled_at: chrono::Utc::now(),
        };
        let mut model = crate::registry::ModelState::from_manifest(&manifest, hot);
        model.gguf_path = PathBuf::from(manifest.filename);
        model
    }

    fn supervisor(
        registry: Arc<ModelRegistry>,
        backend: Arc<FakeEngineBackend>,
        restart_policy: RestartPolicy,
    ) -> EngineSupervisor {
        EngineSupervisor::new(
            registry,
            backend,
            Arc::new(EventBus::new(64)),
            SupervisorOptions {
                configured_headroom_gb: Some(0.0),
                restart_policy,
                ..SupervisorOptions::default()
            },
        )
    }

    #[tokio::test]
    async fn concurrent_acquisition_spawns_one_engine() {
        let registry = Arc::new(ModelRegistry::new());
        registry.insert(model("model-a", 100, false));
        let backend = Arc::new(FakeEngineBackend::new(10_000));
        let supervisor = supervisor(registry, backend.clone(), RestartPolicy::default());

        let mut tasks = Vec::new();
        for _ in 0..20 {
            let supervisor = supervisor.clone();
            tasks.push(tokio::spawn(async move {
                let lease = supervisor.acquire("model-a").await.unwrap();
                assert_eq!(lease.port(), 8700);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(backend.spawn_count.load(Ordering::Acquire), 1);
        let snapshot = supervisor.snapshot("model-a").await.unwrap();
        assert_eq!(snapshot.lifecycle, LifecycleState::Ready);
        assert_eq!(snapshot.active_requests, 0);
    }

    #[test]
    fn model_profile_overrides_are_scoped_and_validated() {
        let registry = Arc::new(ModelRegistry::new());
        registry.insert(model("model-a", 100, false));
        let backend = Arc::new(FakeEngineBackend::new(10_000));
        let supervisor = supervisor(registry, backend, RestartPolicy::default());

        supervisor
            .set_optimization_profile("model-a", OptimizationProfile::Latency)
            .unwrap();

        assert_eq!(
            supervisor
                .inner
                .profile_overrides
                .read()
                .get("model-a")
                .copied(),
            Some(OptimizationProfile::Latency)
        );
        assert!(matches!(
            supervisor.set_optimization_profile(
                "missing",
                OptimizationProfile::Memory
            ),
            Err(SupervisorError::UnknownModel(_))
        ));
    }

    #[test]
    fn lifecycle_accepts_only_documented_transitions() {
        let now = Instant::now();
        let mut state = ManagedEngine::unloaded(100, false, now);
        state.transition(LifecycleState::Loading).unwrap();
        state.transition(LifecycleState::Ready).unwrap();
        state.transition(LifecycleState::Draining).unwrap();
        state.transition(LifecycleState::Unloaded).unwrap();

        assert_eq!(
            state.transition(LifecycleState::Ready),
            Err(SupervisorError::InvalidTransition {
                from: LifecycleState::Unloaded,
                to: LifecycleState::Ready,
            })
        );
    }

    #[test]
    fn restart_policy_is_exponential_and_bounded() {
        let policy = RestartPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_secs(2),
            max_backoff: Duration::from_secs(5),
            stable_after: Duration::from_secs(60),
        };

        assert_eq!(
            policy.crash_decision(false, 0),
            RestartDecision::DoNotRestart
        );
        assert_eq!(
            policy.crash_decision(true, 0),
            RestartDecision::RestartAfter(Duration::from_secs(2))
        );
        assert_eq!(
            policy.crash_decision(true, 1),
            RestartDecision::RestartAfter(Duration::from_secs(4))
        );
        assert_eq!(
            policy.crash_decision(true, 2),
            RestartDecision::RestartAfter(Duration::from_secs(5))
        );
        assert_eq!(
            policy.crash_decision(true, 3),
            RestartDecision::AttemptsExhausted
        );
    }

    #[tokio::test]
    async fn watcher_restarts_hot_crashes_until_limit() {
        let registry = Arc::new(ModelRegistry::new());
        registry.insert(model("hot-model", 100, true));
        let backend = Arc::new(FakeEngineBackend::new(10_000));
        let policy = RestartPolicy {
            max_attempts: 2,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            stable_after: Duration::from_secs(60),
        };
        let supervisor = supervisor(registry, backend.clone(), policy);

        drop(supervisor.acquire("hot-model").await.unwrap());
        let first_pid = supervisor.snapshot("hot-model").await.unwrap().pid.unwrap();
        backend.crash(first_pid);
        supervisor.watch_once().await;
        assert_eq!(backend.spawn_count.load(Ordering::Acquire), 2);

        let second_pid = supervisor.snapshot("hot-model").await.unwrap().pid.unwrap();
        backend.crash(second_pid);
        supervisor.watch_once().await;
        assert_eq!(backend.spawn_count.load(Ordering::Acquire), 3);

        let third_pid = supervisor.snapshot("hot-model").await.unwrap().pid.unwrap();
        backend.crash(third_pid);
        supervisor.watch_once().await;
        assert_eq!(backend.spawn_count.load(Ordering::Acquire), 3);
        assert_eq!(
            supervisor.snapshot("hot-model").await.unwrap().lifecycle,
            LifecycleState::Failed
        );
    }

    #[tokio::test]
    async fn active_and_hot_models_are_not_evicted() {
        let registry = Arc::new(ModelRegistry::new());
        registry.insert(model("active", 400, false));
        registry.insert(model("hot", 400, false));
        registry.insert(model("incoming", 400, false));
        let backend = Arc::new(FakeEngineBackend::new(1_000));
        let supervisor = supervisor(registry.clone(), backend, RestartPolicy::default());

        let active = supervisor.acquire("active").await.unwrap();
        drop(supervisor.acquire("hot").await.unwrap());
        registry.update("hot", |model| model.hot = true);
        let result = supervisor.acquire("incoming").await;

        assert!(matches!(
            result,
            Err(SupervisorError::InsufficientRam { .. })
        ));
        assert_eq!(
            supervisor.snapshot("active").await.unwrap().lifecycle,
            LifecycleState::Ready
        );
        assert_eq!(
            supervisor.snapshot("hot").await.unwrap().lifecycle,
            LifecycleState::Ready
        );
        drop(active);
    }
}
