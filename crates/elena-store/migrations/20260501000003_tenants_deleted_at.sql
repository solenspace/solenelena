-- Soft-delete column on tenants.
--
-- Hard-deleting a tenant cascades through audit_events (FK ON DELETE CASCADE
-- in 20260418000003_audit_events.sql), erasing the compliance trail Solen
-- positions to enterprise buyers. Soft-delete keeps the audit history while
-- removing the tenant from active reads.

ALTER TABLE tenants
    ADD COLUMN deleted_at timestamptz NULL;

CREATE INDEX tenants_active_idx ON tenants (id) WHERE deleted_at IS NULL;
