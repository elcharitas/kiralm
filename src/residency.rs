//! Admission control, hot/cold + LRU eviction policy, headroom math, and
//! the RSS watcher background task. SPEC.md §5.
//!
//! RAM sourcing note (resolves a real ambiguity in SPEC.md §5.1 vs §5.2):
//! the headroom-tier pick (2GB vs 1.5GB) is about fixed hardware capacity,
//! so it uses `sysinfo::System::total_memory()`. Every other "total_ram"
//! term in the spec's admission/ratio formulas is fed by the *live*
//! `.available_memory()` reading per §5.1's explicit instruction ("RAM
//! read via ... .available_memory() ... refreshed on-demand before every
//! load decision") — that live-refreshed number is what actually varies
//! and is what admission must react to.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio::process::Child;
use tokio::sync::Mutex as AsyncMutex;

use crate::config::Config;
use crate::engine::{self, SpawnRequest};
use crate::registry::{compute_ram_verdict, CanonicalId, ModelRegistry, ModelStatus, Verdict};
use crate::state::{PersistedModelState, State, VerdictCache};

const WATCHER_TICK: Duration = Duration::from_secs(5);
const STARTUP_POLL: Duration = Duration::from_millis(250);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    #[error("need {needed_bytes} more bytes ({available_bytes} available); evict a hot model manually or lower headroom")]
    InsufficientRoom { needed_bytes: u64, available_bytes: u64 },
    #[error("architecture unsupported on this engine build: {0}")]
    UnsupportedArchitecture(String),
    #[error("model verdict is NOT_RECOMMENDED on this hardware; retry with force to override")]
    NotRecommended,
}

fn human_bytes(b: u64) -> String {
    format!("{:.2}GB", b as f64 / (1024.0 * 1024.0 * 1024.0))
}

pub struct RamSnapshot {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

pub struct ResidencyManager {
    pub registry: Arc<ModelRegistry>,
    pub config: Config,
    pub nodelm_home: PathBuf,
    state_path: PathBuf,
    state: StdMutex<State>,
    children: AsyncMutex<HashMap<CanonicalId, Child>>,
    sys: AsyncMutex<System>,
}

impl ResidencyManager {
    pub fn new(registry: Arc<ModelRegistry>, config: Config, nodelm_home: PathBuf, state: State, state_path: PathBuf) -> Self {
        ResidencyManager {
            registry,
            config,
            nodelm_home,
            state_path,
            state: StdMutex::new(state),
            children: AsyncMutex::new(HashMap::new()),
            sys: AsyncMutex::new(System::new()),
        }
    }

    async fn ram_snapshot(&self) -> RamSnapshot {
        let mut sys = self.sys.lock().await;
        sys.refresh_memory();
        RamSnapshot { total_bytes: sys.total_memory(), available_bytes: sys.available_memory() }
    }

    /// Live budget cap: available RAM right now, minus the (fixed-tier)
    /// headroom. SPEC.md §5.1/§5.4.
    async fn usable_budget_bytes(&self) -> u64 {
        let snap = self.ram_snapshot().await;
        let headroom = self.config.headroom_bytes(snap.total_bytes);
        snap.available_bytes.saturating_sub(headroom)
    }

    /// Live RAM-tier verdict for one model (does not layer in a cached
    /// UNSUPPORTED_ARCHITECTURE verdict — caller merges that in).
    pub async fn compute_verdict(&self, size_bytes: u64, is_moe: bool) -> Verdict {
        let snap = self.ram_snapshot().await;
        let headroom = self.config.headroom_bytes(snap.total_bytes);
        compute_ram_verdict(size_bytes, is_moe, snap.available_bytes, headroom)
    }

    /// Pre-spawn admission check. SPEC.md §5.4.
    async fn admit(&self, size_bytes: u64, exclude: Option<&str>) -> Result<(), AdmissionError> {
        let cap = self.usable_budget_bytes().await;
        let needed = self.registry.loaded_size_sum(exclude) + size_bytes;
        if needed > cap {
            return Err(AdmissionError::InsufficientRoom { needed_bytes: needed - cap, available_bytes: cap });
        }
        Ok(())
    }

    /// SPEC.md §2.5/§5.4: hot-toggle runs the same admission check against
    /// the sum of *hot* models, structurally guaranteeing hot models never
    /// need eviction later. Turning off never kills a running model.
    pub async fn set_hot(&self, id: &str, on: bool) -> Result<()> {
        let model = self.registry.get(id).context("unknown model")?;
        if on {
            let cap = self.usable_budget_bytes().await;
            let needed = self.registry.hot_size_sum(Some(id)) + model.size_bytes;
            if needed > cap {
                bail!(AdmissionError::InsufficientRoom { needed_bytes: needed - cap, available_bytes: cap });
            }
        }
        self.registry.set_hot(id, on);
        self.persist_hot(id, on)?;
        Ok(())
    }

    fn persist_hot(&self, id: &str, hot: bool) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let entry = state.models.entry(id.to_string()).or_insert_with(|| PersistedModelState {
            hot,
            last_used: Utc::now(),
            verdict_cache: None,
        });
        entry.hot = hot;
        state.save(&self.state_path)
    }

    fn persist_last_used(&self, id: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let hot = self.registry.get(id).map(|m| m.hot).unwrap_or(false);
        let entry = state.models.entry(id.to_string()).or_insert_with(|| PersistedModelState {
            hot,
            last_used: Utc::now(),
            verdict_cache: None,
        });
        entry.last_used = Utc::now();
        state.save(&self.state_path)
    }

    pub fn persist_verdict_cache(&self, id: &str, cache: VerdictCache) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let hot = self.registry.get(id).map(|m| m.hot).unwrap_or(false);
        let entry = state.models.entry(id.to_string()).or_insert_with(|| PersistedModelState {
            hot,
            last_used: Utc::now(),
            verdict_cache: None,
        });
        entry.verdict_cache = Some(cache);
        state.save(&self.state_path)
    }

    fn ports_in_use(&self) -> HashSet<u16> {
        self.registry
            .all_ids()
            .into_iter()
            .filter_map(|id| self.registry.get(&id).and_then(|m| m.port))
            .collect()
    }

    /// Full load orchestration: verdict gate, admission, spawn, bounded
    /// post-spawn health+RSS watch. SPEC.md §4.4/§5.4.
    pub async fn load_model(&self, id: &str, force: bool) -> Result<()> {
        let model = self.registry.get(id).context("unknown model")?;
        if model.is_loaded() {
            return Ok(());
        }
        if let Verdict::UnsupportedArchitecture(err) = &model.verdict {
            bail!(AdmissionError::UnsupportedArchitecture(err.clone()));
        }
        if matches!(model.verdict, Verdict::NotRecommended) && !force {
            bail!(AdmissionError::NotRecommended);
        }

        self.admit(model.size_bytes, None).await?;
        self.registry.set_status(id, ModelStatus::Loading);

        let snap = self.ram_snapshot().await;
        let headroom = self.config.headroom_bytes(snap.total_bytes);
        let req = SpawnRequest {
            canonical_id: id.to_string(),
            gguf_path: model.gguf_path.clone(),
            is_moe: model.is_moe,
            size_bytes: model.size_bytes,
            context_length_trained: model.context_length_trained,
            hot: model.hot,
            draft_model_path: None,
        };

        let outcome = match engine::spawn(&req, &self.config, &self.nodelm_home, snap.available_bytes, headroom, &self.ports_in_use()).await {
            Ok(o) => o,
            Err(e) => {
                self.registry.set_status(id, ModelStatus::Failed(e.to_string()));
                return Err(e);
            }
        };

        let mut child = outcome.child;
        let (port, pid) = (outcome.port, outcome.pid);
        let cap = snap.available_bytes.saturating_sub(headroom);

        match self.watch_startup(pid, port, cap).await {
            Ok(()) => {
                self.children.lock().await.insert(id.to_string(), child);
                self.registry.mark_loaded(id, pid, port, model.hot);
                let _ = self.persist_last_used(id);
                Ok(())
            }
            Err(e) => {
                engine::kill_now(&mut child).await;
                let log = engine::tail_log(&engine::log_path(&self.nodelm_home, id), 50);
                let msg = if log.is_empty() { e.to_string() } else { format!("{e}\n--- log tail ---\n{log}") };
                self.registry.set_status(id, ModelStatus::Failed(msg.clone()));
                bail!(msg)
            }
        }
    }

    async fn watch_startup(&self, candidate_pid: u32, port: u16, cap_bytes: u64) -> Result<()> {
        let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
        loop {
            if engine::health_check_once(port).await {
                return Ok(());
            }
            let rss = self.rss_sum(Some(candidate_pid)).await;
            if rss > cap_bytes {
                bail!("resident memory during startup exceeded budget ({} > {})", human_bytes(rss), human_bytes(cap_bytes));
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("health check timed out after {:?}", STARTUP_TIMEOUT);
            }
            tokio::time::sleep(STARTUP_POLL).await;
        }
    }

    async fn rss_sum(&self, extra_pid: Option<u32>) -> u64 {
        let mut pids: Vec<Pid> = self.registry.loaded_pids().into_iter().map(|(_, pid)| Pid::from_u32(pid)).collect();
        if let Some(pid) = extra_pid {
            pids.push(Pid::from_u32(pid));
        }
        let mut sys = self.sys.lock().await;
        sys.refresh_processes(ProcessesToUpdate::Some(&pids), true);
        pids.iter().filter_map(|p| sys.process(*p)).map(|p| p.memory()).sum()
    }

    async fn evict(&self, id: &str) {
        if let Some(mut child) = self.children.lock().await.remove(id) {
            if let Some(pid) = self.registry.get(id).and_then(|m| m.pid) {
                engine::shutdown_gracefully(pid, &mut child).await;
            } else {
                engine::kill_now(&mut child).await;
            }
        }
        self.registry.mark_unloaded(id);
    }

    /// One enforcement pass: evict cold models in strict LRU order,
    /// re-checking after each, until under budget or no cold models
    /// remain. Hot models are never evicted here, full stop. SPEC.md §5.4.
    pub async fn enforce_budget(&self) {
        let cap = self.usable_budget_bytes().await;
        loop {
            let rss = self.rss_sum(None).await;
            if rss <= cap {
                return;
            }
            let cold = self.registry.cold_loaded_by_lru();
            match cold.first() {
                Some(victim) => self.evict(victim).await,
                None => {
                    eprintln!(
                        "[nodelm] WARNING: resident memory ({}) exceeds budget ({}) with only hot models left; not evicting hot models",
                        human_bytes(rss),
                        human_bytes(cap)
                    );
                    return;
                }
            }
        }
    }

    pub fn spawn_watcher(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(WATCHER_TICK);
            loop {
                interval.tick().await;
                self.enforce_budget().await;
            }
        })
    }

    /// Shutdown (daemon exit): SIGTERM/SIGKILL every resident child.
    pub async fn shutdown_all(&self) {
        let ids: Vec<CanonicalId> = self.children.lock().await.keys().cloned().collect();
        for id in ids {
            self.evict(&id).await;
        }
    }
}
