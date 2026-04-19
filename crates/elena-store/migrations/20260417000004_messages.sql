-- Messages — individual rows in a thread. One row per message event
-- (user turn, assistant turn, tool_result, system notice).

CREATE TABLE messages (
    id          uuid         PRIMARY KEY,
    thread_id   uuid         NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    tenant_id   uuid         NOT NULL,
    parent_id   uuid         REFERENCES messages(id) ON DELETE SET NULL,
    role        text         NOT NULL CHECK (role IN ('user', 'assistant', 'system', 'tool')),
    kind        jsonb        NOT NULL,
    content     jsonb        NOT NULL,
    token_count integer      CHECK (token_count IS NULL OR token_count >= 0),
    embedding   vector(384),
    created_at  timestamptz  NOT NULL DEFAULT now()
);

-- Chronological order within a thread.
CREATE INDEX messages_thread_created_idx
    ON messages (thread_id, created_at);

-- Tenant-scoped audit views.
CREATE INDEX messages_tenant_created_idx
    ON messages (tenant_id, created_at);

-- HNSW index for cosine-similarity search on embeddings.
-- Partial on non-null embeddings so Phase 1 NULL rows don't bloat the index.
-- m=16 and ef_construction=64 are pgvector defaults — good quality/build tradeoff.
CREATE INDEX messages_embedding_hnsw_idx
    ON messages USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64)
    WHERE embedding IS NOT NULL;
