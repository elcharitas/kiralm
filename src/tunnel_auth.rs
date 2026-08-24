//! Bearer-auth middleware for the tunnel listener only (SPEC.md "Remote
//! tunnel integration" §"Remote-request auth"). `http_listen` and the
//! Unix socket stay exactly as they were — zero-auth, unchanged — this
//! module is only ever layered onto the second, tunnel-only router.

use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::nodelm_home;

fn credentials_path() -> PathBuf {
    nodelm_home().join("tunnel_credentials.json")
}

/// Per-device tunnel credential, distinct from `CloudCredentials` (which
/// is nodelm-web's, not this daemon's to gate remote requests with —
/// see SPEC.md's "Tunnel credential" note). Minted by `nodelm tunnel
/// enable` (outside this build's scope); this module only ever reads it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelCredentials {
    pub token: String,
}

impl TunnelCredentials {
    pub fn load() -> anyhow::Result<Option<TunnelCredentials>> {
        match std::fs::read_to_string(credentials_path()) {
            Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // ponytail: unused until `nodelm tunnel enable` mints one for real
    #[allow(dead_code)]
    pub fn save(&self) -> anyhow::Result<()> {
        let path = credentials_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

/// Byte-for-byte equal in time independent of *where* the mismatch is —
/// no early return on the first differing byte. Lengths differing is not
/// itself hidden (a fixed-length uuid token makes that leak uninteresting
/// here), matching the spec's "hand-rolled constant-time loop" allowance.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// axum middleware layered only onto the tunnel listener's router (§main.rs
/// `run_serve`). Missing/non-matching bearer -> 401 in the same shape
/// `GatewayError::AuthRequired` already uses on each surface, before any
/// handler runs. No credential on disk (tunnel never enabled) means every
/// request is rejected — there is nothing valid to match against.
pub async fn require_tunnel_bearer(req: Request, next: Next) -> Response {
    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let authorized = matches!(
        (provided, TunnelCredentials::load()),
        (Some(token), Ok(Some(creds))) if constant_time_eq(token.as_bytes(), creds.token.as_bytes())
    );

    if authorized {
        return next.run(req).await;
    }

    let msg = "missing or invalid tunnel bearer token";
    if req.uri().path() == "/v1/messages" {
        crate::api::anthropic::anthropic_error(StatusCode::UNAUTHORIZED, "authentication_error", msg)
    } else {
        crate::api::openai::openai_err(StatusCode::UNAUTHORIZED, "authentication_error", msg, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::{get, post};
    use axum::Router;
    use tower::ServiceExt;

    async fn ok_handler() -> &'static str {
        "ok"
    }

    fn test_router() -> Router {
        Router::new()
            .route("/v1/models", get(ok_handler))
            .route("/v1/messages", post(ok_handler))
            .layer(axum::middleware::from_fn(require_tunnel_bearer))
    }

    // `TunnelCredentials` resolves its path off the process-global `HOME`
    // env var, so every test that pokes it must run one-at-a-time or
    // parallel `cargo test` threads clobber each other's HOME.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn constant_time_eq_matches_equal_bytes() {
        assert!(constant_time_eq(b"abc123", b"abc123"));
    }

    #[test]
    fn constant_time_eq_rejects_mismatch() {
        assert!(!constant_time_eq(b"abc123", b"abc124"));
        assert!(!constant_time_eq(b"short", b"muchlonger"));
    }

    #[tokio::test]
    async fn no_credential_file_rejects_every_request() {
        let _guard = HOME_LOCK.lock().unwrap();
        // fresh temp HOME, nothing written to it
        let dir = std::env::temp_dir().join(format!("nodelm-tunnel-auth-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);

        let app = test_router();
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_header_is_401_on_openai_route() {
        let _guard = HOME_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("nodelm-tunnel-auth-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);
        TunnelCredentials {
            token: "the-real-token".into(),
        }
        .save()
        .unwrap();

        let app = test_router();
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "authentication_error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn wrong_token_is_401_on_anthropic_route() {
        let _guard = HOME_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("nodelm-tunnel-auth-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);
        TunnelCredentials {
            token: "the-real-token".into(),
        }
        .save()
        .unwrap();

        let app = test_router();
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/v1/messages")
                    .method("POST")
                    .header(header::AUTHORIZATION, "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "error"); // anthropic error envelope shape
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn correct_bearer_token_passes_through() {
        let _guard = HOME_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("nodelm-tunnel-auth-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);
        TunnelCredentials {
            token: "the-real-token".into(),
        }
        .save()
        .unwrap();

        let app = test_router();
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/v1/models")
                    .header(header::AUTHORIZATION, "Bearer the-real-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
