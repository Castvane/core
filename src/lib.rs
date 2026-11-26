#![forbid(unsafe_code)]

mod api;
mod auth;
pub mod config;
mod engine;
mod http_server;
mod listener;
pub use http_server::serve_http;
mod media;
mod persistence;
mod state;
mod tls;
pub use tls::TlsPeer;

pub use api::router;
pub use state::{App, StartupError};

pub const LEGACY_API_PREFIX: &str = "/api/v3";
