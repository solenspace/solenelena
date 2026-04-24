-- B1 — Plan assignments. Resolution at request time runs most-specific-wins
-- across this table; the tenant default (no user, no workspace) is *not*
-- stored here — it lives on the plans row marked is_default = true. So this
-- table only holds rules 1, 2, and 3 from the resolver:
--
--     1. (tenant, user,    workspace)  exact override
--     2. (tenant, user,    NULL)       user default across workspaces
--     3. (tenant, NULL,    workspace)  workspace default for any user
--
-- A row with both user_id IS NULL and workspace_id IS NULL is rejected by
-- CHECK so callers cannot accidentally shadow the tenant default.

CREATE TABLE plan_assignments (
    id            uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     uuid        NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    user_id       uuid,
    workspace_id  uuid,
    -- ON DELETE RESTRICT enforces the "cannot delete an assigned plan"
    -- invariant the admin DELETE /plans/:id route depends on. Postgres
    -- raises foreign_key_violation; the admin route translates it to 409.
    plan_id       uuid        NOT NULL REFERENCES plans(id) ON DELETE RESTRICT,
    effective_at  timestamptz NOT NULL DEFAULT now(),
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    -- Forbid the all-NULL row that would shadow the tenant default and
    -- create resolver ambiguity with rule 4.
    CONSTRAINT plan_assignments_scope_chk
        CHECK (user_id IS NOT NULL OR workspace_id IS NOT NULL)
);

-- One row per (tenant, user, workspace) shape combination. Each shape gets
-- its own partial unique index because Postgres treats NULLs as distinct
-- by default (NULLS NOT DISTINCT is PG15+; we target PG13+).
CREATE UNIQUE INDEX plan_assignments_user_workspace_uidx
    ON plan_assignments (tenant_id, user_id, workspace_id)
    WHERE user_id IS NOT NULL AND workspace_id IS NOT NULL;

CREATE UNIQUE INDEX plan_assignments_user_only_uidx
    ON plan_assignments (tenant_id, user_id)
    WHERE user_id IS NOT NULL AND workspace_id IS NULL;

CREATE UNIQUE INDEX plan_assignments_workspace_only_uidx
    ON plan_assignments (tenant_id, workspace_id)
    WHERE user_id IS NULL AND workspace_id IS NOT NULL;

-- Resolver path: fetch all assignments for a tenant ordered by specificity.
-- Tenant-scoped index keeps the resolver's per-request scan bounded.
CREATE INDEX plan_assignments_tenant_idx ON plan_assignments (tenant_id);

-- Drop-cascade safety: when a plan is deleted via cascade-from-tenant,
-- assignments go too. The ON DELETE RESTRICT above only fires for direct
-- DELETE FROM plans WHERE id = ... — the intended admin path.
CREATE INDEX plan_assignments_plan_idx ON plan_assignments (plan_id);

CREATE TRIGGER plan_assignments_updated_at
    BEFORE UPDATE ON plan_assignments
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();
