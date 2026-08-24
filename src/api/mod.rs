pub mod anthropic;
pub mod openai;

use std::sync::Arc;

use crate::registry::ModelRegistry;
use crate::residency::Residency;

/// Shared handler state. `internal.rs` (§2.5, not part of this task) would
/// carry its own additional bits (config, state.json handle); this is only
/// what the Anthropic route needs.
#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<ModelRegistry>,
    pub residency: Arc<Residency>,
    pub http: reqwest::Client,
}
