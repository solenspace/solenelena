-- Phase 7 follow-up: per-tenant credential storage.
--
-- Each row holds one tenant's credentials for one plugin, encrypted at
-- rest with the operator's master key (Aes256Gcm). The worker decrypts
-- in-memory just before dispatching a tool call and attaches the
-- key/value pairs to the outgoing gRPC request as `x-elena-cred-<key>`
-- metadata. Connectors prefer the metadata over their startup env, so
-- this table is the substrate for true multi-tenant credential
-- isolation without rebuilding the connector binaries.
--
-- We store ciphertext + nonce as separate `bytea` columns rather than a
-- single combined blob so backup tooling can spot-check that the nonce
-- field is unique per row (each AES-GCM encryption MUST use a fresh
-- nonce; reusing one is catastrophic).

CREATE TABLE tenant_credentials (
    tenant_id      UUID        NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    plugin_id      TEXT        NOT NULL,
    kv_ciphertext  BYTEA       NOT NULL,
    kv_nonce       BYTEA       NOT NULL,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, plugin_id)
);

CREATE INDEX tenant_credentials_plugin_idx
    ON tenant_credentials (plugin_id);
