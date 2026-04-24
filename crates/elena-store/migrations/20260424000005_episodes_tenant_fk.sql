-- The original episodes migration declared `tenant_id` as a plain `uuid`
-- column without a foreign-key reference. Every other tenant-scoped
-- table cascades on tenant delete via `REFERENCES tenants(id) ON
-- DELETE CASCADE`; episodes silently skipped that link, so deleting
-- a tenant left orphan episode rows behind.
--
-- Surfaced by the tri-tenant fire-test (`bins/elena-tri-tenant-firetest`)
-- when it discovered the operator had no admin path to drop a tenant.
-- Adding the FK here so the cascading `DELETE /admin/v1/tenants/:id`
-- handler that ships in the same change can rely on the schema to do
-- the right thing.
--
-- Idempotent: skips the constraint add if a previous migration race
-- already added it (defensive — `pg_constraint` lookup is cheap).

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM   pg_constraint
        WHERE  conname = 'episodes_tenant_id_fkey'
        AND    conrelid = 'episodes'::regclass
    ) THEN
        ALTER TABLE episodes
            ADD CONSTRAINT episodes_tenant_id_fkey
            FOREIGN KEY (tenant_id)
            REFERENCES tenants(id)
            ON DELETE CASCADE;
    END IF;
END$$;
