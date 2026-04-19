//! [`ToolContext`] — what a tool sees when it runs.
//!
//! Cheap to clone (`tokio::sync::mpsc::Sender` is an `Arc` internally,
//! `CancellationToken` is `Arc<Mutex<..>>`, `TenantContext` is plain-old-data).
//! The orchestrator hands one of these to each concurrent tool, so the tools
//! themselves never need to lock or share state to fan progress events out.

use std::collections::BTreeMap;

use elena_types::{StreamEvent, TenantContext, ThreadId, ToolCallId};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Everything a tool needs to run one call.
#[derive(Clone, Debug)]
pub struct ToolContext {
    /// The tenant this call belongs to.
    pub tenant: TenantContext,
    /// The thread the model is working inside.
    pub thread_id: ThreadId,
    /// The model-supplied tool-call ID — tools echo this in any
    /// `ToolProgress` events they emit.
    pub tool_call_id: ToolCallId,
    /// Per-call cancellation.
    pub cancel: CancellationToken,
    /// Progress channel. Tools may fan-out `ToolProgress` events here while
    /// they work. Dropping is fine: the receiver may have gone away, in which
    /// case the `send` call returns an error the tool can ignore.
    pub progress_tx: mpsc::Sender<StreamEvent>,
    /// Per-tenant credentials decrypted from `tenant_credentials` for the
    /// plugin owning this tool. Empty for built-in tools and for plugin
    /// tools when no row exists for `(tenant_id, plugin_id)` — connectors
    /// fall back to their startup env in that case. Populated by the
    /// loop driver immediately before [`Tool::execute`].
    pub credentials: BTreeMap<String, String>,
}
