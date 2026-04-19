-- Enable required Postgres extensions.
--
-- `vector`   — pgvector for embedding similarity search.
-- `pgcrypto` — gen_random_uuid() as a fallback when the app doesn't supply one.

CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pgcrypto;
