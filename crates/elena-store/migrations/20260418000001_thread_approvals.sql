-- Phase 7 · A1 — Per-tool approvals submitted by the client while the loop
-- is parked in `LoopPhase::AwaitingApproval`.
--
-- Written by the gateway in `POST /v1/threads/:id/approvals`, read by the
-- worker on resume to build the filtered tool-execution batch. A row's
-- absence at resume time means "still waiting"; a completed decision set
-- unlocks the loop.

CREATE TABLE thread_approvals (
    thread_id       uuid        NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    tool_use_id     uuid        NOT NULL,
    tenant_id       uuid        NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    decision        text        NOT NULL CHECK (decision IN ('allow', 'deny')),
    edits           jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (thread_id, tool_use_id)
);

-- Approvals live only until the worker has consumed them; the worker
-- clears them when transitioning out of AwaitingApproval. Index supports
-- the "give me this thread's pending decisions" fetch.
CREATE INDEX thread_approvals_thread_idx
    ON thread_approvals (thread_id);
