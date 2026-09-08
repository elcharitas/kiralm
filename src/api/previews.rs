use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::SharedState;

pub fn routes() -> Router<SharedState> {
    Router::new().route("/api/previews/probe", get(probe))
}

#[derive(Deserialize)]
struct ProbeQuery {
    url: String,
}

async fn probe(State(_app): State<SharedState>, Query(query): Query<ProbeQuery>) -> Response {
    let url = match reqwest::Url::parse(&query.url) {
        Ok(url) if safe_preview_url(&url) => url,
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "preview URL must use a loopback host",
            )
        }
    };
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "preview probe unavailable",
            )
        }
    };
    let started = Instant::now();
    match client.get(url).send().await {
        Ok(response) => Json(json!({
            "reachable": true,
            "status": response.status().as_u16(),
            "latency_ms": started.elapsed().as_millis(),
            "content_type": response.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
        }))
        .into_response(),
        Err(_) => Json(json!({
            "reachable": false,
            "status": null,
            "latency_ms": started.elapsed().as_millis(),
            "content_type": null,
        }))
        .into_response(),
    }
}

fn safe_preview_url(url: &reqwest::Url) -> bool {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_http_urls_are_allowed() {
        assert!(safe_preview_url(
            &reqwest::Url::parse("http://localhost:5173").unwrap()
        ));
        assert!(safe_preview_url(
            &reqwest::Url::parse("http://127.0.0.1:3000").unwrap()
        ));
        assert!(safe_preview_url(
            &reqwest::Url::parse("http://127.0.0.2:3000").unwrap()
        ));
        assert!(safe_preview_url(
            &reqwest::Url::parse("http://[::1]:3000").unwrap()
        ));
        assert!(!safe_preview_url(
            &reqwest::Url::parse("https://example.com").unwrap()
        ));
        assert!(!safe_preview_url(
            &reqwest::Url::parse("file:///tmp/x").unwrap()
        ));
        assert!(!safe_preview_url(
            &reqwest::Url::parse("http://user:pass@localhost:3000").unwrap()
        ));
    }
}
