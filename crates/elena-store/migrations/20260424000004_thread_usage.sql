-- C2 — Cumulative per-thread token usage.
--
-- The Phase-7 budget check at step.rs:784 compares state.usage.total()
-- against tenant.budget.max_tokens_per_thread, but state.usage is
-- per-loop-run (reset to zero on every WorkRequestKind::Turn). So a
-- thread that spans many turns evaluates the per-thread cap as if it
-- were per-turn.
--
-- This table stores the running per-thread token sum that the worker
-- loads at fresh-start so the budget check evaluates the *real*
-- per-thread total. Reset / cleanup is the operator's call (TTL via
-- DELETE policy or just leave forever — token counts are small).

CREATE TABLE thread_usage (
    tenant_id    uuid        NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    thread_id    uuid        NOT NULL,
    tokens_used  bigint      NOT NULL DEFAULT 0 CHECK (tokens_used >= 0),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, thread_id)
);

CREATE INDEX thread_usage_tenant_idx ON thread_usage (tenant_id);

CREATE TRIGGER thread_usage_updated_at
    BEFORE UPDATE ON thread_usage
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();
