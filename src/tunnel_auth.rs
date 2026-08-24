//! Bearer-auth middleware for the tunnel listener only (SPEC.md "Remote
//! tunnel integration" §"Remote-request auth"). `http_listen` and the
//! Unix socket stay exactly as they were, zero-auth, unchanged. This
//! module is only ever layered onto the second, tunnel-only router.

use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::config::kiralm_home;

fn credentials_path() -> PathBuf {
    kiralm_home().join("tunnel_credentials.json")
}

/// Per-device tunnel credential, distinct from `CloudCredentials` (which
/// is kiralm-web's, not this daemon's to gate remote requests with;
/// see SPEC.md's "Tunnel credential" note). Minted by `kiralm tunnel
/// enable` (outside this build's scope); this module only ever reads it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelCredentials {
    pub token: String,
}

impl TunnelCredentials {
    pub fn load() -> anyhow::Result<Option<TunnelCredentials>> {
        Self::load_from(&credentials_path())
    }

    fn load_from(path: &Path) -> anyhow::Result<Option<TunnelCredentials>> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // ponytail: unused until `kiralm tunnel enable` mints one for real
    #[allow(dead_code)]
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&credentials_path())
    }

    fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

/// Byte-for-byte equal in time independent of *where* the mismatch is,
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

/// Pure decision, no I/O: the one thing that actually needs testing.
/// Kept separate from disk access so tests never touch a shared path
/// (env-var-based HOME overrides across parallel `cargo test` threads
/// is a real hazard: `std::env::set_var` mutates process-global state
/// every test binary thread can read, poisoning unrelated tests on a
/// panic; this sidesteps it entirely instead of trying to serialize it).
fn is_valid_bearer(provided: Option<&str>, creds: Option<&TunnelCredentials>) -> bool {
    matches!(
        (provided, creds),
        (Some(token), Some(c)) if constant_time_eq(token.as_bytes(), c.token.as_bytes())
    )
}

fn unauthorized_response(path: &str) -> Response {
    let msg = "missing or invalid tunnel bearer token";
    if path == "/v1/messages" {
        crate::api::anthropic::anthropic_error(StatusCode::UNAUTHORIZED, "authentication_error", msg)
    } else {
        crate::api::openai::openai_err(StatusCode::UNAUTHORIZED, "authentication_error", msg, None)
    }
}

/// axum middleware layered only onto the tunnel listener's router (§main.rs
/// `run_serve`). Missing/non-matching bearer -> 401 in the same shape
/// `GatewayError::AuthRequired` already uses on each surface, before any
/// handler runs. No credential on disk (tunnel never enabled) means every
/// request is rejected, since there is nothing valid to match against.
pub async fn require_tunnel_bearer(req: Request, next: Next) -> Response {
    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let creds = TunnelCredentials::load().ok().flatten();

    if is_valid_bearer(provided, creds.as_ref()) {
        return next.run(req).await;
    }

    unauthorized_response(req.uri().path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::Request as HttpRequest;
    use axum::middleware::from_fn_with_state;
    use axum::routing::{get, post};
    use axum::Router;
    use tower::ServiceExt;

    #[test]
    fn constant_time_eq_matches_equal_bytes() {
        assert!(constant_time_eq(b"abc123", b"abc123"));
    }

    #[test]
    fn constant_time_eq_rejects_mismatch() {
        assert!(!constant_time_eq(b"abc123", b"abc124"));
        assert!(!constant_time_eq(b"short", b"muchlonger"));
    }

    #[test]
    fn is_valid_bearer_matches_correct_token() {
        let creds = TunnelCredentials { token: "the-real-token".into() };
        assert!(is_valid_bearer(Some("the-real-token"), Some(&creds)));
    }

    #[test]
    fn is_valid_bearer_rejects_wrong_token() {
        let creds = TunnelCredentials { token: "the-real-token".into() };
        assert!(!is_valid_bearer(Some("wrong-token"), Some(&creds)));
    }

    #[test]
    fn is_valid_bearer_rejects_missing_header() {
        let creds = TunnelCredentials { token: "the-real-token".into() };
        assert!(!is_valid_bearer(None, Some(&creds)));
    }

    #[test]
    fn is_valid_bearer_rejects_no_credentials_on_disk() {
        assert!(!is_valid_bearer(Some("anything"), None));
    }

    // load_from/save_to round-trip against a real temp path -- no HOME
    // mutation, no shared global state, safe under parallel test threads.
    #[test]
    fn credentials_round_trip_to_disk() {
        let dir = std::env::temp_dir().join(format!("kiralm-tunnel-creds-{}", uuid::Uuid::new_v4()));
        let path = dir.join("tunnel_credentials.json");
        let creds = TunnelCredentials { token: "round-trip-token".into() };
        creds.save_to(&path).unwrap();
        let loaded = TunnelCredentials::load_from(&path).unwrap();
        assert_eq!(loaded.unwrap().token, "round-trip-token");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_missing_path_is_none_not_error() {
        let path = std::env::temp_dir().join(format!("kiralm-tunnel-creds-missing-{}", uuid::Uuid::new_v4()));
        assert!(TunnelCredentials::load_from(&path).unwrap().is_none());
    }

    // Full axum-router exercise of the real middleware wiring (401 shape,
    // route-dependent error envelope, pass-through on success) without
    // touching the real credentials_path() -- a state-injected middleware
    // variant standing in for `require_tunnel_bearer` so the test asserts
    // the actual router/response-shape behavior, not just is_valid_bearer.
    async fn ok_handler() -> &'static str {
        "ok"
    }

    async fn require_bearer_with_state(
        State(creds): State<Option<TunnelCredentials>>,
        req: Request,
        next: Next,
    ) -> Response {
        let provided = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if is_valid_bearer(provided, creds.as_ref()) {
            return next.run(req).await;
        }
        unauthorized_response(req.uri().path())
    }

    fn test_router(creds: Option<TunnelCredentials>) -> Router {
        Router::new()
            .route("/v1/models", get(ok_handler))
            .route("/v1/messages", post(ok_handler))
            .layer(from_fn_with_state(creds, require_bearer_with_state))
    }

    #[tokio::test]
    async fn no_credential_configured_rejects_every_request() {
        let app = test_router(None);
        let resp = app
            .oneshot(HttpRequest::builder().uri("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_header_is_401_on_openai_route() {
        let creds = Some(TunnelCredentials { token: "the-real-token".into() });
        let app = test_router(creds);
        let resp = app
            .oneshot(HttpRequest::builder().uri("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["type"], "authentication_error");
    }

    #[tokio::test]
    async fn wrong_token_is_401_on_anthropic_route() {
        let creds = Some(TunnelCredentials { token: "the-real-token".into() });
        let app = test_router(creds);
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
    }

    #[tokio::test]
    async fn correct_bearer_token_passes_through() {
        let creds = Some(TunnelCredentials { token: "the-real-token".into() });
        let app = test_router(creds);
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
    }
}
