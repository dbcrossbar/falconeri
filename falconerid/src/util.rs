//! Various axum-related utilities.

use std::result;

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
};
use falconeri_common::{
    base64::{prelude::BASE64_STANDARD, Engine},
    db, diesel,
    models::DatumOwnershipError,
    prelude::*,
};

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    /// Database connection pool.
    pub pool: db::AsyncPool,
    /// Admin password for authentication.
    pub admin_password: String,
}

/// An authenticated user. For now, this carries no identity information,
/// because we only distinguish between "authenticated" and "not authenticated",
/// and we therefore just need a placeholder that represents authentication.
pub struct User;

impl FromRequestParts<AppState> for User {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> result::Result<Self, Self::Rejection> {
        // Get our auth header.
        let header = parts
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or((StatusCode::UNAUTHORIZED, "missing auth"))?;

        let (username, password) = parse_basic_auth(header)
            .ok_or((StatusCode::BAD_REQUEST, "invalid auth header"))?;

        // Validate our user.
        if username == "falconeri" && password == state.admin_password {
            Ok(User)
        } else {
            Err((StatusCode::UNAUTHORIZED, "invalid credentials"))
        }
    }
}

/// Parse HTTP Basic Auth credentials from a header value.
fn parse_basic_auth(header: &str) -> Option<(String, String)> {
    let encoded = header.strip_prefix("Basic ")?;
    let decoded = BASE64_STANDARD.decode(encoded).ok()?;
    let credentials = String::from_utf8(decoded).ok()?;
    let (user, pass) = credentials.split_once(':')?;
    Some((user.to_owned(), pass.to_owned()))
}

/// A database connection from the pool, extracted automatically by Axum.
pub struct DbConn(pub db::AsyncPooledConn);

impl FromRequestParts<AppState> for DbConn {
    type Rejection = FalconeridError;

    async fn from_request_parts(
        _parts: &mut Parts,
        state: &AppState,
    ) -> result::Result<Self, Self::Rejection> {
        let conn = state.pool.get().await.map_err(|e| {
            FalconeridError::Internal(format_err!("pool error: {}", e))
        })?;
        Ok(DbConn(conn))
    }
}

/// An error type for `falconerid` that maps to appropriate HTTP status codes.
#[derive(Debug)]
pub enum FalconeridError {
    /// Internal server error (500).
    Internal(Error),
    /// Forbidden - ownership verification failed (403).
    Forbidden(String),
}

impl IntoResponse for FalconeridError {
    fn into_response(self) -> Response {
        match self {
            FalconeridError::Internal(err) => {
                // Log our full error with the error chain using Debug formatting.
                error!("{:?}", err);
                // Use Display to avoid leaking backtraces to clients.
                let payload = format!("{}", err);
                (StatusCode::INTERNAL_SERVER_ERROR, payload).into_response()
            }
            FalconeridError::Forbidden(msg) => {
                warn!("Forbidden: {}", msg);
                (StatusCode::FORBIDDEN, msg).into_response()
            }
        }
    }
}

impl From<Error> for FalconeridError {
    fn from(err: Error) -> Self {
        FalconeridError::Internal(err)
    }
}

impl From<DatumOwnershipError> for FalconeridError {
    fn from(err: DatumOwnershipError) -> Self {
        FalconeridError::Forbidden(err.to_string())
    }
}

impl From<diesel::result::Error> for FalconeridError {
    fn from(err: diesel::result::Error) -> Self {
        FalconeridError::Internal(err.into())
    }
}

/// The result type of `falconerid` handler.
pub type FalconeridResult<T> = result::Result<T, FalconeridError>;
