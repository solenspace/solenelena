-- Episodes — cross-session memory.
--
-- Phase 4 (`elena-memory`) populates this. Phase 1 defines the schema so
-- later phases don't have to migrate an existing row population.

CREATE TABLE episodes (
    id           uuid         PRIMARY KEY,
    tenant_id    uuid         NOT NULL,
    workspace_id uuid         NOT NULL,
    task_summary text         NOT NULL,
    actions      jsonb        NOT NULL,
    outcome      jsonb        NOT NULL,
    embedding    vector(384),
    created_at   timestamptz  NOT NULL DEFAULT now()
);

-- Workspace-scoped recency scans.
CREATE INDEX episodes_workspace_recent_idx
    ON episodes (tenant_id, workspace_id, created_at DESC);

-- HNSW index for similarity recall (same config as messages).
CREATE INDEX episodes_embedding_hnsw_idx
    ON episodes USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64)
    WHERE embedding IS NOT NULL;
