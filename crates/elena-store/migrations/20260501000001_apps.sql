-- Apps — admin grouping above tenants.
--
-- Apps (Solen, Hannlys, Omnii, ...) are an admin-only abstraction. The runtime
-- never references app_id on the request path; tenants remain the isolation
-- boundary. The admin panel uses this table to filter, list, and onboard
-- tenants under a named product.

CREATE TABLE apps (
    id                          uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    slug                        text        NOT NULL,
    display_name                text        NOT NULL,
    -- Plan blueprint applied when a tenant is onboarded under this app.
    -- NULL = onboarding falls back to the tenant-tier default budget.
    default_plan_template       jsonb,
    -- Plugin IDs are strings; mirrors tenants.allowed_plugin_ids.
    default_allowed_plugin_ids  text[]      NOT NULL DEFAULT '{}',
    metadata                    jsonb       NOT NULL DEFAULT '{}'::jsonb,
    created_at                  timestamptz NOT NULL DEFAULT now(),
    updated_at                  timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX apps_slug_uidx ON apps (slug);

CREATE TRIGGER apps_updated_at
    BEFORE UPDATE ON apps
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();
