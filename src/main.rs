//! `nodelm` binary entry point: arg parse -> dispatch to `cli.rs` or the
//! `serve` daemon (§1, §7).

mod api;
mod cli;
mod config;
mod engine;
mod gguf;
mod pull;
mod registry;
mod residency;
mod state;

use std::sync::Arc;

use clap::{Parser, Subcommand};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use hyper_util::service::TowerToHyperService;

use crate::api::AppState;
use crate::config::Config;
use crate::registry::ModelRegistry;
use crate::state::{ModelManifest, State};

#[derive(Parser)]
#[command(name = "nodelm", about = "Local AI inference gateway")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Runs the daemon in the foreground.
    Serve,
    /// `<ref>` = org/repo:quant or org/repo/file.gguf.
    Pull { r#ref: String },
    /// All downloaded models + verdict.
    #[command(alias = "ls")]
    List,
    /// Currently-resident models + RSS.
    Ps,
    /// Manifest + hardware verdict for one model.
    Show { name: String },
    /// One-shot chat against a model.
    Run {
        name: String,
        #[arg(trailing_var_arg = true)]
        prompt: Vec<String>,
    },
    /// Toggle hot/cold for a model.
    Hot { name: String, state: String },
    /// Delete a downloaded model.
    Rm { name: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    let cli = Cli::parse();

    if let Command::Serve = cli.command {
        let config = Config::load()?;
        return run_serve(config).await;
    }

    let config = Config::load()?;
    match cli.command {
        Command::Serve => unreachable!(),
        Command::Pull { r#ref } => cli::pull(&config, &r#ref).await,
        Command::List => cli::list(&config).await,
        Command::Ps => cli::ps(&config).await,
        Command::Show { name } => cli::show(&config, &name).await,
        Command::Run { name, prompt } => {
            let prompt = if prompt.is_empty() {
                repl_prompt()?
            } else {
                prompt.join(" ")
            };
            cli::run(&config, &name, &prompt).await
        }
        Command::Hot { name, state } => {
            let on = match state.as_str() {
                "on" => true,
                "off" => false,
                other => anyhow::bail!("expected 'on' or 'off', got '{other}'"),
            };
            cli::hot(&config, &name, on).await
        }
        Command::Rm { name } => cli::rm(&config, &name).await,
    }
}

fn repl_prompt() -> anyhow::Result<String> {
    use std::io::Write;
    print!("> ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Wires the daemon together: loads state + manifests into the registry,
/// then serves the OpenAI/Anthropic/internal API on both listeners (§2).
async fn run_serve(config: Config) -> anyhow::Result<()> {
    let home = config::nodelm_home();
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&config.model_dir)?;
    std::fs::create_dir_all(home.join("logs"))?;
    std::fs::write(home.join("nodelm.pid"), std::process::id().to_string())?;

    let state = State::load()?;

    // Every model starts Unloaded (§3 "Runtime-only fields ... never
    // persisted"); populate the registry from manifest sidecars on disk.
    // Same registration path a fresh pull uses (api::register_model), so
    // a model is loadable identically whether it was there at boot or
    // just finished downloading.
    let app_state = Arc::new(AppState::new(config.clone(), Arc::new(ModelRegistry::new()), state));
    for manifest in ModelManifest::load_all(&config.model_dir)? {
        api::register_model(&app_state, &manifest);
    }

    let router = api::build_router(app_state);

    if config.unix_socket.exists() {
        std::fs::remove_file(&config.unix_socket)?;
    }
    if let Some(parent) = config.unix_socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let unix_listener = tokio::net::UnixListener::bind(&config.unix_socket)?;
    let tcp_listener = tokio::net::TcpListener::bind(&config.http_listen).await?;

    tracing::info!(
        "nodelm serving on {} and {}",
        config.http_listen,
        config.unix_socket.display()
    );

    let tcp_router = router.clone();
    let tcp_task = tokio::spawn(async move {
        if let Err(e) = axum::serve(tcp_listener, tcp_router.into_make_service()).await {
            tracing::error!("tcp listener stopped: {e}");
        }
    });

    let unix_task = tokio::spawn(serve_unix(unix_listener, router));

    let _ = tokio::join!(tcp_task, unix_task);
    Ok(())
}

/// Both listeners route to the same handlers (§2) — axum 0.7's
/// `serve()` only accepts `TcpListener`, so the Unix socket gets its own
/// small accept loop mirroring axum's own TCP implementation.
async fn serve_unix(listener: tokio::net::UnixListener, router: axum::Router) {
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("unix accept failed: {e}");
                continue;
            }
        };
        let io = TokioIo::new(stream);
        let service = TowerToHyperService::new(router.clone());
        tokio::spawn(async move {
            if let Err(e) = ConnBuilder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(io, service)
                .await
            {
                tracing::debug!("unix connection error: {e:#?}");
            }
        });
    }
}
