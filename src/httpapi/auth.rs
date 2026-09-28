use super::errors::{ApiError, ApiJson};
use crate::auth::{AuthError, AuthService, CredentialKind, Principal, Scope};
use axum::{
    Extension, Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Path, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Deserialize;
use std::{net::SocketAddr, sync::Arc};

#[derive(Clone)]
pub struct AuthContext {
    pub service: AuthService,
    pub origin: Arc<str>,
}

pub fn routes(context: AuthContext) -> Router {
    Router::new()
        .route("/api/v1/auth/setup", get(setup_status).post(setup))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/session", get(session).delete(logout))
        .route("/api/v1/auth/keys", get(keys).post(create_key))
        .route("/api/v1/auth/keys/{id}", delete(revoke_key))
        .route("/api/v1/about", get(about))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(context)
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Invalid(message) => Self::invalid(message),
            AuthError::Unauthorized => Self::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Authentication required",
            ),
            AuthError::AlreadyConfigured => Self::new(
                StatusCode::CONFLICT,
                "already_configured",
                "An administrator already exists",
            ),
            AuthError::RateLimited => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "Wait before trying again",
            ),
            AuthError::Unavailable => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "Authentication is temporarily unavailable",
            ),
            AuthError::Database(_) => Self::internal(),
        }
    }
}

pub fn check_origin(
    context: &AuthContext,
    headers: &HeaderMap,
    required: bool,
) -> Result<(), ApiError> {
    match headers.get(header::ORIGIN) {
        Some(origin) if origin.as_bytes() == context.origin.as_bytes() => Ok(()),
        None if !required => Ok(()),
        _ => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "invalid_origin",
            "Request origin is not allowed",
        )),
    }
}

fn credential(headers: &HeaderMap) -> Result<(&str, bool), ApiError> {
    let bearer = headers.get(header::AUTHORIZATION);
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .split(';')
                .map(str::trim)
                .find_map(|pair| pair.strip_prefix("library_session="))
        });
    if bearer.is_some() && cookie.is_some() {
        return Err(ApiError::invalid(
            "Use one authentication method per request",
        ));
    }
    if let Some(value) = bearer {
        return value
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|token| token.starts_with("key_") && token.len() <= 256)
            .map(|token| (token, false))
            .ok_or_else(|| AuthError::Unauthorized.into());
    }
    cookie
        .filter(|token| token.starts_with("session_") && token.len() <= 256)
        .map(|token| (token, true))
        .ok_or_else(|| AuthError::Unauthorized.into())
}

pub async fn authorize(
    context: &AuthContext,
    headers: &HeaderMap,
    method: &Method,
    required: Scope,
) -> Result<Principal, ApiError> {
    let (token, cookie) = credential(headers)?;
    if cookie && !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        check_origin(context, headers, true)?;
    }
    let principal = context.service.authenticate(token).await?;
    if cookie != matches!(principal.kind, CredentialKind::Session) {
        return Err(AuthError::Unauthorized.into());
    }
    if !principal.scope.allows(required) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Credential scope does not allow this operation",
        ));
    }
    Ok(principal)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    username: String,
    password: String,
}

async fn setup_status(
    State(context): State<AuthContext>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(
        serde_json::json!({"configured": context.service.configured().await?}),
    ))
}

async fn setup(
    State(context): State<AuthContext>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<Credentials>,
) -> Result<Response, ApiError> {
    check_origin(&context, &headers, false)?;
    let result = context
        .service
        .setup(&input.username, &input.password)
        .await?;
    login_response(&context, result, StatusCode::CREATED)
}
async fn login(
    State(context): State<AuthContext>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<Credentials>,
) -> Result<Response, ApiError> {
    check_origin(&context, &headers, false)?;
    let result = match peer {
        Some(Extension(ConnectInfo(peer))) => {
            context
                .service
                .login_from_peer(&input.username, &input.password, peer.ip())
                .await?
        }
        None => {
            context
                .service
                .login(&input.username, &input.password)
                .await?
        }
    };
    login_response(&context, result, StatusCode::OK)
}

fn login_response(
    context: &AuthContext,
    login: crate::auth::Login,
    status: StatusCode,
) -> Result<Response, ApiError> {
    let secure = if context.origin.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "library_session={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400{secure}",
        login.token
    );
    Ok((status, [(header::SET_COOKIE, cookie), (header::CACHE_CONTROL, "no-store".into())],
        Json(serde_json::json!({"user_id": login.principal.user_id, "scope": login.principal.scope, "expires_at": login.expires_at}))).into_response())
}

async fn session(
    State(context): State<AuthContext>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let principal = authorize(&context, &headers, &Method::GET, Scope::Read).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"user_id":principal.user_id,"scope":principal.scope})),
    )
        .into_response())
}
async fn about(
    State(context): State<AuthContext>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize(&context, &headers, &Method::GET, Scope::Read).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"name": "Foldout", "version": env!("CARGO_PKG_VERSION")})),
    )
        .into_response())
}
async fn logout(
    State(context): State<AuthContext>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize(&context, &headers, &Method::DELETE, Scope::Read).await?;
    let (token, cookie) = credential(&headers)?;
    if !cookie {
        return Err(ApiError::invalid("Logout requires a browser session"));
    }
    context.service.logout(token).await?;
    Ok((
        StatusCode::NO_CONTENT,
        [(
            header::SET_COOKIE,
            "library_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        )],
    )
        .into_response())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyInput {
    name: String,
    scope: Scope,
}
async fn keys(
    State(context): State<AuthContext>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize(&context, &headers, &Method::GET, Scope::Admin).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(context.service.list_keys().await?),
    )
        .into_response())
}
async fn create_key(
    State(context): State<AuthContext>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<KeyInput>,
) -> Result<Response, ApiError> {
    authorize(&context, &headers, &Method::POST, Scope::Admin).await?;
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(context.service.create_key(&input.name, input.scope).await?),
    )
        .into_response())
}
async fn revoke_key(
    State(context): State<AuthContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    authorize(&context, &headers, &Method::DELETE, Scope::Admin).await?;
    context.service.revoke_key(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}
