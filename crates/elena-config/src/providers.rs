//! [`ProvidersConfig`] — per-provider credential + endpoint configuration.
//!
//! Operators configure each LLM provider in its own nested section. Elena's
//! worker builds a [`LlmMultiplexer`](elena_llm::LlmMultiplexer) from this
//! config and dispatches per-request by the
//! [`LlmRequest::provider`](elena_llm::LlmRequest) field.

use secrecy::SecretString;
use serde::Deserialize;

use crate::anthropic::AnthropicConfig;

/// Top-level multi-provider configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ProvidersConfig {
    /// Provider name used when [`LlmRequest::provider`](elena_llm::LlmRequest)
    /// isn't recognised. Must match one of the populated sub-sections.
    ///
    /// Leaving this unset defaults to `"anthropic"` for backward compat.
    #[serde(default = "default_provider_name")]
    pub default: String,

    /// Anthropic native (`/v1/messages`, prompt caching). Optional.
    #[serde(default)]
    pub anthropic: Option<AnthropicConfig>,

    /// `OpenRouter` — OpenAI-compatible aggregator. Optional.
    #[serde(default)]
    pub openrouter: Option<OpenAiCompatProviderEntry>,

    /// Groq — OpenAI-compatible, ultra-fast. Optional.
    #[serde(default)]
    pub groq: Option<OpenAiCompatProviderEntry>,
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self { default: default_provider_name(), anthropic: None, openrouter: None, groq: None }
    }
}

fn default_provider_name() -> String {
    "anthropic".to_owned()
}

/// One entry's worth of OpenAI-compatible provider configuration.
///
/// Intentionally close to `elena_llm::OpenAiCompatConfig` — this is the
/// figment-loaded flavour, constructed once from env/TOML and consumed to
/// build the client.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenAiCompatProviderEntry {
    /// API key sent as `Authorization: Bearer <key>`.
    pub api_key: SecretString,
    /// Base URL ending at the provider's chat-completions root
    /// (e.g. `https://openrouter.ai/api/v1` or
    /// `https://api.groq.com/openai/v1`). Client appends
    /// `/chat/completions`.
    pub base_url: String,
    /// Per-attempt HTTP timeout (ms). `None` = no cap.
    #[serde(default)]
    pub request_timeout_ms: Option<u64>,
    /// Connect timeout (ms). Default `10_000`.
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    /// Max retry attempts per request. Default 3.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
}

const fn default_connect_timeout_ms() -> u64 {
    10_000
}

const fn default_max_attempts() -> u32 {
    3
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn default_providers_is_empty() {
        let cfg = ProvidersConfig::default();
        assert_eq!(cfg.default, "anthropic");
        assert!(cfg.anthropic.is_none());
        assert!(cfg.openrouter.is_none());
        assert!(cfg.groq.is_none());
    }

    #[test]
    fn openrouter_entry_deserialises_from_partial_json() {
        let raw = serde_json::json!({
            "default": "openrouter",
            "openrouter": {
                "api_key": "sk-or-v1-abc",
                "base_url": "https://openrouter.ai/api/v1"
            }
        });
        let cfg: ProvidersConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(cfg.default, "openrouter");
        let or = cfg.openrouter.expect("openrouter present");
        assert_eq!(or.api_key.expose_secret(), "sk-or-v1-abc");
        assert_eq!(or.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(or.connect_timeout_ms, 10_000);
        assert_eq!(or.max_attempts, 3);
    }
}
