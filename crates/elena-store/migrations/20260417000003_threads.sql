-- Threads — a conversation context scoped to a tenant/user/workspace.

CREATE TABLE threads (
    id              uuid        PRIMARY KEY,
    tenant_id       uuid        NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    user_id         uuid        NOT NULL,
    workspace_id    uuid        NOT NULL,
    title           text,
    metadata        jsonb       NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    last_message_at timestamptz
);

-- Primary list view: "recent threads for this user in this tenant."
CREATE INDEX threads_tenant_user_recent_idx
    ON threads (tenant_id, user_id, last_message_at DESC NULLS LAST);

-- Workspace-scoped listings.
CREATE INDEX threads_workspace_idx
    ON threads (tenant_id, workspace_id);

CREATE TRIGGER threads_updated_at
    BEFORE UPDATE ON threads
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();
