use axum::{
    Json,
    extract::{FromRequest, Request},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Serialize, de::DeserializeOwned};

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "The operation could not be completed",
        )
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetails,
}
#[derive(Serialize)]
struct ErrorDetails {
    code: &'static str,
    message: String,
    trace_id: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let trace_id = uuid::Uuid::new_v4().to_string();
        if self.status.is_server_error() {
            tracing::error!(%trace_id, code = self.code, "API operation failed");
        }
        (
            self.status,
            Json(ErrorBody {
                error: ErrorDetails {
                    code: self.code,
                    message: self.message,
                    trace_id,
                },
            }),
        )
            .into_response()
    }
}

pub struct ApiJson<T>(pub T);
pub struct ApiQuery<T>(pub T);

impl<S, T> axum::extract::FromRequestParts<S> for ApiQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Query(value)| Self(value))
            .map_err(|_| ApiError::invalid("Invalid query parameters"))
    }
}

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;
    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(|_| {
                ApiError::invalid("Expected a valid JSON request with the documented fields")
            })
    }
}

pub async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "API resource not found")
}
