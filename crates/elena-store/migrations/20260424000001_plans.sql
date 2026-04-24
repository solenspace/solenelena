-- B1 — App-defined plans replace the hard-coded TenantTier enum.
--
-- A "plan" is a tenant-scoped policy bundle. Each tenant (Solen, Hannlys,
-- Omnii, ...) defines its own plans via the admin API; users / workspaces
-- get assigned to a plan via plan_assignments. Resolution order at request
-- time is most-specific-wins:
--
--     1. (tenant, user, workspace) exact match in plan_assignments
--     2. (tenant, user, NULL)      user default across workspaces
--     3. (tenant, NULL, workspace) workspace default for any user
--     4. (tenant, NULL, NULL)      tenant default = plans.is_default = true
--
-- Schema-light JSONB columns hold policy bundles whose Rust shapes evolve
-- in elena-types; the DB only enforces (a) tenant-scoped slug uniqueness,
-- (b) at most one default plan per tenant, and (c) a CHECK on the autonomy
-- string so a typo in admin input fails loudly at write time.

CREATE TABLE plans (
    id                       uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id                uuid        NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    slug                     text        NOT NULL,
    display_name             text        NOT NULL,
    is_default               boolean     NOT NULL DEFAULT false,
    budget                   jsonb       NOT NULL,
    rate_limits              jsonb       NOT NULL DEFAULT '{}'::jsonb,
    -- Plugin IDs are strings (see elena-plugins::PluginId), not UUIDs;
    -- mirrors the shape used in tenants.allowed_plugin_ids and
    -- workspaces.allowed_plugin_ids.
    allowed_plugin_ids       text[]      NOT NULL DEFAULT '{}',
    -- NULL = inherit tenant/system tier_models config. Non-null = full
    -- per-plan override of {fast, standard, premium} model selection.
    tier_models              jsonb,
    autonomy_default         text        NOT NULL DEFAULT 'moderate'
        CHECK (autonomy_default IN ('cautious', 'moderate', 'yolo')),
    cache_policy             jsonb       NOT NULL DEFAULT '{}'::jsonb,
    max_cascade_escalations  integer     NOT NULL DEFAULT 1
        CHECK (max_cascade_escalations >= 0),
    metadata                 jsonb       NOT NULL DEFAULT '{}'::jsonb,
    created_at               timestamptz NOT NULL DEFAULT now(),
    updated_at               timestamptz NOT NULL DEFAULT now()
);

-- Slugs are tenant-scoped — Solen's "starter" and Hannlys's "starter" are
-- distinct rows and must not collide.
CREATE UNIQUE INDEX plans_tenant_slug_uidx ON plans (tenant_id, slug);

-- Look-up by tenant for "list this tenant's plans".
CREATE INDEX plans_tenant_idx ON plans (tenant_id);

-- At most one default plan per tenant. Partial unique index keeps the
-- invariant enforceable from concurrent writers without app-level locks.
CREATE UNIQUE INDEX plans_one_default_per_tenant ON plans (tenant_id) WHERE is_default;

-- updated_at maintained by the shared set_updated_at() trigger function
-- defined in 20260417000002_tenants.sql.
CREATE TRIGGER plans_updated_at
    BEFORE UPDATE ON plans
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();

-- Backfill: one default plan per existing tenant, slug = current tier
-- string, budget cloned from the tenant row, allowed_plugin_ids cloned
-- from the tenant row. Pre-existing tenants resolve their default plan
-- (rule 4 above) without any plan_assignments rows being written.
INSERT INTO plans (
    tenant_id,
    slug,
    display_name,
    is_default,
    budget,
    allowed_plugin_ids
)
SELECT
    id,
    tier,
    initcap(tier),
    true,
    budget,
    allowed_plugin_ids
FROM tenants;
