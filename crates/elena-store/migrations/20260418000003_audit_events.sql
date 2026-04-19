-- Phase 7 · A3 — Append-only audit log.
--
-- Every load-bearing action Elena takes (tool dispatch, LLM call,
-- approval decision, rate-limit rejection, circuit state change) writes
-- one row here. Writers go through a bounded channel so a slow DB never
-- blocks the hot path.
--
-- v1.0 ships a flat table with a `(tenant_id, created_at)` index for
-- retention scans. Daily partitioning lands in v1.0.x once we have a
-- real volume signal — until then the flat table is simpler to operate.

CREATE TABLE audit_events (
    id           uuid        PRIMARY KEY,
    tenant_id    uuid        NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    workspace_id uuid,
    thread_id    uuid,
    actor        text        NOT NULL,
    kind         text        NOT NULL,
    payload      jsonb       NOT NULL DEFAULT '{}'::jsonb,
    created_at   timestamptz NOT NULL DEFAULT now()
);

-- Scan shape: "give me tenant X's events since timestamp Y." Retention
-- deletes hit the same path.
CREATE INDEX audit_events_tenant_time_idx
    ON audit_events (tenant_id, created_at DESC);

-- Secondary shape: thread-scoped forensics. Often NULL for non-loop
-- events, so a partial index keeps it small.
CREATE INDEX audit_events_thread_idx
    ON audit_events (thread_id)
    WHERE thread_id IS NOT NULL;
