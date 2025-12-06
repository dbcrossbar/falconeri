//! Various axum-related utilities.

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
};
use falconeri_common::base64::{prelude::BASE64_STANDARD, Engine};
use falconeri_common::{db, prelude::*};
use std::result;

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
        let conn = state
            .pool
            .get()
            .await
            .map_err(|e| FalconeridError(format_err!("pool error: {}", e)))?;
        Ok(DbConn(conn))
    }
}

/// An error type for `falconerid`. Ideally, this should be an enum with members
/// like `NotFound` and `Other`, which would allow us to send 404 responses,
/// etc. But for now it's just a wrapper.
#[derive(Debug)]
pub struct FalconeridError(pub Error);

impl IntoResponse for FalconeridError {
    fn into_response(self) -> Response {
        // Log our full error with the error chain using Debug formatting.
        error!("{:?}", self.0);

        // Put the error message in the payload for now. This might become JSON
        // in the future. Use Display to avoid leaking backtraces to clients.
        let payload = format!("{}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, payload).into_response()
    }
}

impl From<Error> for FalconeridError {
    fn from(err: Error) -> Self {
        FalconeridError(err)
    }
}

/// The result type of `falconerid` handler.
pub type FalconeridResult<T> = result::Result<T, FalconeridError>;
