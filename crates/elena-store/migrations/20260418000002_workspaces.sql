-- Phase 7 · A2/A4 — Workspaces with per-workspace global instructions and
-- (for A4) an allow-list of plugin IDs scoped to that workspace.
--
-- `threads.workspace_id` is NOT given a FK to this table: pre-Phase-7
-- threads carry ad-hoc workspace IDs that never had a row here. The
-- gateway's behaviour is "row exists → apply it; row missing → no-op",
-- which keeps rollouts free of data-backfill steps.

CREATE TABLE workspaces (
    id                  uuid        PRIMARY KEY,
    tenant_id           uuid        NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name                text,
    global_instructions text        NOT NULL DEFAULT '',
    -- Plugin IDs are strings (see elena-plugins::PluginId), not UUIDs.
    allowed_plugin_ids  text[]      NOT NULL DEFAULT '{}',
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX workspaces_tenant_idx ON workspaces (tenant_id);

CREATE TRIGGER workspaces_updated_at
    BEFORE UPDATE ON workspaces
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();
