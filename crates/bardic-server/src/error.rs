use axum::{
    extract::{
        multipart::MultipartError,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::Value;

/// The contract's `Error` body, with the status it is sent with.
#[derive(Debug, Clone, Serialize)]
pub struct ApiError {
    #[serde(skip)]
    pub status: StatusCode,
    pub code: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
    /// Extra top-level fields some errors carry (for example `server_place`).
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &str, detail: impl Into<String>) -> Self {
        ApiError {
            status,
            code: code.to_string(),
            detail: detail.into(),
            retryable: None,
            retry_after_seconds: None,
            context: None,
            extra: serde_json::Map::new(),
        }
    }
    pub fn invalid(code: &str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, detail)
    }
    pub fn not_found(code: &str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, detail)
    }
    pub fn conflict(code: &str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, detail)
    }
    /// An unexpected failure. The detail is generic; the cause is logged, never sent.
    pub fn internal(cause: impl std::fmt::Display) -> Self {
        tracing::error!(%cause, "internal error");
        let mut e = Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Something went wrong on the server. Nothing was changed that you need to undo.",
        );
        e.retryable = Some(true);
        e
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self)).into_response()
    }
}

impl From<JsonRejection> for ApiError {
    fn from(r: JsonRejection) -> Self {
        ApiError::invalid(
            "invalid_request",
            format!("The request body is not valid: {r}"),
        )
    }
}

impl From<QueryRejection> for ApiError {
    fn from(r: QueryRejection) -> Self {
        ApiError::invalid(
            "invalid_request",
            format!("A query parameter is not valid: {r}"),
        )
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        ApiError::internal(e)
    }
}

impl From<MultipartError> for ApiError {
    fn from(e: MultipartError) -> Self {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "import_too_large",
                "The file is larger than this server accepts.",
            )
        } else {
            ApiError::invalid(
                "invalid_request",
                format!("The upload could not be read: {e}"),
            )
        }
    }
}
