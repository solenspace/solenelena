-- Phase 7 · A4 — Per-tenant plugin allow-list.
--
-- Plugin IDs are human-readable strings (matching
-- `[a-z0-9][a-z0-9_-]{0,31}`), not UUIDs — see
-- `crates/elena-plugins/src/id.rs`. Empty array means "no tenant-level
-- filter, defer entirely to the workspace allow-list (or global
-- registry if that is also empty)". The `PluginRegistry::tools_for`
-- filter intersects this with the workspace allow-list.

ALTER TABLE tenants
    ADD COLUMN IF NOT EXISTS allowed_plugin_ids text[] NOT NULL DEFAULT '{}';
