//! Foundation types for Elena.
//!
//! This crate defines the core vocabulary the rest of Elena operates on:
//! branded IDs, messages and content blocks, token usage, prompt-cache
//! controls, permissions, tenant context, terminal conditions, stream events,
//! and the error hierarchy.
//!
//! It has zero async or infrastructure dependencies — only `serde`, `ulid`,
//! `uuid`, `chrono`, `bytes`, and `thiserror`. Every type is `Serialize +
//! Deserialize` so it can cross process boundaries (NATS, gRPC, HTTP) without
//! adapter layers.

// B1.6 — TenantTier + BudgetLimits::DEFAULT_FREE/PRO + default_budget_for_tier
// are #[deprecated] during the JWT-claim transition window. Remove this
// crate-level allow once the deprecated items are deleted.
#![allow(deprecated)]
#![warn(missing_docs)]
// Tests use `unwrap`/`expect` freely as assertions — they fail fast on setup
// issues, and re-implementing each as `?` would muddy intent.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod app;
pub mod autonomy;
pub mod cache;
pub mod error;
pub mod id;
pub mod memory;
pub mod message;
pub mod model;
pub mod permission;
pub mod plan;
pub mod stream;
pub mod tenant;
pub mod terminal;
pub mod transport;
pub mod usage;

pub use app::{App, AppSlug};
pub use autonomy::{ApprovalDecision, ApprovalVerdict, AutonomyMode, PendingApproval};
pub use cache::{CacheControl, CacheControlKind, CacheScope, CacheTtl};
pub use error::{ConfigError, ElenaError, LlmApiError, LlmApiErrorKind, StoreError, ToolError};
pub use id::{
    AppId, EpisodeId, IdParseError, MessageId, PlanAssignmentId, PlanId, PluginId, RequestId,
    SessionId, TenantId, ThreadId, ToolCallId, UserId, WorkspaceId,
};
pub use memory::Outcome;
pub use message::{
    ContentBlock, ImageSource, Message, MessageKind, Role, StopReason, SystemMessageKind,
    ToolResultContent,
};
pub use model::{ModelId, ModelTier};
pub use permission::{
    Permission, PermissionBehavior, PermissionDecisionReason, PermissionMode, PermissionRule,
    PermissionRuleSource, PermissionRuleValue, PermissionSet, PermissionUpdate,
    PermissionUpdateDestination,
};
pub use plan::{Plan, PlanAssignment, PlanSlug, ResolvedPlan};
pub use stream::{StreamEnvelope, StreamEvent};
pub use tenant::{BudgetLimits, TenantContext, TenantTier};
pub use terminal::Terminal;
pub use transport::{WorkRequest, WorkRequestKind, subjects};
pub use usage::{CacheCreation, ServerToolUse, ServiceTier, Speed, Usage};
