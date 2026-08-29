use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::SharedState;
use crate::events::EventTopic;

const MAX_UPLOAD_BODY_BYTES: usize = 36 * 1024 * 1024;
const MAX_READ_BYTES: u64 = 25 * 1024 * 1024;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/attachments", post(upload))
        .route("/api/attachments/:id", get(download))
        .route(
            "/api/attachments/:id/references/:reference",
            delete(release_attachment_reference),
        )
        .route(
            "/api/attachment-references/:reference",
            delete(release_reference),
        )
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BODY_BYTES))
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

#[derive(Deserialize)]
struct UploadRequest {
    reference: String,
    filename: Option<String>,
    data_url: String,
}

async fn upload(State(app): State<SharedState>, Json(body): Json<UploadRequest>) -> Response {
    match app
        .attachments
        .ingest_data_url(&body.data_url, body.filename.as_deref(), &body.reference)
    {
        Ok(descriptor) => {
            app.events.publish(
                EventTopic::Attachment,
                "attachment_ingested",
                None,
                json!({
                    "id": &descriptor.id,
                    "media_type": &descriptor.media_type,
                    "size_bytes": descriptor.size_bytes,
                }),
            );
            (StatusCode::CREATED, Json(descriptor)).into_response()
        }
        Err(err) => error(StatusCode::BAD_REQUEST, err.to_string()),
    }
}

async fn download(State(app): State<SharedState>, Path(id): Path<String>) -> Response {
    let descriptor = match app.attachments.descriptor(&id) {
        Ok(Some(descriptor)) => descriptor,
        Ok(None) => return error(StatusCode::NOT_FOUND, "attachment not found"),
        Err(err) => {
            tracing::warn!("attachment descriptor failed: {err}");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "attachment could not be verified",
            );
        }
    };
    let bytes = match app.attachments.read(&id, MAX_READ_BYTES) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!("attachment read failed: {err}");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "attachment could not be read",
            );
        }
    };
    let content_type = HeaderValue::from_str(&descriptor.media_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CACHE_CONTROL,
            "private, max-age=31536000, immutable",
        )
        .header("x-content-type-options", "nosniff")
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn release_reference(
    State(app): State<SharedState>,
    Path(reference): Path<String>,
) -> Response {
    match app.attachments.release_reference(&reference) {
        Ok(released) => {
            app.events.publish(
                EventTopic::Attachment,
                "attachment_reference_released",
                None,
                json!({"reference": reference, "released": released}),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => error(StatusCode::BAD_REQUEST, err.to_string()),
    }
}

async fn release_attachment_reference(
    State(app): State<SharedState>,
    Path((id, reference)): Path<(String, String)>,
) -> Response {
    match app
        .attachments
        .release_attachment_reference(&id, &reference)
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "attachment reference not found"),
        Err(err) => error(StatusCode::BAD_REQUEST, err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn test_state() -> SharedState {
        let (tunnel_listener, _) = tokio::sync::mpsc::channel(1);
        Arc::new(crate::api::AppState::new(
            crate::config::Config::default(),
            Arc::new(crate::registry::ModelRegistry::new()),
            crate::state::State::default(),
            crate::settings::Settings::default(),
            crate::chats::Chats::default(),
            tunnel_listener,
        ))
    }

    #[tokio::test]
    async fn uploads_downloads_and_releases_verified_content() {
        let app = routes().with_state(test_state());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/attachments")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "reference": "chat:test",
                            "filename": "notes.txt",
                            "data_url": "data:text/plain;base64,aGVsbG8=",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let descriptor: crate::attachments::AttachmentDescriptor =
            serde_json::from_slice(&body).unwrap();

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/attachments/{}", descriptor.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "hello"
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!(
                        "/api/attachments/{}/references/chat%3Atest",
                        descriptor.id
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
}
