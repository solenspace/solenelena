-- Tenants — one row per application connected to Elena.

CREATE TABLE tenants (
    id            uuid        PRIMARY KEY,
    name          text        NOT NULL,
    tier          text        NOT NULL CHECK (tier IN ('free', 'pro', 'team', 'enterprise')),
    budget        jsonb       NOT NULL,
    permissions   jsonb       NOT NULL DEFAULT '{}'::jsonb,
    metadata      jsonb       NOT NULL DEFAULT '{}'::jsonb,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX tenants_tier_idx ON tenants (tier);

-- Update `updated_at` on any UPDATE.
CREATE OR REPLACE FUNCTION set_updated_at()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER tenants_updated_at
    BEFORE UPDATE ON tenants
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();
