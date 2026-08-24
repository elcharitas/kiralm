//! `/api/tunnel/*` — daemon-owned `cloudflared` supervision, called by
//! the `nodelm tunnel` CLI subcommands over the local socket (SPEC.md
//! "Remote tunnel (Cloudflare Tunnel) integration" §"Tunnel client
//! (daemon)").

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use super::SharedState;
use crate::state::TunnelState;
use crate::{engine, tunnel, tunnel_auth::TunnelCredentials};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/tunnel/enable", post(enable))
        .route("/api/tunnel/disable", post(disable))
        .route("/api/tunnel/status", get(status))
}

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

/// `nodelm tunnel enable`: detect the binary, provision (or reuse a
/// stored token from a prior `enable`), spawn, mint the local bearer
/// credential if none exists yet, persist state.
async fn enable(State(app): State<SharedState>) -> Response {
    if app.state.read().tunnel.as_ref().is_some_and(|t| t.enabled) {
        return err(
            StatusCode::BAD_REQUEST,
            "tunnel already enabled — run 'nodelm tunnel disable' first",
        );
    }
    if let Err(e) = tunnel::detect(&app.config.tunnel.cloudflared_bin) {
        return err(StatusCode::PRECONDITION_FAILED, e.to_string());
    }

    // A `disable` leaves the provisioned Cloudflare tunnel + its token in
    // place (§"Tunnel client (daemon)" CLI table) — re-enabling reuses
    // both rather than round-tripping nodelm-web/Cloudflare again.
    let prior = app.state.read().tunnel.clone();
    let (cf_token, hostname, cf_tunnel_id) = match (tunnel::TunnelToken::load(), prior) {
        (Ok(Some(stored)), Some(prev)) => (stored.token, prev.hostname, prev.cf_tunnel_id),
        _ => match tunnel::provision(&app.config).await {
            Ok(p) => {
                if let Err(e) = (tunnel::TunnelToken { token: p.token.clone() }).save() {
                    return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
                }
                (p.token, p.hostname, p.cf_tunnel_id)
            }
            Err(e) => return err(StatusCode::BAD_GATEWAY, e.to_string()),
        },
    };

    let child = match tunnel::spawn(&app.config.tunnel.cloudflared_bin, &cf_token).await {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    *app.tunnel_child.lock() = Some(child);

    if matches!(TunnelCredentials::load(), Ok(None)) {
        if let Err(e) = (TunnelCredentials {
            token: uuid::Uuid::new_v4().to_string(),
        })
        .save()
        {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    }

    {
        let mut state = app.state.write();
        state.tunnel = Some(TunnelState {
            enabled: true,
            hostname: hostname.clone(),
            cf_tunnel_id,
        });
        if let Err(e) = state.save() {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    }

    Json(json!({"hostname": hostname})).into_response()
}

/// `nodelm tunnel disable`: kills the supervised child (`engine::shutdown`
/// verbatim) and flips `state.tunnel.enabled = false`. The backend-side
/// Cloudflare resource and the stored token are left alone.
async fn disable(State(app): State<SharedState>) -> Response {
    // Extracted to its own statement so the (non-`Send`) MutexGuard
    // temporary is dropped before the `.await` below, not held across it.
    let child = app.tunnel_child.lock().take();
    if let Some(child) = child {
        engine::shutdown(child).await;
    }

    let mut state = app.state.write();
    let Some(t) = state.tunnel.as_mut() else {
        return err(StatusCode::BAD_REQUEST, "tunnel not enabled");
    };
    t.enabled = false;
    if let Err(e) = state.save() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `nodelm tunnel status`: cross-checks persisted state against the live
/// child (§"Health check" — process-alive) plus a best-effort
/// connectivity probe.
async fn status(State(app): State<SharedState>) -> Response {
    let Some(t) = app.state.read().tunnel.clone() else {
        return Json(json!({"enabled": false})).into_response();
    };
    if !t.enabled {
        return Json(json!({"enabled": false})).into_response();
    }

    let running = {
        let mut guard = app.tunnel_child.lock();
        match guard.as_mut() {
            Some(child) => tunnel::is_alive(child),
            None => false,
        }
    };
    let connectivity_confirmed = tunnel::check_connectivity().await;

    Json(json!({
        "enabled": true,
        "hostname": t.hostname,
        "cf_tunnel_id": t.cf_tunnel_id,
        "running": running,
        "connectivity_confirmed": connectivity_confirmed,
    }))
    .into_response()
}

/// Re-spawns `cloudflared` on daemon startup if `state.json` says the
/// tunnel was left enabled (§"Daemon-restart behavior") — no new
/// Cloudflare API call. A failed re-spawn (missing binary, revoked
/// token) just warns and leaves `enabled = true` so `nodelm tunnel
/// status` reports the crash honestly instead of silently flipping off.
pub async fn respawn_if_enabled(app: &SharedState) {
    let enabled = app.state.read().tunnel.as_ref().is_some_and(|t| t.enabled);
    if !enabled {
        return;
    }
    if let Err(e) = tunnel::detect(&app.config.tunnel.cloudflared_bin) {
        tracing::warn!("tunnel: {e}");
        return;
    }
    let stored = match tunnel::TunnelToken::load() {
        Ok(Some(t)) => t,
        Ok(None) => {
            tracing::warn!(
                "tunnel: state.json says enabled but no stored token was found — \
                 try 'nodelm tunnel disable' then 'enable'"
            );
            return;
        }
        Err(e) => {
            tracing::warn!("tunnel: failed to read stored token: {e}");
            return;
        }
    };
    match tunnel::spawn(&app.config.tunnel.cloudflared_bin, &stored.token).await {
        Ok(child) => *app.tunnel_child.lock() = Some(child),
        Err(e) => tracing::warn!("tunnel: failed to respawn cloudflared: {e}"),
    }
}
