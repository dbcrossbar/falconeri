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

pub use base64;
pub use cast;
pub use chrono;
pub use diesel_async;
pub use futures_util;
pub use handlebars;
pub use rand;
pub use schemars;
pub use semver;
pub use serde;
pub use serde_json;
pub use tokio;
pub use tracing;
pub use uuid;

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
    pub use std::{
        collections::HashMap,
        fmt,
        fs::File,
        io::Write,
        path::{Path, PathBuf},
    };

    pub use anyhow::{format_err, Context};
    pub use chrono::{NaiveDateTime, Utc};
    pub use diesel::{self, prelude::*};
    pub use diesel_async::AsyncPgConnection;
    pub use serde::{Deserialize, Serialize};
    pub use tracing::{
        debug, debug_span, error, error_span, info, info_span, instrument, trace,
        trace_span, warn, warn_span,
    };
    pub use uuid::Uuid;

    pub use super::{connect_via::ConnectVia, models::*, Error, Result};
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
