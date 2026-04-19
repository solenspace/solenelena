//! Phase 2 smoke binary — streams a tiny completion from the real
//! Anthropic Messages API.
//!
//! Reads `ANTHROPIC_API_KEY` from the environment. If it's unset, the
//! binary exits 2 with a clear message (so CI can skip this test without
//! failing). If the stream completes cleanly, exits 0.
//!
//! Run with:
//! ```bash
//! ANTHROPIC_API_KEY=sk-ant-... cargo run -p elena-phase2-smoke
//! ```

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::too_many_lines)]

use std::collections::HashMap;
use std::io::Write as _;
use std::process::ExitCode;

use chrono::Utc;
use elena_config::AnthropicConfig;
use elena_llm::{
    AnthropicAuth, AnthropicClient, CacheAllowlist, CachePolicy, LlmRequest, RequestOptions,
};
use elena_types::{
    BudgetLimits, ContentBlock, Message, MessageId, MessageKind, ModelId, PermissionSet, Role,
    SessionId, StreamEvent, TenantContext, TenantId, TenantTier, Terminal, ThreadId, UserId,
    WorkspaceId,
};
use futures::StreamExt;
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let Ok(key) = std::env::var("ANTHROPIC_API_KEY") else {
        eprintln!(
            "elena-phase2-smoke: SKIP — ANTHROPIC_API_KEY is not set.\n\
             To run the smoke against the real API:\n    \
             ANTHROPIC_API_KEY=sk-ant-... cargo run -p elena-phase2-smoke"
        );
        return ExitCode::from(2);
    };

    match run(key).await {
        Ok(()) => {
            println!("\nelena-phase2-smoke: OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nelena-phase2-smoke: FAIL — {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(api_key: String) -> anyhow::Result<()> {
    let model = std::env::var("ELENA_SMOKE_MODEL")
        .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_owned());

    let cfg = AnthropicConfig {
        api_key: SecretString::from(api_key.clone()),
        base_url: std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".to_owned()),
        api_version: "2023-06-01".to_owned(),
        request_timeout_ms: Some(60_000),
        connect_timeout_ms: 10_000,
        max_attempts: 3,
    };

    let client = AnthropicClient::new(&cfg, AnthropicAuth::ApiKey(SecretString::from(api_key)))?;
    let req = tiny_request(model);
    let policy = CachePolicy::new(TenantTier::Pro, CacheAllowlist::default());

    println!("Prompt: \"Say hi in five words.\"\n");
    print!("Reply:  ");
    std::io::stdout().flush().ok();

    let cancel = CancellationToken::new();
    let mut stream = client.stream(req, policy, cancel);
    let mut got_text = false;
    let mut terminal: Option<Terminal> = None;

    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta { delta } => {
                got_text = true;
                print!("{delta}");
                std::io::stdout().flush().ok();
            }
            StreamEvent::UsageUpdate(u) => {
                tracing::debug!(
                    input = u.input_tokens,
                    output = u.output_tokens,
                    cache_read = u.cache_read_input_tokens,
                    "usage update",
                );
            }
            StreamEvent::Done(t) => {
                terminal = Some(t);
                break;
            }
            StreamEvent::Error(err) => {
                anyhow::bail!("stream error: {err}");
            }
            _ => {}
        }
    }

    match terminal {
        Some(Terminal::Completed) if got_text => Ok(()),
        Some(Terminal::Completed) => anyhow::bail!("completed but no text received"),
        Some(t) => anyhow::bail!("non-success terminal: {t:?}"),
        None => anyhow::bail!("stream ended without terminal event"),
    }
}

fn tiny_request(model: String) -> LlmRequest {
    LlmRequest {
        provider: "anthropic".to_owned(),
        model: ModelId::new(model),
        tenant: TenantContext {
            tenant_id: TenantId::new(),
            user_id: UserId::new(),
            workspace_id: WorkspaceId::new(),
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            permissions: PermissionSet::default(),
            budget: BudgetLimits::default(),
            tier: TenantTier::Pro,
            metadata: HashMap::new(),
        },
        messages: vec![Message {
            id: MessageId::new(),
            thread_id: ThreadId::new(),
            tenant_id: TenantId::new(),
            role: Role::User,
            kind: MessageKind::User,
            content: vec![ContentBlock::Text {
                text: "Say hi in exactly five words.".into(),
                cache_control: None,
            }],
            created_at: Utc::now(),
            token_count: None,
            parent_id: None,
        }],
        system: vec![],
        tools: vec![],
        tool_choice: None,
        max_tokens: 64,
        thinking: None,
        temperature: Some(0.0),
        top_p: None,
        stop_sequences: vec![],
        options: RequestOptions {
            query_source: Some("sdk".into()),
            enable_prompt_caching: true,
            ..RequestOptions::default()
        },
    }
}
