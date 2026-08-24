//! Entry point. Only wires the OpenAI-compatible HTTP surface (SPEC §2.1)
//! for this worktree — `cli.rs`/`config.rs`/`state.rs`/`residency.rs`/
//! `pull.rs` and the Unix control socket (§2, §7) are future passes.

mod api;
mod engine;
mod registry;

use std::sync::Arc;

use engine::EngineSupervisor;

#[tokio::main]
async fn main() {
    let engine = Arc::new(EngineSupervisor::new());
    let app = api::router(engine);

    let addr = "127.0.0.1:11535";
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    println!("nodelm listening on http://{addr}");
    axum::serve(listener, app).await.unwrap();
}
