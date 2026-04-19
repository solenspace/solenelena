-- Budget state — per-tenant running counters for quota enforcement.

CREATE TABLE budget_state (
    tenant_id           uuid        PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE,
    tokens_used_today   bigint      NOT NULL DEFAULT 0 CHECK (tokens_used_today >= 0),
    day_rollover_at     timestamptz NOT NULL DEFAULT (date_trunc('day', now() + interval '1 day')),
    threads_active      integer     NOT NULL DEFAULT 0 CHECK (threads_active >= 0),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER budget_state_updated_at
    BEFORE UPDATE ON budget_state
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();
