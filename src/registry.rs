//! In-memory model registry (SPEC.md §4.2), minimal fields for the OpenAI
//! surface. Real pull/persistence (state.json, manifest sidecars, §3) is
//! out of scope for this worktree — see the stub note in engine.rs.

/// Mirrors `ModelStatus` from SPEC §4.2, plus the permanent
/// unsupported-architecture tag from §5.2 (kept as its own variant here
/// since this stub has no separate verdict_cache). Only `Loaded` and
/// `UnsupportedArchitecture` are reachable until residency.rs/engine.rs
/// exist and drive real state transitions — the rest are dead for now.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    Unloaded,
    Loading,
    Loaded,
    Failed(String),
    UnsupportedArchitecture(String),
}

#[derive(Debug, Clone)]
pub struct ModelState {
    pub canonical_id: String,
    /// Unused by this stub's handlers (no /api/status here yet); kept so
    /// the struct already matches SPEC §4.2's real field set.
    #[allow(dead_code)]
    pub size_bytes: u64,
    /// Unix epoch seconds, for /v1/models "created".
    pub pulled_at: i64,
    pub status: ModelStatus,
    /// Loopback port of the child llama-server, set only when Loaded.
    pub port: Option<u16>,
}
