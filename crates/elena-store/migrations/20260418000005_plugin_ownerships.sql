-- Phase 7 · A5 — Per-tenant plugin ownership.
--
-- A plugin_id can belong to many tenants (shared utilities) or exactly
-- one (a creator's private Seed product on Hannlys). The absence of a
-- row for a given `plugin_id` means "global plugin, visible to all
-- tenants" — this keeps Phase-6 registrations working without a backfill.
--
-- See `crates/elena-plugins/src/registry.rs::tools_for` for how this
-- intersects with the per-tenant and per-workspace allow-lists from A4.

CREATE TABLE plugin_ownerships (
    plugin_id  text NOT NULL,
    tenant_id  uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (plugin_id, tenant_id)
);

-- "Which tenants own this plugin?" — the Hannlys creator-isolation path.
CREATE INDEX plugin_ownerships_plugin_idx ON plugin_ownerships (plugin_id);
-- "Which plugins does this tenant own?" — admin listings.
CREATE INDEX plugin_ownerships_tenant_idx ON plugin_ownerships (tenant_id);
