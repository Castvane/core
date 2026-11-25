use std::collections::BTreeMap;

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

#[derive(Debug)]
pub enum ApiError {
    Http(StatusCode, &'static str),
    Validation(BTreeMap<&'static str, Vec<&'static str>>),
    Internal(&'static str),
    Throttled,
}

#[derive(Serialize)]
struct Envelope {
    code: u16,
    message: &'static str,
    details: Vec<&'static str>,
}

impl ApiError {
    pub fn forbidden(detail: &'static str) -> Self {
        Self::Http(StatusCode::FORBIDDEN, detail)
    }

    pub fn bad_request(detail: &'static str) -> Self {
        Self::Http(StatusCode::BAD_REQUEST, detail)
    }

    pub fn validation(field: &'static str, detail: &'static str) -> Self {
        Self::Validation(BTreeMap::from([(field, vec![detail])]))
    }

    pub fn not_found() -> Self {
        Self::Http(StatusCode::NOT_FOUND, "Not Found")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, detail) = match self {
            Self::Validation(errors) => {
                return (StatusCode::CONFLICT, Json(errors)).into_response();
            }
            Self::Http(status, detail) => (status, detail),
            Self::Internal(operation) => {
                tracing::error!(operation, "request operation failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "")
            }
            Self::Throttled => (StatusCode::TOO_MANY_REQUESTS, "too many login attempts"),
        };
        let body = Envelope {
            code: status.as_u16(),
            message: status.canonical_reason().unwrap_or("Error"),
            details: if detail.is_empty() {
                Vec::new()
            } else {
                vec![detail]
            },
        };
        let mut response = (status, Json(body)).into_response();
        if status == StatusCode::TOO_MANY_REQUESTS {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
        }
        response
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(_: sqlx::Error) -> Self {
        Self::Internal("database")
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(_: serde_json::Error) -> Self {
        Self::Internal("configuration decoding")
    }
}
