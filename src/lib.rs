#![forbid(unsafe_code)]

mod api;
mod auth;
pub mod config;
mod engine;
mod listener;
mod persistence;
mod state;

pub use api::router;
pub use state::{App, StartupError};

pub const LEGACY_API_PREFIX: &str = "/api/v3";
