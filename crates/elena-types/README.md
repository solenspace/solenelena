# elena-types

Foundation types for Elena. Zero async or infrastructure dependencies —
safe to depend on from every other crate without pulling in tokio / sqlx /
fred.

## Modules

| Module | Types |
|--------|-------|
| `id` | `TenantId`, `UserId`, `WorkspaceId`, `ThreadId`, `MessageId`, `ToolCallId`, `SessionId`, `PluginId` |
| `message` | `Message`, `Role`, `MessageKind`, `ContentBlock` (incl. `Thinking`, `RedactedThinking`), `StopReason`, `ToolResultContent`, `ImageSource` |
| `cache` | `CacheControl`, `CacheTtl`, `CacheScope`, `CacheControlKind` |
| `usage` | `Usage`, `CacheCreation`, `ServerToolUse`, `ServiceTier`, `Speed` |
| `terminal` | `Terminal` (15 variants) |
| `error` | `ElenaError`, `LlmApiError` (22 variants) + kinds, `ToolError`, `StoreError`, `ConfigError` |
| `permission` | `Permission`, `PermissionSet`, `PermissionBehavior`, `PermissionMode`, `PermissionRule*` |
| `tenant` | `TenantContext`, `BudgetLimits`, `TenantTier` |
| `model` | `ModelId`, `ModelTier` |
| `stream` | `StreamEvent` |
