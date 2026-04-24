-- S2 — Per-tenant admin scope. A non-NULL `admin_scope_hash` means
-- `/admin/v1/*` calls scoped to this tenant must additionally present
-- `X-Elena-Tenant-Scope: <token>` whose SHA-256 matches the stored
-- bytes. NULL (the default) means "inherit the global admin token" —
-- preserves legacy single-operator behaviour for tenants that don't
-- yet have a per-tenant operator boundary.
--
-- The column stores a raw 32-byte SHA-256 digest (not hex, not
-- base64). The token itself is never persisted; the gateway hashes the
-- supplied value at request time and compares with `subtle`'s
-- constant-time equality.

ALTER TABLE tenants
    ADD COLUMN admin_scope_hash bytea;

-- Length check: SHA-256 is exactly 32 bytes. Forbids the
-- "accidentally stored a hex string" footgun by failing writes that
-- aren't 32 raw bytes.
ALTER TABLE tenants
    ADD CONSTRAINT tenants_admin_scope_hash_len_chk
    CHECK (admin_scope_hash IS NULL OR octet_length(admin_scope_hash) = 32);
