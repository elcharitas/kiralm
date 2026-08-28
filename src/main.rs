//! `kiralm` binary entry point: arg parse -> dispatch to `cli.rs` or the
//! `serve` daemon.

mod agent_plugin;
mod api;
mod chats;
mod cli;
mod cloud;
mod config;
mod dsh;
mod engine;
mod gguf;
mod git;
mod marketplace;
mod mcp;
mod plugin_catalog;
mod pull;
mod registry;
mod residency;
mod settings;
mod skills;
mod state;
mod tools;
mod tunnel;
mod tunnel_auth;

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
#[command(name = "kiralm", about = "Local AI inference gateway")]
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
    /// Device-flow login against kiralm-web, for cloud models.
    Login,
    /// Deletes stored cloud credentials.
    Logout,
    /// Shows this machine's registered device identity.
    Device,
    /// Enable/disable/check the Cloudflare Tunnel remote connection.
    Tunnel {
        #[command(subcommand)]
        cmd: TunnelCommand,
    },
}

#[derive(Subcommand)]
enum TunnelCommand {
    /// Provisions (or reuses) a tunnel and starts `cloudflared`.
    Enable,
    /// Stops `cloudflared`; the backend-side tunnel stays provisioned.
    Disable,
    /// Reports whether the tunnel is enabled and `cloudflared` is alive.
    Status,
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
        Command::Login => cloud::login(&config).await,
        Command::Logout => cloud::logout(),
        Command::Device => cloud::device_info(),
        Command::Tunnel { cmd } => match cmd {
            TunnelCommand::Enable => cli::tunnel_enable(&config).await,
            TunnelCommand::Disable => cli::tunnel_disable(&config).await,
            TunnelCommand::Status => cli::tunnel_status(&config).await,
        },
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
/// then serves the OpenAI/Anthropic/internal API on both listeners.
async fn run_serve(config: Config) -> anyhow::Result<()> {
    let home = config::kiralm_home();
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&config.model_dir)?;
    std::fs::create_dir_all(home.join("logs"))?;
    std::fs::write(home.join("kiralm.pid"), std::process::id().to_string())?;

    let state = State::load()?;

    // Every model starts Unloaded (runtime-only field, never persisted);
    // populate the registry from manifest sidecars on disk.
    // Same registration path a fresh pull uses (api::register_model), so
    // a model is loadable identically whether it was there at boot or
    // just finished downloading.
    let app_state = Arc::new(AppState::new(
        config.clone(),
        Arc::new(ModelRegistry::new()),
        state,
        settings::Settings::load(),
        chats::Chats::load(),
    ));
    for manifest in ModelManifest::load_all(&config.model_dir)? {
        api::register_model(&app_state, &manifest);
    }

    let cloud_config = app_state.config.cloud.clone();
    let registry_for_sync = app_state.registry.clone();
    let app_state_for_tunnel = app_state.clone();
    let app_state_for_mcp = app_state.clone();
    let router = api::build_router(app_state);

    // bind TCP first: if another daemon already owns this port, fail fast
    // here instead of deleting and re-creating its live unix socket file.
    let tcp_listener = tokio::net::TcpListener::bind(&config.http_listen).await?;

    if config.unix_socket.exists() {
        std::fs::remove_file(&config.unix_socket)?;
    }
    if let Some(parent) = config.unix_socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let unix_listener = tokio::net::UnixListener::bind(&config.unix_socket)?;

    // Third listener, bearer-checked -- the only thing `cloudflared`'s
    // tunnel ingress ever points at.
    // Bound unconditionally, same as `http_listen`; nothing reaches it
    // from the public internet until `kiralm tunnel enable` configures
    // the tunnel's ingress to target this port.
    let tunnel_listener = tokio::net::TcpListener::bind(&config.tunnel.listen).await?;

    tracing::info!(
        "kiralm serving on {}, {} (unix), and {} (tunnel, bearer-checked)",
        config.http_listen,
        config.unix_socket.display(),
        config.tunnel.listen
    );

    let tcp_router = router.clone();
    let tcp_task = tokio::spawn(async move {
        if let Err(e) = axum::serve(tcp_listener, tcp_router.into_make_service()).await {
            tracing::error!("tcp listener stopped: {e}");
        }
    });

    let tunnel_router = router.clone().layer(axum::middleware::from_fn(tunnel_auth::require_tunnel_bearer));
    let tunnel_task = tokio::spawn(async move {
        if let Err(e) = axum::serve(tunnel_listener, tunnel_router.into_make_service()).await {
            tracing::error!("tunnel listener stopped: {e}");
        }
    });

    let unix_task = tokio::spawn(serve_unix(unix_listener, router));

    // Re-spawn `cloudflared` if it was left enabled across a restart,
    // after listeners are already bound, same ordering
    // catalog-sync-at-boot uses.
    api::tunnel::respawn_if_enabled(&app_state_for_tunnel).await;

    // Detached, like the catalog sync below: a slow or hanging MCP
    // server (e.g. an `npx` first-install) must never delay `serve`
    // itself becoming ready to accept requests.
    tokio::spawn(async move {
        let configs = mcp::load_configs();
        app_state_for_mcp.mcp.connect_all_enabled(&configs).await;
    });

    // One non-blocking sync after the listeners are already accepting.
    // Detached, never awaited, so a slow or unreachable backend can
    // never delay `serve` startup. Plugins have no startup sync of their
    // own -- that catalog is proxied live, per request, instead.
    if cloud_config.catalog_sync_on_startup {
        tokio::spawn(async move {
            cloud::sync_cloud_models(&cloud_config.api_base_url, &registry_for_sync).await;
        });
    }

    let _ = tokio::join!(tcp_task, unix_task, tunnel_task);
    Ok(())
}

/// Both listeners route to the same handlers. axum 0.7's
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
