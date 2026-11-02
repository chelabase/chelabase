-- The identity service's user and session ids are store-neutral: rename the Zitadel-named
-- columns, and the constraint names Postgres derived from them. Behaviour is unchanged.
-- `tenants.zitadel_org_id` and `project_auth.zitadel_org_id` stay until the Zitadel path goes.
ALTER TABLE members RENAME COLUMN zitadel_user_id TO identity_user_id;
ALTER TABLE members RENAME CONSTRAINT members_tenant_id_zitadel_user_id_key TO members_tenant_id_identity_user_id_key;
ALTER TABLE invites RENAME COLUMN zitadel_user_id TO identity_user_id;
ALTER TABLE setup_claims RENAME COLUMN zitadel_user_id TO identity_user_id;
ALTER TABLE setup_claims RENAME CONSTRAINT setup_claims_zitadel_user_id_check TO setup_claims_identity_user_id_check;
ALTER TABLE app_sessions RENAME COLUMN zitadel_session_id TO identity_session_id;

-- An empty schema owned by the metadata role. With every pool pinned to `search_path=public`,
-- no later role can shadow `public` through a `"$user"` schema.
CREATE SCHEMA IF NOT EXISTS chelabase AUTHORIZATION chelabase;
