#![forbid(unsafe_code)]

use axum::{Router, routing::get};

pub const LEGACY_API_PREFIX: &str = "/api/v3";

/// Liveness is deliberately independent of authentication and readiness.
pub fn router() -> Router {
    Router::new().route("/ping", get(|| async { "pong" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn legacy_ping_contract() {
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/ping")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"],
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            to_bytes(response.into_body(), 64).await.unwrap().as_ref(),
            b"pong"
        );
    }

    #[tokio::test]
    async fn unfinished_api_is_not_reported_as_success() {
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/api/v3/process")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
