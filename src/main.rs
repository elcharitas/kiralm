//! Entry point. Full CLI dispatch (`cli.rs`) and the HTTP surface
//! (`api/`) are separate build tracks; this wires up config, state, the
//! model registry, and the residency manager/watcher — the daemon core.
//!
//! Several public methods below (hot-toggle, verdict cache, manifest
//! read/write) have no caller yet in this partial build — `api/` and
//! `pull.rs` are the consumers, built on other branches.
#![allow(dead_code)]

mod config;
mod engine;
mod registry;
mod residency;
mod state;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use config::Config;
use registry::ModelRegistry;
use residency::ResidencyManager;
use state::State;

fn nodelm_home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".nodelm")
}

#[tokio::main]
async fn main() -> Result<()> {
    let home = nodelm_home();
    std::fs::create_dir_all(&home)?;

    let config = Config::load(&home.join("config.toml"))?;
    let state_path = home.join("state.json");
    let state = State::load(&state_path)?;

    let registry = Arc::new(ModelRegistry::new());
    let residency = Arc::new(ResidencyManager::new(registry.clone(), config, home.clone(), state, state_path));
    let _watcher = residency.clone().spawn_watcher();

    println!("nodelm daemon core up (home={})", home.display());

    tokio::signal::ctrl_c().await?;
    residency.shutdown_all().await;
    Ok(())
}
