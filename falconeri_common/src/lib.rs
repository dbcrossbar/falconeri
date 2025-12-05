//! Code shared between various Falconeri tools.

#![deny(unsafe_code)]
#![warn(missing_docs)]
// Silence diesel warnings: https://github.com/diesel-rs/diesel/pull/1787
#![allow(proc_macro_derive_resolution_fallback)]
// If we do this, it's generally deliberate (because a future version of the
// struct might contain floats, which don't support `Eq`).
#![allow(clippy::derive_partial_eq_without_eq)]

// Keep `macro_use` for `diesel` until it's easier to use Rust 2018 macro
// imports with it.
#[macro_use]
pub extern crate diesel;
pub extern crate diesel_migrations;

pub use cast;
pub use chrono;
pub use rand;
pub use semver;
pub use serde_json;

pub mod connect_via;
pub mod db;
pub mod kubernetes;
pub mod manifest;
pub mod models;
pub mod pipeline;
pub mod rest_api;
mod schema;
pub mod secret;
pub mod storage;
pub mod tracing_support;

/// Common imports used by many modules.
pub mod prelude {
    pub use anyhow::{format_err, Context};
    pub use chrono::{NaiveDateTime, Utc};
    pub use diesel::{self, prelude::*, PgConnection};
    pub use diesel_async::AsyncPgConnection;
    pub use serde::{Deserialize, Serialize};
    pub use std::{
        collections::HashMap,
        fmt,
        fs::File,
        io::Write,
        path::{Path, PathBuf},
    };
    pub use tracing::{
        debug, debug_span, error, error_span, info, info_span, instrument, trace,
        trace_span, warn, warn_span,
    };
    pub use uuid::Uuid;

    pub use super::connect_via::ConnectVia;
    pub use super::models::*;
    pub use super::{Error, Result};
}

/// Error type for this crate's functions.
pub use anyhow::Error;

/// Result type for this crate's functions.
pub use anyhow::Result;

/// The version of `falconeri_common` that we're using. This can be used
/// to make sure that our various clients and servers match.
pub fn falconeri_common_version() -> semver::Version {
    env!("CARGO_PKG_VERSION")
        .parse::<semver::Version>()
        .expect("could not parse built-in version")
}

/// Initialize OpenSSL certificate paths by probing the system.
///
/// This should be called early in main(), before spawning threads or making
/// TLS connections. It's a safe wrapper around `openssl_probe::probe()`.
pub fn init_openssl_probe() {
    use std::env;

    let result = openssl_probe::probe();

    if let Some(cert_file) = result.cert_file {
        if env::var_os("SSL_CERT_FILE").is_none() {
            env::set_var("SSL_CERT_FILE", cert_file);
        }
    }

    if let Some(cert_dir) = result.cert_dir {
        if env::var_os("SSL_CERT_DIR").is_none() {
            env::set_var("SSL_CERT_DIR", cert_dir);
        }
    }
}
