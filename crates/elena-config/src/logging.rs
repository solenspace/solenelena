//! Logging / tracing configuration.

use serde::Deserialize;

/// Logging settings.
#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    /// `tracing` subscriber filter directive (e.g. `"info"`, `"elena=debug,warn"`).
    ///
    /// Any value accepted by [`tracing_subscriber::EnvFilter`] works here.
    #[serde(default = "default_level")]
    pub level: String,

    /// Output format.
    #[serde(default)]
    pub format: LogFormat,
}

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Single-line JSON per event (production default).
    Json,
    /// Human-friendly multi-line format (dev default).
    #[default]
    Pretty,
}

fn default_level() -> String {
    "info".to_owned()
}
