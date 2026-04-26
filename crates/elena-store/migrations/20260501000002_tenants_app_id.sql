-- Add app_id link from tenants to apps.
--
-- Nullable so existing tenants stay valid (additive-only rule). ON DELETE SET
-- NULL — deleting an app must not cascade-destroy its tenants; the admin path
-- already rejects deletes while tenants reference the app.

ALTER TABLE tenants
    ADD COLUMN app_id uuid NULL REFERENCES apps(id) ON DELETE SET NULL;

CREATE INDEX tenants_app_id_idx ON tenants (app_id) WHERE app_id IS NOT NULL;

-- Supports keyset pagination on the new admin "list tenants" endpoint.
CREATE INDEX tenants_created_desc_idx ON tenants (created_at DESC, id DESC);
