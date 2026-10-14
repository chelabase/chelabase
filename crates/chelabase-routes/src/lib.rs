//! Public HTTP endpoints of Chelabase services.
//!
//! One declaration per endpoint, shared by the gateway (routing + policy) and
//! the owning service (axum route paths), so the two cannot drift. Path
//! templates use axum's `{param}` syntax.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }

    /// Parse an HTTP method token. Tokens no endpoint can declare (`OPTIONS`,
    /// `HEAD`, lowercase variants, …) return `None` and never match.
    pub fn parse(token: &str) -> Option<Method> {
        match token {
            "GET" => Some(Method::Get),
            "POST" => Some(Method::Post),
            "PUT" => Some(Method::Put),
            "PATCH" => Some(Method::Patch),
            "DELETE" => Some(Method::Delete),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Service {
    Orchestrator,
    Auth,
    Rest,
}

impl Service {
    /// Stable service name; also the `aud` of internal identity tokens.
    pub const fn name(self) -> &'static str {
        match self {
            Service::Orchestrator => "orchestrator",
            Service::Auth => "auth",
            Service::Rest => "rest",
        }
    }
}

/// Which family of hostnames an endpoint is served on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The API hostname family (the gateway's host registry, `CHELABASE_API_HOST`): management, login and data routes.
    Api,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// No credential required.
    Public,
    /// Zitadel-issued management token (developers / operators).
    Management,
    /// A data API key of either kind (`chk_pub_` or `chk_sec_`).
    DataKey,
    /// Only a secret data API key.
    SecretDataKey,
    /// A public data key; a secret key is refused.
    PublicDataKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cors {
    None,
    /// Callable from the Studio origin (the gateway's host registry, `CHELABASE_STUDIO_HOST`).
    Studio,
    /// Callable from the Studio origins with cookies (credentials); never for a "*" setting.
    StudioCredentials,
    /// Any origin: `Access-Control-Allow-Origin: *`, never with credentials.
    Any,
}

pub use chelabase_types::access::{Access, PlatformRole};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub auth: Auth,
    pub cors: Cors,
    /// Who may call it (the members-and-roles design, part 4). Required, so a route without
    /// an access level doesn't compile.
    pub access: Access,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    /// Stable name used in logs.
    pub name: &'static str,
    pub method: Method,
    pub path: &'static str,
    pub scope: Scope,
    pub service: Service,
    pub policy: Policy,
}

pub mod orchestrator {
    use super::PlatformRole::{Admin, Developer, Viewer};
    use super::{Access, Auth, Cors, Endpoint, Method, Policy, Scope, Service};

    /// Branch-changing routes: Developer on an unprotected branch, Admin on a protected one.
    const BRANCH: Access = Access::Branch {
        unprotected: Developer,
        protected: Admin,
    };

    pub const HEALTH: Endpoint = public_get("orchestrator.health", "/health");

    /// The Management API's OpenAPI document.
    pub const OPENAPI_JSON: Endpoint = public_get("orchestrator.openapi_json", "/v1/openapi.json");

    const fn public_get(name: &'static str, path: &'static str) -> Endpoint {
        Endpoint {
            name,
            method: Method::Get,
            path,
            scope: Scope::Api,
            service: Service::Orchestrator,
            policy: Policy {
                auth: Auth::Public,
                cors: Cors::None,
                access: Access::Public,
            },
        }
    }

    /// Creates a database in a project. The project comes from the request body, not the
    /// path: the access check resolves it there, and a missing or foreign project is 404.
    pub const CREATE_DATABASE: Endpoint =
        key_endpoint("orchestrator.create_database", Method::Post, "/v1/databases", Access::Project(Admin));

    pub const GET_DATABASE: Endpoint =
        key_endpoint("orchestrator.get_database", Method::Get, "/v1/databases/{id}", Access::Project(Viewer));

    /// Owner credentials of a ready database (re-revealable).
    pub const DATABASE_CONNECTION: Endpoint = key_endpoint(
        "orchestrator.database_connection",
        Method::Get,
        "/v1/databases/{id}/connection",
        Access::Project(Admin),
    );

    pub const RESET_PASSWORD: Endpoint = key_endpoint(
        "orchestrator.reset_password",
        Method::Post,
        "/v1/databases/{id}/reset-password",
        Access::Project(Admin),
    );

    pub const DELETE_DATABASE: Endpoint =
        key_endpoint("orchestrator.delete_database", Method::Delete, "/v1/databases/{id}", Access::Project(Admin));

    /// The database's branches (a flat list; `parent_branch_id` makes the tree).
    pub const LIST_BRANCHES: Endpoint = key_endpoint(
        "orchestrator.list_branches",
        Method::Get,
        "/v1/databases/{id}/branches",
        Access::Project(Viewer),
    );

    /// A branch of an existing branch, at a point in its history (created unprotected).
    pub const CREATE_BRANCH: Endpoint = key_endpoint(
        "orchestrator.create_branch",
        Method::Post,
        "/v1/databases/{id}/branches",
        Access::Project(Developer),
    );

    /// Credentials of a ready branch: the owner or a login role. Admin for every role (R22:
    /// grants name NOLOGIN group roles, and a login role's connection string is its password).
    pub const BRANCH_CONNECTION: Endpoint = key_endpoint(
        "orchestrator.branch_connection",
        Method::Get,
        "/v1/databases/{id}/branches/{branch_id}/connection",
        Access::Project(Admin),
    );

    pub const BRANCH_RESET_PASSWORD: Endpoint = key_endpoint(
        "orchestrator.branch_reset_password",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/reset-password",
        Access::Project(Admin),
    );

    pub const BRANCH_RESET_TO_PARENT: Endpoint = key_endpoint(
        "orchestrator.branch_reset_to_parent",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/reset-to-parent",
        BRANCH,
    );

    /// Rewinds a branch to a point, keeping the old timeline as a backup branch.
    pub const BRANCH_RESTORE: Endpoint = key_endpoint(
        "orchestrator.branch_restore",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/restore",
        BRANCH,
    );

    pub const DELETE_BRANCH: Endpoint = key_endpoint(
        "orchestrator.delete_branch",
        Method::Delete,
        "/v1/databases/{id}/branches/{branch_id}",
        BRANCH,
    );

    /// Marks a branch protected or not (protect, unprotect: Admin). A future rename is
    /// `Branch(Developer, Admin)` in the design, so it needs its own route.
    pub const UPDATE_BRANCH: Endpoint = key_endpoint(
        "orchestrator.update_branch",
        Method::Patch,
        "/v1/databases/{id}/branches/{branch_id}",
        Access::Project(Admin),
    );

    /// The branch's guard exemptions: login roles the "guard login roles" switch leaves alone.
    pub const LIST_GUARD_EXEMPTIONS: Endpoint = key_endpoint(
        "orchestrator.list_guard_exemptions",
        Method::Get,
        "/v1/databases/{id}/branches/{branch_id}/guard-exemptions",
        Access::Project(Developer),
    );

    /// Exempts a role, by name, from the branch's login-role guard (idempotent).
    pub const PUT_GUARD_EXEMPTION: Endpoint = key_endpoint(
        "orchestrator.put_guard_exemption",
        Method::Put,
        "/v1/databases/{id}/branches/{branch_id}/guard-exemptions/{role}",
        Access::Project(Admin),
    );

    /// Removes a role's exemption (idempotent).
    pub const DELETE_GUARD_EXEMPTION: Endpoint = key_endpoint(
        "orchestrator.delete_guard_exemption",
        Method::Delete,
        "/v1/databases/{id}/branches/{branch_id}/guard-exemptions/{role}",
        Access::Project(Admin),
    );

    /// The branch's review: code owned by non-Admin roles that can run in someone else's
    /// session, across its databases.
    pub const GET_BRANCH_REVIEW: Endpoint = key_endpoint(
        "orchestrator.get_branch_review",
        Method::Get,
        "/v1/databases/{id}/branches/{branch_id}/review",
        Access::Project(Developer),
    );

    /// Stores the current review as acknowledged.
    pub const ACKNOWLEDGE_BRANCH_REVIEW: Endpoint = key_endpoint(
        "orchestrator.acknowledge_branch_review",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/review/acknowledge",
        Access::Project(Admin),
    );

    /// Reassigns one review item to the owner, or drops it.
    pub const FIX_BRANCH_REVIEW_ITEM: Endpoint = key_endpoint(
        "orchestrator.fix_branch_review_item",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/review/fix",
        Access::Project(Admin),
    );

    /// The branch's Postgres databases: the main one first.
    pub const LIST_BRANCH_DATABASES: Endpoint = key_endpoint(
        "orchestrator.list_branch_databases",
        Method::Get,
        "/v1/databases/{id}/branches/{branch_id}/databases",
        Access::Project(Viewer),
    );

    /// Database settings: the history window and the branch cap.
    pub const UPDATE_DATABASE: Endpoint =
        key_endpoint("orchestrator.update_database", Method::Patch, "/v1/databases/{id}", Access::Project(Admin));

    /// The caller, their tenant, projects and databases (Studio landing page).
    pub const ME: Endpoint = key_endpoint("orchestrator.me", Method::Get, "/v1/me", Access::SignedIn);

    /// The branch's live data API keys (the public key in full).
    pub const LIST_KEYS: Endpoint = key_endpoint(
        "orchestrator.list_keys",
        Method::Get,
        "/v1/databases/{id}/branches/{branch_id}/keys",
        Access::Project(Viewer),
    );

    /// A new secret key, shown once. Keys: Developer for a key bound to an allowed role, Admin
    /// for any key (the handler refines).
    pub const CREATE_KEY: Endpoint = key_endpoint(
        "orchestrator.create_key",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/keys",
        Access::Project(Developer),
    );

    pub const ROTATE_KEY: Endpoint = key_endpoint(
        "orchestrator.rotate_key",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/keys/{key_id}/rotate",
        Access::Project(Developer),
    );

    pub const REVOKE_KEY: Endpoint = key_endpoint(
        "orchestrator.revoke_key",
        Method::Delete,
        "/v1/databases/{id}/branches/{branch_id}/keys/{key_id}",
        Access::Project(Developer),
    );

    /// The branch's schema as the data API sees it, with each table's role privileges.
    pub const DATA_API_SCHEMA: Endpoint = key_endpoint(
        "orchestrator.data_api_schema",
        Method::Get,
        "/v1/databases/{id}/branches/{branch_id}/data-api/schema",
        Access::Project(Viewer),
    );

    /// Starts serving one of the branch's Postgres databases through the data API.
    pub const ENABLE_DATA_API: Endpoint = key_endpoint(
        "orchestrator.enable_data_api",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/pg-databases/{name}/data-api",
        Access::Project(Admin),
    );

    /// Stops serving a Postgres database (`?remove=true` also drops the machinery).
    pub const DISABLE_DATA_API: Endpoint = key_endpoint(
        "orchestrator.disable_data_api",
        Method::Delete,
        "/v1/databases/{id}/branches/{branch_id}/pg-databases/{name}/data-api",
        Access::Project(Admin),
    );

    /// Changes a Postgres database's data API settings (its default schema).
    pub const UPDATE_PG_DATABASE: Endpoint = key_endpoint(
        "orchestrator.update_pg_database",
        Method::Patch,
        "/v1/databases/{id}/branches/{branch_id}/pg-databases/{name}",
        Access::Project(Admin),
    );

    /// Schema merges between a branch and its parent (step 6g). Changing ones need
    /// `Branch(Developer, Admin)` by the target branch (the one merged into).
    ///
    /// For `create_merge`, the path's `{branch_id}` is the source branch: the access check
    /// must read the protection of its parent (the merge target), not of `{branch_id}`.
    pub const CREATE_MERGE: Endpoint = key_endpoint(
        "orchestrator.create_merge",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/merges",
        BRANCH,
    );
    pub const LIST_MERGES: Endpoint = key_endpoint(
        "orchestrator.list_merges",
        Method::Get,
        "/v1/databases/{id}/merges",
        Access::Project(Viewer),
    );
    pub const GET_MERGE: Endpoint = key_endpoint(
        "orchestrator.get_merge",
        Method::Get,
        "/v1/databases/{id}/merges/{merge_id}",
        Access::Project(Viewer),
    );
    pub const UPDATE_MERGE: Endpoint = key_endpoint(
        "orchestrator.update_merge",
        Method::Patch,
        "/v1/databases/{id}/merges/{merge_id}",
        BRANCH,
    );
    pub const REHEARSE_MERGE: Endpoint = key_endpoint(
        "orchestrator.rehearse_merge",
        Method::Post,
        "/v1/databases/{id}/merges/{merge_id}/rehearse",
        BRANCH,
    );
    pub const APPLY_MERGE: Endpoint = key_endpoint(
        "orchestrator.apply_merge",
        Method::Post,
        "/v1/databases/{id}/merges/{merge_id}/apply",
        BRANCH,
    );
    pub const RECOMPUTE_MERGE: Endpoint = key_endpoint(
        "orchestrator.recompute_merge",
        Method::Post,
        "/v1/databases/{id}/merges/{merge_id}/recompute",
        BRANCH,
    );
    pub const RETRY_MERGE: Endpoint = key_endpoint(
        "orchestrator.retry_merge",
        Method::Post,
        "/v1/databases/{id}/merges/{merge_id}/retry",
        BRANCH,
    );
    pub const CANCEL_MERGE: Endpoint = key_endpoint(
        "orchestrator.cancel_merge",
        Method::Post,
        "/v1/databases/{id}/merges/{merge_id}/cancel",
        BRANCH,
    );
    pub const ABANDON_MERGE: Endpoint = key_endpoint(
        "orchestrator.abandon_merge",
        Method::Post,
        "/v1/databases/{id}/merges/{merge_id}/abandon",
        BRANCH,
    );
    pub const FINISH_MERGE: Endpoint = key_endpoint(
        "orchestrator.finish_merge",
        Method::Post,
        "/v1/databases/{id}/merges/{merge_id}/finish",
        BRANCH,
    );
    pub const EXPORT_MERGE_PLAN: Endpoint = key_endpoint(
        "orchestrator.export_merge_plan",
        Method::Get,
        "/v1/databases/{id}/merges/{merge_id}/plan.sql",
        Access::Project(Viewer),
    );

    /// Studio's console: one data API request on the branch, as the owner or a named role.
    pub const CONSOLE_QUERY: Endpoint = key_endpoint(
        "orchestrator.console_query",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/console/query",
        Access::ActAs(Viewer),
    );

    /// Studio's SQL editor: each user's saved queries, per database. Editing and deleting
    /// one also needs its author or Admin (the handler refines).
    pub const LIST_SAVED_QUERIES: Endpoint = key_endpoint(
        "orchestrator.list_saved_queries",
        Method::Get,
        "/v1/databases/{id}/sql-queries",
        Access::Project(Developer),
    );
    pub const CREATE_SAVED_QUERY: Endpoint = key_endpoint(
        "orchestrator.create_saved_query",
        Method::Post,
        "/v1/databases/{id}/sql-queries",
        Access::Project(Developer),
    );
    pub const GET_SAVED_QUERY: Endpoint = key_endpoint(
        "orchestrator.get_saved_query",
        Method::Get,
        "/v1/databases/{id}/sql-queries/{query_id}",
        Access::Project(Developer),
    );
    pub const UPDATE_SAVED_QUERY: Endpoint = key_endpoint(
        "orchestrator.update_saved_query",
        Method::Patch,
        "/v1/databases/{id}/sql-queries/{query_id}",
        Access::Project(Developer),
    );
    pub const DELETE_SAVED_QUERY: Endpoint = key_endpoint(
        "orchestrator.delete_saved_query",
        Method::Delete,
        "/v1/databases/{id}/sql-queries/{query_id}",
        Access::Project(Developer),
    );

    /// Studio's SQL editor: runs a script on a branch (an NDJSON stream), cancels a run (its
    /// starter or Admin; the handler refines), and serves the schema for completion.
    pub const RUN_SQL: Endpoint = key_endpoint(
        "orchestrator.run_sql",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/sql",
        Access::ActAs(Developer),
    );
    pub const CANCEL_SQL_RUN: Endpoint = key_endpoint(
        "orchestrator.cancel_sql_run",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/sql/runs/{run_id}/cancel",
        Access::Project(Developer),
    );
    pub const GET_SQL_SCHEMA: Endpoint = key_endpoint(
        "orchestrator.get_sql_schema",
        Method::Get,
        "/v1/databases/{id}/branches/{branch_id}/sql/schema",
        Access::ActAs(Developer),
    );

    /// Studio's console: the schema it may show, as the owner or a named role.
    pub const CONSOLE_SCHEMA: Endpoint = key_endpoint(
        "orchestrator.console_schema",
        Method::Get,
        "/v1/databases/{id}/branches/{branch_id}/console/schema",
        Access::ActAs(Viewer),
    );

    /// Changes which databases a key may reach.
    pub const UPDATE_KEY: Endpoint = key_endpoint(
        "orchestrator.update_key",
        Method::Patch,
        "/v1/databases/{id}/branches/{branch_id}/keys/{key_id}",
        Access::Project(Developer),
    );

    /// The branch's roles: the owner first, then the login roles.
    pub const LIST_ROLES: Endpoint = key_endpoint(
        "orchestrator.list_roles",
        Method::Get,
        "/v1/databases/{id}/branches/{branch_id}/roles",
        Access::Project(Viewer),
    );

    pub const CREATE_ROLE: Endpoint = key_endpoint(
        "orchestrator.create_role",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/roles",
        Access::Project(Admin),
    );

    pub const RESET_ROLE: Endpoint = key_endpoint(
        "orchestrator.reset_role_password",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/roles/{role}/reset-password",
        Access::Project(Admin),
    );

    pub const DELETE_ROLE: Endpoint = key_endpoint(
        "orchestrator.delete_role",
        Method::Delete,
        "/v1/databases/{id}/branches/{branch_id}/roles/{role}",
        Access::Project(Admin),
    );

    /// Changes a role's LOGIN, owner-level, attributes or memberships (step 6g, U1).
    pub const UPDATE_ROLE: Endpoint = key_endpoint(
        "orchestrator.update_role",
        Method::Patch,
        "/v1/databases/{id}/branches/{branch_id}/roles/{role}",
        Access::Project(Admin),
    );

    /// A role's live sessions on the branch's compute (step 6g, E29a): a read that never
    /// starts the compute.
    pub const LIST_ROLE_SESSIONS: Endpoint = key_endpoint(
        "orchestrator.list_role_sessions",
        Method::Get,
        "/v1/databases/{id}/branches/{branch_id}/roles/{role}/sessions",
        Access::Project(Admin),
    );

    /// The tenant's projects (filtered to the caller's grants; tenant admins see all).
    pub const LIST_PROJECTS: Endpoint =
        key_endpoint("orchestrator.list_projects", Method::Get, "/v1/projects", Access::TenantMember);

    pub const CREATE_PROJECT: Endpoint =
        key_endpoint("orchestrator.create_project", Method::Post, "/v1/projects", Access::TenantAdmin);

    pub const GET_PROJECT: Endpoint =
        key_endpoint("orchestrator.get_project", Method::Get, "/v1/projects/{id}", Access::Project(Viewer));

    /// Renames a project (the slug stays).
    pub const UPDATE_PROJECT: Endpoint =
        key_endpoint("orchestrator.update_project", Method::Patch, "/v1/projects/{id}", Access::Project(Admin));

    /// Starts deleting a project and its databases.
    pub const DELETE_PROJECT: Endpoint =
        key_endpoint("orchestrator.delete_project", Method::Delete, "/v1/projects/{id}", Access::Project(Admin));

    /// The project's app-users settings (a read: Viewers see whether app users are on).
    pub const GET_APP_USERS: Endpoint = key_endpoint(
        "orchestrator.get_app_users",
        Method::Get,
        "/v1/projects/{id}/app-users",
        Access::Project(Viewer),
    );

    /// Enables app users, or changes their settings.
    pub const PUT_APP_USERS: Endpoint = key_endpoint(
        "orchestrator.put_app_users",
        Method::Put,
        "/v1/projects/{id}/app-users",
        Access::Project(Admin),
    );

    /// Disables app users (refused while users exist).
    pub const DISABLE_APP_USERS: Endpoint = key_endpoint(
        "orchestrator.disable_app_users",
        Method::Delete,
        "/v1/projects/{id}/app-users",
        Access::Project(Admin),
    );

    /// The project's users, 50 per page.
    pub const LIST_APP_USERS: Endpoint = key_endpoint(
        "orchestrator.list_app_users",
        Method::Get,
        "/v1/projects/{id}/users",
        Access::Project(Viewer),
    );

    /// Creates a user.
    pub const CREATE_APP_USER: Endpoint = key_endpoint(
        "orchestrator.create_app_user",
        Method::Post,
        "/v1/projects/{id}/users",
        Access::Project(Developer),
    );

    pub const GET_APP_USER: Endpoint = key_endpoint(
        "orchestrator.get_app_user",
        Method::Get,
        "/v1/projects/{id}/users/{user_id}",
        Access::Project(Viewer),
    );

    /// Changes a user's email, verified flag or password.
    pub const UPDATE_APP_USER: Endpoint = key_endpoint(
        "orchestrator.update_app_user",
        Method::Patch,
        "/v1/projects/{id}/users/{user_id}",
        Access::Project(Developer),
    );

    pub const DEACTIVATE_APP_USER: Endpoint = key_endpoint(
        "orchestrator.deactivate_app_user",
        Method::Post,
        "/v1/projects/{id}/users/{user_id}/deactivate",
        Access::Project(Developer),
    );

    pub const REACTIVATE_APP_USER: Endpoint = key_endpoint(
        "orchestrator.reactivate_app_user",
        Method::Post,
        "/v1/projects/{id}/users/{user_id}/reactivate",
        Access::Project(Developer),
    );

    pub const LIST_APP_USER_SESSIONS: Endpoint = key_endpoint(
        "orchestrator.list_app_user_sessions",
        Method::Get,
        "/v1/projects/{id}/users/{user_id}/sessions",
        Access::Project(Viewer),
    );

    pub const REVOKE_APP_USER_SESSIONS: Endpoint = key_endpoint(
        "orchestrator.revoke_app_user_sessions",
        Method::Delete,
        "/v1/projects/{id}/users/{user_id}/sessions",
        Access::Project(Developer),
    );

    pub const DELETE_APP_USER: Endpoint = key_endpoint(
        "orchestrator.delete_app_user",
        Method::Delete,
        "/v1/projects/{id}/users/{user_id}",
        Access::Project(Developer),
    );

    /// The tenant's members (names and emails, for grant pickers).
    pub const LIST_MEMBERS: Endpoint =
        key_endpoint("orchestrator.list_members", Method::Get, "/v1/members", Access::TenantMember);

    /// Sets or clears a member's tenant-admin flag.
    pub const UPDATE_MEMBER: Endpoint = key_endpoint(
        "orchestrator.update_member",
        Method::Patch,
        "/v1/members/{member_id}",
        Access::TenantAdmin,
    );

    /// Removes a member: grants gone, Zitadel user deactivated, sessions ended.
    pub const REMOVE_MEMBER: Endpoint = key_endpoint(
        "orchestrator.remove_member",
        Method::Delete,
        "/v1/members/{member_id}",
        Access::TenantAdmin,
    );

    /// Approves a pending sign-up with its first grants (R40).
    pub const APPROVE_MEMBER: Endpoint = key_endpoint(
        "orchestrator.approve_member",
        Method::Post,
        "/v1/members/{member_id}/approve",
        Access::TenantAdmin,
    );

    /// Declines a pending sign-up: removed, its Zitadel user deactivated (R40).
    pub const DECLINE_MEMBER: Endpoint = key_endpoint(
        "orchestrator.decline_member",
        Method::Post,
        "/v1/members/{member_id}/decline",
        Access::TenantAdmin,
    );

    /// Invites a person by email, with grants on any of the tenant's projects.
    pub const CREATE_INVITE: Endpoint =
        key_endpoint("orchestrator.create_invite", Method::Post, "/v1/invites", Access::TenantAdmin);

    /// Sends an open invite again with a new code.
    pub const RESEND_INVITE: Endpoint = key_endpoint(
        "orchestrator.resend_invite",
        Method::Post,
        "/v1/invites/{invite_id}/resend",
        Access::TenantAdmin,
    );

    /// Revokes an open invite (the never-activated Zitadel user is deleted).
    pub const REVOKE_INVITE: Endpoint = key_endpoint(
        "orchestrator.revoke_invite",
        Method::Delete,
        "/v1/invites/{invite_id}",
        Access::TenantAdmin,
    );

    /// Invites a person straight into this project.
    pub const CREATE_PROJECT_INVITE: Endpoint = key_endpoint(
        "orchestrator.create_project_invite",
        Method::Post,
        "/v1/projects/{id}/invites",
        Access::Project(Admin),
    );

    /// The project's grants, with their per-database overrides.
    pub const LIST_PROJECT_GRANTS: Endpoint = key_endpoint(
        "orchestrator.list_project_grants",
        Method::Get,
        "/v1/projects/{id}/grants",
        Access::Project(Admin),
    );

    pub const PUT_PROJECT_GRANT: Endpoint = key_endpoint(
        "orchestrator.put_project_grant",
        Method::Put,
        "/v1/projects/{id}/grants/{member_id}",
        Access::Project(Admin),
    );

    pub const DELETE_PROJECT_GRANT: Endpoint = key_endpoint(
        "orchestrator.delete_project_grant",
        Method::Delete,
        "/v1/projects/{id}/grants/{member_id}",
        Access::Project(Admin),
    );

    /// Narrows a member's grant on one of the project's databases.
    pub const PUT_GRANT_OVERRIDE: Endpoint = key_endpoint(
        "orchestrator.put_grant_override",
        Method::Put,
        "/v1/projects/{id}/grants/{member_id}/overrides",
        Access::Project(Admin),
    );

    pub const DELETE_GRANT_OVERRIDE: Endpoint = key_endpoint(
        "orchestrator.delete_grant_override",
        Method::Delete,
        "/v1/projects/{id}/grants/{member_id}/overrides",
        Access::Project(Admin),
    );

    /// The tenant's access log, newest first.
    pub const LIST_ACCESS_EVENTS: Endpoint = key_endpoint(
        "orchestrator.list_access_events",
        Method::Get,
        "/v1/access-events",
        Access::TenantAdmin,
    );

    /// One project's access log, newest first.
    pub const LIST_PROJECT_ACCESS_EVENTS: Endpoint = key_endpoint(
        "orchestrator.list_project_access_events",
        Method::Get,
        "/v1/projects/{id}/access-events",
        Access::Project(Admin),
    );

    /// The instance's settings (open sign-up).
    pub const GET_INSTANCE_SETTINGS: Endpoint = key_endpoint(
        "orchestrator.get_instance_settings",
        Method::Get,
        "/v1/instance/settings",
        Access::Instance,
    );

    pub const UPDATE_INSTANCE_SETTINGS: Endpoint = key_endpoint(
        "orchestrator.update_instance_settings",
        Method::Patch,
        "/v1/instance/settings",
        Access::Instance,
    );

    /// Whether Zitadel can email invites (an active email provider).
    pub const GET_INVITE_DELIVERY: Endpoint = key_endpoint(
        "orchestrator.get_invite_delivery",
        Method::Get,
        "/v1/invites/delivery",
        Access::TenantMember,
    );

    /// Renames the tenant (its display name).
    pub const UPDATE_TENANT: Endpoint =
        key_endpoint("orchestrator.update_tenant", Method::Patch, "/v1/tenant", Access::TenantAdmin);

    /// The database's tables, views and other relations (kind, size, whether the table API edits
    /// them).
    pub const LIST_OBJECTS: Endpoint = key_endpoint(
        "orchestrator.list_objects",
        Method::Get,
        "/v1/databases/{id}/branches/{branch_id}/objects",
        Access::Project(Viewer),
    );

    /// One ordinary table's structure.
    pub const GET_TABLE: Endpoint = key_endpoint(
        "orchestrator.get_table",
        Method::Get,
        "/v1/databases/{id}/branches/{branch_id}/tables/{schema}/{table}",
        Access::Project(Viewer),
    );

    /// The database's types (for a column type picker).
    pub const LIST_TYPES: Endpoint = key_endpoint(
        "orchestrator.list_types",
        Method::Get,
        "/v1/databases/{id}/branches/{branch_id}/types",
        Access::Project(Viewer),
    );

    /// DDL and enum changes through the table API (dry run or real).
    pub const APPLY_SCHEMA_CHANGES: Endpoint = key_endpoint(
        "orchestrator.apply_schema_changes",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/schema-changes",
        Access::ActAsBranch { unprotected: Developer, protected: Admin },
    );

    /// A row of all defaults (`INSERT … DEFAULT VALUES`) in one table.
    pub const INSERT_DEFAULT_ROW: Endpoint = key_endpoint(
        "orchestrator.insert_default_row",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/tables/{schema}/{table}/rows/default",
        Access::ActAs(Developer),
    );

    /// One cell's full value, found by the table's primary key.
    pub const GET_CELL: Endpoint = key_endpoint(
        "orchestrator.get_cell",
        Method::Post,
        "/v1/databases/{id}/branches/{branch_id}/tables/{schema}/{table}/cell",
        Access::ActAs(Viewer),
    );

    /// A management route callable from Studio.
    const fn key_endpoint(name: &'static str, method: Method, path: &'static str, access: Access) -> Endpoint {
        Endpoint {
            name,
            method,
            path,
            scope: Scope::Api,
            service: Service::Orchestrator,
            policy: Policy {
                auth: Auth::Management,
                cors: Cors::Studio,
                access,
            },
        }
    }
}

/// The data API, served by `rest`.
pub mod rest {
    use super::{Access, Auth, Cors, Endpoint, Method, Policy, Scope, Service};

    pub const QUERY: Endpoint = Endpoint {
        name: "rest.query",
        method: Method::Post,
        path: "/data/v1/query",
        scope: Scope::Api,
        service: Service::Rest,
        policy: Policy {
            auth: Auth::DataKey,
            cors: Cors::Any,
            access: Access::DataKey,
        },
    };

    pub const SCHEMA: Endpoint = Endpoint {
        name: "rest.schema",
        method: Method::Get,
        path: "/data/v1/schema",
        scope: Scope::Api,
        service: Service::Rest,
        policy: Policy {
            auth: Auth::SecretDataKey,
            cors: Cors::Any,
            access: Access::DataKey,
        },
    };
}

/// Studio's sign-in pages, served by `auth`.
pub mod auth {
    use super::{Access, Auth, Cors, Endpoint, Method, Policy, Scope, Service};

    const POLICY: Policy = Policy {
        auth: Auth::Public,
        cors: Cors::StudioCredentials,
        access: Access::Public,
    };

    const fn post(name: &'static str, path: &'static str) -> Endpoint {
        Endpoint {
            name,
            method: Method::Post,
            path,
            scope: Scope::Api,
            service: Service::Auth,
            policy: POLICY,
        }
    }

    const fn get(name: &'static str, path: &'static str) -> Endpoint {
        Endpoint {
            method: Method::Get,
            ..post(name, path)
        }
    }

    pub const AUTH_REQUEST: Endpoint = post("auth.auth_request", "/auth/v1/auth-request");
    pub const SIGN_IN: Endpoint = post("auth.sign_in", "/auth/v1/sign-in");
    pub const SIGN_UP: Endpoint = post("auth.sign_up", "/auth/v1/sign-up");
    pub const ACCEPT_INVITE: Endpoint = post("auth.accept_invite", "/auth/v1/accept-invite");
    /// Who invited the visitor and to what, for the accept page (`{user, t}` in the body, never a URL). Public: the link's
    /// token is the credential.
    pub const DESCRIBE_INVITE: Endpoint = post("auth.describe_invite", "/auth/v1/invite");
    pub const VERIFY_EMAIL: Endpoint = post("auth.verify_email", "/auth/v1/verify-email");
    pub const RESEND_CODE: Endpoint = post("auth.resend_code", "/auth/v1/resend-code");
    pub const PASSWORD_RESET: Endpoint = post("auth.password_reset", "/auth/v1/password-reset");
    pub const PASSWORD_RESET_CONFIRM: Endpoint =
        post("auth.password_reset_confirm", "/auth/v1/password-reset/confirm");
    pub const LOGOUT: Endpoint = post("auth.logout", "/auth/v1/logout");

    /// App users' flows: a public data key, from any origin (apps call them from the browser).
    const APP_POLICY: Policy = Policy {
        auth: Auth::PublicDataKey,
        cors: Cors::Any,
        access: Access::DataKey,
    };

    const fn app(name: &'static str, method: Method, path: &'static str) -> Endpoint {
        Endpoint {
            name,
            method,
            path,
            scope: Scope::Api,
            service: Service::Auth,
            policy: APP_POLICY,
        }
    }

    pub const APP_SIGN_UP: Endpoint = app("auth.app_sign_up", Method::Post, "/auth/v1/app/sign-up");
    pub const APP_SIGN_IN: Endpoint = app("auth.app_sign_in", Method::Post, "/auth/v1/app/sign-in");
    pub const APP_REFRESH: Endpoint = app("auth.app_refresh", Method::Post, "/auth/v1/app/refresh");
    pub const APP_SIGN_OUT: Endpoint =
        app("auth.app_sign_out", Method::Post, "/auth/v1/app/sign-out");
    pub const APP_USER: Endpoint = app("auth.app_user", Method::Get, "/auth/v1/app/user");
    pub const APP_VERIFY_EMAIL: Endpoint =
        app("auth.app_verify_email", Method::Post, "/auth/v1/app/verify-email");
    pub const APP_RESEND_CODE: Endpoint =
        app("auth.app_resend_code", Method::Post, "/auth/v1/app/resend-code");
    pub const APP_PASSWORD_RESET: Endpoint =
        app("auth.app_password_reset", Method::Post, "/auth/v1/app/password-reset");
    pub const APP_PASSWORD_RESET_CONFIRM: Endpoint = app(
        "auth.app_password_reset_confirm",
        Method::Post,
        "/auth/v1/app/password-reset/confirm",
    );
    pub const APP_CHANGE_EMAIL: Endpoint =
        app("auth.app_change_email", Method::Post, "/auth/v1/app/change-email");
    pub const APP_CHANGE_PASSWORD: Endpoint =
        app("auth.app_change_password", Method::Post, "/auth/v1/app/change-password");
    /// The app token signing key, for apps' own servers: no key needed.
    pub const APP_JWKS: Endpoint = Endpoint {
        name: "auth.app_jwks",
        method: Method::Get,
        path: "/auth/v1/app/jwks",
        scope: Scope::Api,
        service: Service::Auth,
        policy: Policy {
            auth: Auth::Public,
            cors: Cors::Any,
            access: Access::Public,
        },
    };
}

/// Every public endpoint the gateway exposes.
pub const ENDPOINTS: &[Endpoint] = &[
    orchestrator::HEALTH,
    orchestrator::OPENAPI_JSON,
    orchestrator::CREATE_DATABASE,
    orchestrator::GET_DATABASE,
    orchestrator::ME,
    orchestrator::DATABASE_CONNECTION,
    orchestrator::RESET_PASSWORD,
    orchestrator::DELETE_DATABASE,
    orchestrator::UPDATE_DATABASE,
    orchestrator::LIST_BRANCHES,
    orchestrator::CREATE_BRANCH,
    orchestrator::BRANCH_CONNECTION,
    orchestrator::BRANCH_RESET_PASSWORD,
    orchestrator::BRANCH_RESET_TO_PARENT,
    orchestrator::BRANCH_RESTORE,
    orchestrator::DELETE_BRANCH,
    orchestrator::UPDATE_BRANCH,
    orchestrator::LIST_GUARD_EXEMPTIONS,
    orchestrator::PUT_GUARD_EXEMPTION,
    orchestrator::DELETE_GUARD_EXEMPTION,
    orchestrator::GET_BRANCH_REVIEW,
    orchestrator::ACKNOWLEDGE_BRANCH_REVIEW,
    orchestrator::FIX_BRANCH_REVIEW_ITEM,
    orchestrator::LIST_BRANCH_DATABASES,
    orchestrator::LIST_KEYS,
    orchestrator::CREATE_KEY,
    orchestrator::ROTATE_KEY,
    orchestrator::REVOKE_KEY,
    orchestrator::DATA_API_SCHEMA,
    orchestrator::ENABLE_DATA_API,
    orchestrator::DISABLE_DATA_API,
    orchestrator::UPDATE_PG_DATABASE,
    orchestrator::UPDATE_KEY,
    orchestrator::CONSOLE_QUERY,
    orchestrator::CONSOLE_SCHEMA,
    orchestrator::CREATE_MERGE,
    orchestrator::LIST_MERGES,
    orchestrator::GET_MERGE,
    orchestrator::UPDATE_MERGE,
    orchestrator::REHEARSE_MERGE,
    orchestrator::APPLY_MERGE,
    orchestrator::RECOMPUTE_MERGE,
    orchestrator::RETRY_MERGE,
    orchestrator::CANCEL_MERGE,
    orchestrator::ABANDON_MERGE,
    orchestrator::FINISH_MERGE,
    orchestrator::EXPORT_MERGE_PLAN,
    orchestrator::LIST_SAVED_QUERIES,
    orchestrator::CREATE_SAVED_QUERY,
    orchestrator::GET_SAVED_QUERY,
    orchestrator::UPDATE_SAVED_QUERY,
    orchestrator::DELETE_SAVED_QUERY,
    orchestrator::RUN_SQL,
    orchestrator::CANCEL_SQL_RUN,
    orchestrator::GET_SQL_SCHEMA,
    orchestrator::LIST_ROLES,
    orchestrator::CREATE_ROLE,
    orchestrator::RESET_ROLE,
    orchestrator::DELETE_ROLE,
    orchestrator::UPDATE_ROLE,
    orchestrator::LIST_ROLE_SESSIONS,
    orchestrator::LIST_PROJECTS,
    orchestrator::CREATE_PROJECT,
    orchestrator::GET_PROJECT,
    orchestrator::UPDATE_PROJECT,
    orchestrator::DELETE_PROJECT,
    orchestrator::GET_APP_USERS,
    orchestrator::PUT_APP_USERS,
    orchestrator::DISABLE_APP_USERS,
    orchestrator::LIST_APP_USERS,
    orchestrator::CREATE_APP_USER,
    orchestrator::GET_APP_USER,
    orchestrator::UPDATE_APP_USER,
    orchestrator::DEACTIVATE_APP_USER,
    orchestrator::REACTIVATE_APP_USER,
    orchestrator::LIST_APP_USER_SESSIONS,
    orchestrator::REVOKE_APP_USER_SESSIONS,
    orchestrator::DELETE_APP_USER,
    orchestrator::LIST_MEMBERS,
    orchestrator::UPDATE_MEMBER,
    orchestrator::REMOVE_MEMBER,
    orchestrator::APPROVE_MEMBER,
    orchestrator::DECLINE_MEMBER,
    orchestrator::CREATE_INVITE,
    orchestrator::RESEND_INVITE,
    orchestrator::REVOKE_INVITE,
    orchestrator::CREATE_PROJECT_INVITE,
    orchestrator::LIST_PROJECT_GRANTS,
    orchestrator::PUT_PROJECT_GRANT,
    orchestrator::DELETE_PROJECT_GRANT,
    orchestrator::PUT_GRANT_OVERRIDE,
    orchestrator::DELETE_GRANT_OVERRIDE,
    orchestrator::LIST_ACCESS_EVENTS,
    orchestrator::LIST_PROJECT_ACCESS_EVENTS,
    orchestrator::GET_INSTANCE_SETTINGS,
    orchestrator::UPDATE_INSTANCE_SETTINGS,
    orchestrator::GET_INVITE_DELIVERY,
    orchestrator::UPDATE_TENANT,
    orchestrator::LIST_OBJECTS,
    orchestrator::GET_TABLE,
    orchestrator::LIST_TYPES,
    orchestrator::APPLY_SCHEMA_CHANGES,
    orchestrator::INSERT_DEFAULT_ROW,
    orchestrator::GET_CELL,
    rest::QUERY,
    rest::SCHEMA,
    auth::AUTH_REQUEST,
    auth::SIGN_IN,
    auth::SIGN_UP,
    auth::ACCEPT_INVITE,
    auth::DESCRIBE_INVITE,
    auth::VERIFY_EMAIL,
    auth::RESEND_CODE,
    auth::PASSWORD_RESET,
    auth::PASSWORD_RESET_CONFIRM,
    auth::LOGOUT,
    auth::APP_SIGN_UP,
    auth::APP_SIGN_IN,
    auth::APP_REFRESH,
    auth::APP_SIGN_OUT,
    auth::APP_USER,
    auth::APP_VERIFY_EMAIL,
    auth::APP_RESEND_CODE,
    auth::APP_PASSWORD_RESET,
    auth::APP_PASSWORD_RESET_CONFIRM,
    auth::APP_CHANGE_EMAIL,
    auth::APP_CHANGE_PASSWORD,
    auth::APP_JWKS,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    Found(&'static Endpoint),
    /// The path exists in this scope, but not for the requested method.
    MethodNotAllowed(Vec<Method>),
    NotFound,
}

/// Find the endpoint for `method` + `path` (no query string) within `scope`.
pub fn match_endpoint(scope: Scope, method: &str, path: &str) -> Match {
    match_in(ENDPOINTS, scope, method, path)
}

fn match_in(endpoints: &'static [Endpoint], scope: Scope, method: &str, path: &str) -> Match {
    let method = Method::parse(method);
    let mut best: Option<(&'static Endpoint, Vec<bool>)> = None;
    let mut allowed = Vec::new();

    for ep in endpoints.iter().filter(|ep| ep.scope == scope) {
        let Some(specificity) = path_specificity(ep.path, path) else {
            continue;
        };
        if Some(ep.method) == method {
            let better = best
                .as_ref()
                .is_none_or(|(_, current)| specificity > *current);
            if better {
                best = Some((ep, specificity));
            }
        } else if !allowed.contains(&ep.method) {
            allowed.push(ep.method);
        }
    }

    match best {
        Some((ep, _)) => Match::Found(ep),
        None if !allowed.is_empty() => Match::MethodNotAllowed(allowed),
        None => Match::NotFound,
    }
}

/// If `template` matches `path`, return one flag per segment (`true` = literal
/// segment), so literal matches compare greater than param matches.
fn path_specificity(template: &str, path: &str) -> Option<Vec<bool>> {
    let template = template.strip_prefix('/')?;
    let path = path.strip_prefix('/')?;
    let template_segments: Vec<&str> = template.split('/').collect();
    let path_segments: Vec<&str> = path.split('/').collect();
    if template_segments.len() != path_segments.len() {
        return None;
    }
    template_segments
        .iter()
        .zip(&path_segments)
        .map(|(t, p)| {
            if t.starts_with('{') && t.ends_with('}') {
                (!p.is_empty()).then_some(false)
            } else {
                (t == p).then_some(true)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_docs_paths_are_unrouted() {
        for path in ["/v1/docs", "/v1/docs/scalar.js"] {
            assert_eq!(match_endpoint(Scope::Api, "GET", path), Match::NotFound, "{path}");
        }
    }

    #[test]
    fn docs_routes_are_public() {
        for (path, name) in [("/v1/openapi.json", "orchestrator.openapi_json")] {
            match match_endpoint(Scope::Api, "GET", path) {
                Match::Found(ep) => {
                    assert_eq!(ep.name, name);
                    assert_eq!(ep.policy.auth, Auth::Public, "{path}");
                    assert_eq!(ep.service, Service::Orchestrator);
                }
                other => panic!("{path}: expected Found, got {other:?}"),
            }
        }
    }

    #[test]
    fn literal_path_is_found() {
        assert_eq!(
            match_endpoint(Scope::Api, "POST", "/v1/databases"),
            Match::Found(&orchestrator::CREATE_DATABASE)
        );
    }

    #[test]
    fn param_segment_matches_any_non_empty_value() {
        assert_eq!(
            match_endpoint(
                Scope::Api,
                "GET",
                "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11"
            ),
            Match::Found(&orchestrator::GET_DATABASE)
        );
    }

    #[test]
    fn known_path_with_wrong_method_lists_allowed_methods() {
        assert_eq!(
            match_endpoint(Scope::Api, "DELETE", "/v1/databases"),
            Match::MethodNotAllowed(vec![Method::Post])
        );
    }

    #[test]
    fn undeclarable_method_token_on_known_path_is_method_not_allowed() {
        assert_eq!(
            match_endpoint(Scope::Api, "OPTIONS", "/v1/databases"),
            Match::MethodNotAllowed(vec![Method::Post])
        );
    }

    #[test]
    fn unknown_path_is_not_found() {
        assert_eq!(
            match_endpoint(Scope::Api, "GET", "/v1/other"),
            Match::NotFound
        );
    }

    #[test]
    fn trailing_slash_is_not_found() {
        assert_eq!(
            match_endpoint(Scope::Api, "POST", "/v1/databases/"),
            Match::NotFound
        );
    }

    #[test]
    fn empty_param_segment_is_not_found() {
        assert_eq!(
            match_endpoint(Scope::Api, "GET", "/v1/databases//"),
            Match::NotFound
        );
    }

    #[test]
    fn prefix_of_a_path_is_not_found() {
        assert_eq!(match_endpoint(Scope::Api, "GET", "/v1"), Match::NotFound);
    }

    const LITERAL_VS_PARAM: &[Endpoint] = &[
        Endpoint {
            name: "test.by_id",
            method: Method::Get,
            path: "/things/{id}",
            ..orchestrator::HEALTH
        },
        Endpoint {
            name: "test.search",
            method: Method::Get,
            path: "/things/search",
            ..orchestrator::HEALTH
        },
    ];

    #[test]
    fn literal_segment_beats_param_segment() {
        match match_in(LITERAL_VS_PARAM, Scope::Api, "GET", "/things/search") {
            Match::Found(ep) => assert_eq!(ep.name, "test.search"),
            other => panic!("expected Found, got {other:?}"),
        }
        match match_in(LITERAL_VS_PARAM, Scope::Api, "GET", "/things/42") {
            Match::Found(ep) => assert_eq!(ep.name, "test.by_id"),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn declared_endpoints_are_unique_and_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for ep in ENDPOINTS {
            assert!(
                seen.insert((ep.scope, ep.method, ep.path)),
                "duplicate endpoint {} {}",
                ep.method.as_str(),
                ep.path
            );
            assert!(ep.path.starts_with('/'), "{} must start with /", ep.path);
            assert!(
                ep.path == "/" || !ep.path.ends_with('/'),
                "{} must not end with /",
                ep.path
            );
            for segment in ep.path.split('/').skip(1) {
                assert!(!segment.is_empty(), "{} has an empty segment", ep.path);
                let opens = segment.starts_with('{');
                let closes = segment.ends_with('}');
                assert_eq!(opens, closes, "{} has a malformed param segment", ep.path);
            }
        }
    }

    const BRANCHES: &str = "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11/branches";
    const BRANCH: &str =
        "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11/branches/0d8f2f1d-7c11-4c52-9a51-6c0f1c2e2f7e";

    fn found(method: &str, path: &str) -> &'static str {
        match match_endpoint(Scope::Api, method, path) {
            Match::Found(ep) => ep.name,
            other => panic!("{method} {path}: expected Found, got {other:?}"),
        }
    }

    #[test]
    fn merge_endpoints_route_and_are_management_auth_with_studio_cors() {
        let db = "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11";
        let merge = format!("{db}/merges/3b1f6a2e-0d8f-4c52-9a51-6c0f1c2e2f7e");
        assert_eq!(found("POST", &format!("{BRANCH}/merges")), "orchestrator.create_merge");
        assert_eq!(found("GET", &format!("{db}/merges")), "orchestrator.list_merges");
        assert_eq!(found("GET", &merge), "orchestrator.get_merge");
        assert_eq!(found("PATCH", &merge), "orchestrator.update_merge");
        assert_eq!(found("POST", &format!("{merge}/rehearse")), "orchestrator.rehearse_merge");
        assert_eq!(found("POST", &format!("{merge}/apply")), "orchestrator.apply_merge");
        assert_eq!(found("POST", &format!("{merge}/recompute")), "orchestrator.recompute_merge");
        assert_eq!(found("POST", &format!("{merge}/retry")), "orchestrator.retry_merge");
        assert_eq!(found("POST", &format!("{merge}/cancel")), "orchestrator.cancel_merge");
        assert_eq!(found("POST", &format!("{merge}/finish")), "orchestrator.finish_merge");
        assert_eq!(found("GET", &format!("{merge}/plan.sql")), "orchestrator.export_merge_plan");
        for name in [
            "finish_merge",
            "create_merge",
            "list_merges",
            "get_merge",
            "update_merge",
            "rehearse_merge",
            "apply_merge",
            "recompute_merge",
            "retry_merge",
            "cancel_merge",
            "export_merge_plan",
        ] {
            let ep = ENDPOINTS
                .iter()
                .find(|e| e.name == format!("orchestrator.{name}"))
                .unwrap_or_else(|| panic!("{name} is not in ENDPOINTS"));
            assert_eq!((ep.policy.auth, ep.policy.cors), (Auth::Management, Cors::Studio), "{name}");
            assert_eq!(ep.service, Service::Orchestrator, "{name}");
        }
    }

    #[test]
    fn branch_endpoints_route_with_two_path_params() {
        assert_eq!(found("GET", BRANCHES), "orchestrator.list_branches");
        assert_eq!(found("POST", BRANCHES), "orchestrator.create_branch");
        assert_eq!(found("DELETE", BRANCH), "orchestrator.delete_branch");
        assert_eq!(found("PATCH", BRANCH), "orchestrator.update_branch");
        assert_eq!(
            found("GET", &format!("{BRANCH}/databases")),
            "orchestrator.list_branch_databases"
        );
        for (method, suffix, name) in [
            ("GET", "connection", "orchestrator.branch_connection"),
            ("POST", "reset-password", "orchestrator.branch_reset_password"),
            ("POST", "reset-to-parent", "orchestrator.branch_reset_to_parent"),
            ("POST", "restore", "orchestrator.branch_restore"),
        ] {
            assert_eq!(found(method, &format!("{BRANCH}/{suffix}")), name);
        }
    }

    #[test]
    fn role_endpoints_route_and_are_management_auth_with_studio_cors() {
        let roles = format!("{BRANCH}/roles");
        assert_eq!(found("GET", &roles), "orchestrator.list_roles");
        assert_eq!(found("POST", &roles), "orchestrator.create_role");
        assert_eq!(found("DELETE", &format!("{roles}/app")), "orchestrator.delete_role");
        assert_eq!(found("PATCH", &format!("{roles}/app")), "orchestrator.update_role");
        assert_eq!(
            found("POST", &format!("{roles}/app/reset-password")),
            "orchestrator.reset_role_password"
        );
        assert_eq!(
            found("GET", &format!("{roles}/app/sessions")),
            "orchestrator.list_role_sessions"
        );
        for name in [
            "list_roles",
            "create_role",
            "reset_role_password",
            "delete_role",
            "update_role",
            "list_role_sessions",
        ] {
            let ep = ENDPOINTS
                .iter()
                .find(|e| e.name == format!("orchestrator.{name}"))
                .unwrap_or_else(|| panic!("{name} is not in ENDPOINTS"));
            assert_eq!((ep.policy.auth, ep.policy.cors), (Auth::Management, Cors::Studio), "{name}");
            assert_eq!(ep.service, Service::Orchestrator, "{name}");
        }
    }

    #[test]
    fn patch_database_routes_and_is_distinct_from_get_and_delete() {
        let db = "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11";
        assert_eq!(found("PATCH", db), "orchestrator.update_database");
        assert_eq!(found("GET", db), "orchestrator.get_database");
        assert_eq!(found("DELETE", db), "orchestrator.delete_database");
        assert_eq!(
            match_endpoint(Scope::Api, "PUT", db),
            Match::MethodNotAllowed(vec![Method::Get, Method::Delete, Method::Patch])
        );
    }

    #[test]
    fn branch_paths_reject_wrong_methods_and_extra_segments() {
        assert_eq!(
            match_endpoint(Scope::Api, "PUT", BRANCH),
            Match::MethodNotAllowed(vec![Method::Delete, Method::Patch])
        );
        assert_eq!(
            match_endpoint(Scope::Api, "GET", &format!("{BRANCH}/restore")),
            Match::MethodNotAllowed(vec![Method::Post])
        );
        assert_eq!(
            match_endpoint(Scope::Api, "GET", &format!("{BRANCH}/nope")),
            Match::NotFound
        );
    }

    #[test]
    fn branch_endpoints_are_management_auth_with_studio_cors() {
        for name in [
            "list_branches",
            "create_branch",
            "branch_connection",
            "branch_reset_password",
            "branch_reset_to_parent",
            "branch_restore",
            "delete_branch",
            "update_branch",
            "list_branch_databases",
            "update_database",
        ] {
            let ep = ENDPOINTS
                .iter()
                .find(|e| e.name == format!("orchestrator.{name}"))
                .unwrap_or_else(|| panic!("{name} is not in ENDPOINTS"));
            assert_eq!((ep.policy.auth, ep.policy.cors), (Auth::Management, Cors::Studio), "{name}");
            assert_eq!(ep.service, Service::Orchestrator, "{name}");
        }
    }

    #[test]
    fn auth_endpoints_are_public_posts_with_credentialed_cors() {
        let platform = |e: &&Endpoint| e.service == Service::Auth && !e.path.starts_with("/auth/v1/app/");
        for ep in ENDPOINTS.iter().filter(platform) {
            // All posts, the accept page's invite description included (M8: its user id and
            // token go in the body, never a URL).
            assert_eq!(ep.method, Method::Post, "{}", ep.path);
            assert_eq!(ep.policy.auth, Auth::Public, "{}", ep.path);
            assert_eq!(ep.policy.cors, Cors::StudioCredentials, "{}", ep.path);
            assert!(ep.path.starts_with("/auth/v1/"), "{}", ep.path);
        }
        assert_eq!(ENDPOINTS.iter().filter(platform).count(), 10);
        assert!(matches!(match_endpoint(Scope::Api, "POST", "/auth/v1/password-reset/confirm"),
            Match::Found(ep) if ep.name == "auth.password_reset_confirm"));
    }

    #[test]
    fn app_auth_endpoints_take_a_public_key_from_any_origin() {
        let want = [
            ("auth.app_sign_up", "POST", "/auth/v1/app/sign-up"),
            ("auth.app_sign_in", "POST", "/auth/v1/app/sign-in"),
            ("auth.app_refresh", "POST", "/auth/v1/app/refresh"),
            ("auth.app_sign_out", "POST", "/auth/v1/app/sign-out"),
            ("auth.app_user", "GET", "/auth/v1/app/user"),
            ("auth.app_verify_email", "POST", "/auth/v1/app/verify-email"),
            ("auth.app_resend_code", "POST", "/auth/v1/app/resend-code"),
            ("auth.app_password_reset", "POST", "/auth/v1/app/password-reset"),
            ("auth.app_password_reset_confirm", "POST", "/auth/v1/app/password-reset/confirm"),
            ("auth.app_change_email", "POST", "/auth/v1/app/change-email"),
            ("auth.app_change_password", "POST", "/auth/v1/app/change-password"),
            ("auth.app_jwks", "GET", "/auth/v1/app/jwks"),
        ];
        let app: Vec<&Endpoint> = ENDPOINTS
            .iter()
            .filter(|e| e.path.starts_with("/auth/v1/app/"))
            .collect();
        assert_eq!(app.len(), want.len());
        for (name, method, path) in want {
            assert_eq!(found(method, path), name, "{method} {path}");
            let ep = app.iter().find(|e| e.name == name).unwrap();
            assert_eq!(ep.service, Service::Auth, "{name}");
            assert_eq!(ep.scope, Scope::Api, "{name}");
            let auth = if name == "auth.app_jwks" { Auth::Public } else { Auth::PublicDataKey };
            assert_eq!((ep.policy.auth, ep.policy.cors), (auth, Cors::Any), "{name}");
        }
    }

    const KEYS: &str = "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11/branches/0d8f2f1d-7c11-4c52-9a51-6c0f1c2e2f7e/keys";

    #[test]
    fn rest_endpoints_match_and_are_listed() {
        assert_eq!(found("POST", "/data/v1/query"), "rest.query");
        assert_eq!(found("GET", "/data/v1/schema"), "rest.schema");
        assert_eq!(
            rest::QUERY.policy,
            Policy { auth: Auth::DataKey, cors: Cors::Any, access: Access::DataKey }
        );
        assert_eq!(
            rest::SCHEMA.policy,
            Policy { auth: Auth::SecretDataKey, cors: Cors::Any, access: Access::DataKey }
        );
        for ep in [rest::QUERY, rest::SCHEMA] {
            assert_eq!(ep.service, Service::Rest);
            assert_eq!(ep.scope, Scope::Api);
            assert!(ENDPOINTS.contains(&ep));
        }
        assert_eq!(found("GET", KEYS), "orchestrator.list_keys");
        assert_eq!(found("POST", KEYS), "orchestrator.create_key");
        assert_eq!(found("DELETE", &format!("{KEYS}/abc")), "orchestrator.revoke_key");
        assert_eq!(found("POST", &format!("{KEYS}/abc/rotate")), "orchestrator.rotate_key");
        assert_eq!(
            found("GET", "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11/branches/0d8f2f1d-7c11-4c52-9a51-6c0f1c2e2f7e/data-api/schema"),
            "orchestrator.data_api_schema"
        );
        for name in ["list_keys", "create_key", "rotate_key", "revoke_key", "data_api_schema"] {
            let ep = ENDPOINTS
                .iter()
                .find(|e| e.name == format!("orchestrator.{name}"))
                .unwrap_or_else(|| panic!("{name} is not in ENDPOINTS"));
            assert_eq!((ep.policy.auth, ep.policy.cors), (Auth::Management, Cors::Studio), "{name}");
        }
    }

    #[test]
    fn per_database_data_api_routes_are_registered() {
        let branch = "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11/branches/0d8f2f1d-7c11-4c52-9a51-6c0f1c2e2f7e";
        assert_eq!(found("POST", &format!("{branch}/pg-databases/analytics/data-api")), "orchestrator.enable_data_api");
        assert_eq!(found("DELETE", &format!("{branch}/pg-databases/analytics/data-api")), "orchestrator.disable_data_api");
        assert_eq!(found("PATCH", &format!("{branch}/pg-databases/analytics")), "orchestrator.update_pg_database");
        assert_eq!(found("PATCH", &format!("{KEYS}/abc")), "orchestrator.update_key");
        for name in ["enable_data_api", "disable_data_api", "update_pg_database", "update_key"] {
            let ep = ENDPOINTS
                .iter()
                .find(|e| e.name == format!("orchestrator.{name}"))
                .unwrap_or_else(|| panic!("{name} is not in ENDPOINTS"));
            assert_eq!((ep.policy.auth, ep.policy.cors), (Auth::Management, Cors::Studio), "{name}");
            assert_eq!(ep.service, Service::Orchestrator, "{name}");
        }
    }

    #[test]
    fn console_schema_route_is_management_auth_with_studio_cors() {
        let branch = "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11/branches/0d8f2f1d-7c11-4c52-9a51-6c0f1c2e2f7e";
        assert_eq!(found("GET", &format!("{branch}/console/schema")), "orchestrator.console_schema");
        let ep = ENDPOINTS
            .iter()
            .find(|e| e.name == "orchestrator.console_schema")
            .expect("console_schema is in ENDPOINTS");
        assert_eq!((ep.policy.auth, ep.policy.cors), (Auth::Management, Cors::Studio));
        assert_eq!(ep.service, Service::Orchestrator);
    }

    #[test]
    fn saved_query_routes_are_management_auth_with_studio_cors() {
        let db = "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11/sql-queries";
        let one = format!("{db}/0d8f2f1d-7c11-4c52-9a51-6c0f1c2e2f7e");
        for (method, path, name) in [
            ("GET", db.to_string(), "orchestrator.list_saved_queries"),
            ("POST", db.to_string(), "orchestrator.create_saved_query"),
            ("GET", one.clone(), "orchestrator.get_saved_query"),
            ("PATCH", one.clone(), "orchestrator.update_saved_query"),
            ("DELETE", one.clone(), "orchestrator.delete_saved_query"),
        ] {
            assert_eq!(found(method, &path), name);
            let ep = ENDPOINTS.iter().find(|e| e.name == name).expect(name);
            assert_eq!((ep.policy.auth, ep.policy.cors), (Auth::Management, Cors::Studio), "{name}");
            assert_eq!(ep.service, Service::Orchestrator);
        }
        assert_eq!(
            match_endpoint(Scope::Api, "PUT", &one),
            Match::MethodNotAllowed(vec![Method::Get, Method::Patch, Method::Delete])
        );
    }

    #[test]
    fn sql_editor_routes_are_management_auth_with_studio_cors() {
        let branch = "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11/branches/0d8f2f1d-7c11-4c52-9a51-6c0f1c2e2f7e";
        let cancel = format!("{branch}/sql/runs/9a510d8f-2f1d-4c52-9a51-6c0f1c2e2f7e/cancel");
        for (method, path, name) in [
            ("POST", format!("{branch}/sql"), "orchestrator.run_sql"),
            ("POST", cancel.clone(), "orchestrator.cancel_sql_run"),
            ("GET", format!("{branch}/sql/schema"), "orchestrator.get_sql_schema"),
        ] {
            assert_eq!(found(method, &path), name);
            let ep = ENDPOINTS.iter().find(|e| e.name == name).expect(name);
            assert_eq!((ep.policy.auth, ep.policy.cors), (Auth::Management, Cors::Studio), "{name}");
            assert_eq!(ep.service, Service::Orchestrator);
        }
        assert_eq!(
            match_endpoint(Scope::Api, "GET", &format!("{branch}/sql")),
            Match::MethodNotAllowed(vec![Method::Post])
        );
        assert_eq!(
            match_endpoint(Scope::Api, "GET", &cancel),
            Match::MethodNotAllowed(vec![Method::Post])
        );
    }

    #[test]
    fn console_query_route_is_management_auth_with_studio_cors() {
        let branch = "/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11/branches/0d8f2f1d-7c11-4c52-9a51-6c0f1c2e2f7e";
        assert_eq!(found("POST", &format!("{branch}/console/query")), "orchestrator.console_query");
        assert_eq!(
            match_endpoint(Scope::Api, "GET", &format!("{branch}/console/query")),
            Match::MethodNotAllowed(vec![Method::Post])
        );
        let ep = ENDPOINTS
            .iter()
            .find(|e| e.name == "orchestrator.console_query")
            .expect("console_query is in ENDPOINTS");
        assert_eq!((ep.policy.auth, ep.policy.cors), (Auth::Management, Cors::Studio));
        assert_eq!(ep.service, Service::Orchestrator);
    }

    #[test]
    fn literal_keys_rotate_beats_param() {
        let id = "6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11";
        assert_eq!(found("POST", &format!("{KEYS}/{id}/rotate")), "orchestrator.rotate_key");
        assert_eq!(found("DELETE", &format!("{KEYS}/{id}")), "orchestrator.revoke_key");
        // `rotate` in the key-id position is a key id, not the rotate action.
        assert_eq!(
            match_endpoint(Scope::Api, "POST", &format!("{KEYS}/rotate")),
            Match::MethodNotAllowed(vec![Method::Delete, Method::Patch])
        );
        assert_eq!(
            match_endpoint(Scope::Api, "GET", &format!("{KEYS}/{id}/rotate")),
            Match::MethodNotAllowed(vec![Method::Post])
        );
    }

    #[test]
    fn service_rest_name_is_rest() {
        assert_eq!(Service::Rest.name(), "rest");
    }

    #[test]
    fn auth_and_access_agree() {
        for ep in ENDPOINTS {
            let access = ep.policy.access;
            match ep.policy.auth {
                Auth::Public => assert_eq!(access, Access::Public, "{}", ep.name),
                Auth::DataKey | Auth::SecretDataKey | Auth::PublicDataKey => {
                    assert_eq!(access, Access::DataKey, "{}", ep.name)
                }
                Auth::Management => assert!(
                    !matches!(access, Access::Public | Access::DataKey),
                    "{}: a management route needs a member access level, not {access:?}",
                    ep.name
                ),
            }
        }
    }

    /// The design's per-route table (members and roles, part 4), one row per endpoint.
    #[test]
    fn spec_table() {
        use PlatformRole::{Admin, Developer, Viewer};
        const BRANCH: Access = Access::Branch { unprotected: Developer, protected: Admin };
        let table: &[(&str, Access)] = &[
            ("orchestrator.health", Access::Public),
            ("orchestrator.openapi_json", Access::Public),
            ("orchestrator.me", Access::SignedIn),
            // Console.
            ("orchestrator.console_query", Access::ActAs(Viewer)),
            ("orchestrator.console_schema", Access::ActAs(Viewer)),
            // SQL editor (cancel: the starter or Admin, refined in the handler).
            ("orchestrator.run_sql", Access::ActAs(Developer)),
            ("orchestrator.get_sql_schema", Access::ActAs(Developer)),
            ("orchestrator.cancel_sql_run", Access::Project(Developer)),
            // Table API (TE-A).
            ("orchestrator.list_objects", Access::Project(Viewer)),
            ("orchestrator.get_table", Access::Project(Viewer)),
            ("orchestrator.list_types", Access::Project(Viewer)),
            ("orchestrator.apply_schema_changes", Access::ActAsBranch { unprotected: Developer, protected: Admin }),
            ("orchestrator.insert_default_row", Access::ActAs(Developer)),
            ("orchestrator.get_cell", Access::ActAs(Viewer)),
            // Saved queries (edit, delete: the author or Admin, refined in the handler).
            ("orchestrator.list_saved_queries", Access::Project(Developer)),
            ("orchestrator.get_saved_query", Access::Project(Developer)),
            ("orchestrator.create_saved_query", Access::Project(Developer)),
            ("orchestrator.update_saved_query", Access::Project(Developer)),
            ("orchestrator.delete_saved_query", Access::Project(Developer)),
            // Connections.
            ("orchestrator.database_connection", Access::Project(Admin)),
            ("orchestrator.reset_password", Access::Project(Admin)),
            ("orchestrator.branch_connection", Access::Project(Admin)),
            ("orchestrator.branch_reset_password", Access::Project(Admin)),
            // Roles.
            ("orchestrator.list_roles", Access::Project(Viewer)),
            ("orchestrator.create_role", Access::Project(Admin)),
            ("orchestrator.update_role", Access::Project(Admin)),
            ("orchestrator.delete_role", Access::Project(Admin)),
            ("orchestrator.reset_role_password", Access::Project(Admin)),
            ("orchestrator.list_role_sessions", Access::Project(Admin)),
            // Data keys (Admin for a key not bound to an allowed role, refined in the handler).
            ("orchestrator.list_keys", Access::Project(Viewer)),
            ("orchestrator.data_api_schema", Access::Project(Viewer)),
            ("orchestrator.create_key", Access::Project(Developer)),
            ("orchestrator.rotate_key", Access::Project(Developer)),
            ("orchestrator.update_key", Access::Project(Developer)),
            ("orchestrator.revoke_key", Access::Project(Developer)),
            // Data API per Postgres database.
            ("orchestrator.enable_data_api", Access::Project(Admin)),
            ("orchestrator.disable_data_api", Access::Project(Admin)),
            ("orchestrator.update_pg_database", Access::Project(Admin)),
            // Branches.
            ("orchestrator.list_branches", Access::Project(Viewer)),
            ("orchestrator.list_branch_databases", Access::Project(Viewer)),
            ("orchestrator.create_branch", Access::Project(Developer)),
            ("orchestrator.branch_reset_to_parent", BRANCH),
            ("orchestrator.branch_restore", BRANCH),
            ("orchestrator.delete_branch", BRANCH),
            ("orchestrator.update_branch", Access::Project(Admin)),
            // Protected branches (P3): reads Developer, writes Admin.
            ("orchestrator.list_guard_exemptions", Access::Project(Developer)),
            ("orchestrator.put_guard_exemption", Access::Project(Admin)),
            ("orchestrator.delete_guard_exemption", Access::Project(Admin)),
            ("orchestrator.get_branch_review", Access::Project(Developer)),
            ("orchestrator.acknowledge_branch_review", Access::Project(Admin)),
            ("orchestrator.fix_branch_review_item", Access::Project(Admin)),
            // Databases.
            ("orchestrator.get_database", Access::Project(Viewer)),
            ("orchestrator.create_database", Access::Project(Admin)),
            ("orchestrator.update_database", Access::Project(Admin)),
            ("orchestrator.delete_database", Access::Project(Admin)),
            // Merges (by the target branch).
            ("orchestrator.list_merges", Access::Project(Viewer)),
            ("orchestrator.get_merge", Access::Project(Viewer)),
            ("orchestrator.export_merge_plan", Access::Project(Viewer)),
            ("orchestrator.create_merge", BRANCH),
            ("orchestrator.update_merge", BRANCH),
            ("orchestrator.rehearse_merge", BRANCH),
            ("orchestrator.recompute_merge", BRANCH),
            ("orchestrator.retry_merge", BRANCH),
            ("orchestrator.cancel_merge", BRANCH),
            ("orchestrator.abandon_merge", BRANCH),
            ("orchestrator.apply_merge", BRANCH),
            ("orchestrator.finish_merge", BRANCH),
            // Projects.
            ("orchestrator.list_projects", Access::TenantMember),
            ("orchestrator.create_project", Access::TenantAdmin),
            ("orchestrator.get_project", Access::Project(Viewer)),
            ("orchestrator.update_project", Access::Project(Admin)),
            ("orchestrator.delete_project", Access::Project(Admin)),
            // App users: settings changes Admin, management Developer, reads Viewer.
            ("orchestrator.get_app_users", Access::Project(Viewer)),
            ("orchestrator.put_app_users", Access::Project(Admin)),
            ("orchestrator.disable_app_users", Access::Project(Admin)),
            ("orchestrator.list_app_users", Access::Project(Viewer)),
            ("orchestrator.get_app_user", Access::Project(Viewer)),
            ("orchestrator.list_app_user_sessions", Access::Project(Viewer)),
            ("orchestrator.create_app_user", Access::Project(Developer)),
            ("orchestrator.update_app_user", Access::Project(Developer)),
            ("orchestrator.deactivate_app_user", Access::Project(Developer)),
            ("orchestrator.reactivate_app_user", Access::Project(Developer)),
            ("orchestrator.revoke_app_user_sessions", Access::Project(Developer)),
            ("orchestrator.delete_app_user", Access::Project(Developer)),
            // Members (part 4, "Members (new)").
            ("orchestrator.list_members", Access::TenantMember),
            ("orchestrator.update_member", Access::TenantAdmin),
            ("orchestrator.remove_member", Access::TenantAdmin),
            ("orchestrator.approve_member", Access::TenantAdmin),
            ("orchestrator.decline_member", Access::TenantAdmin),
            ("orchestrator.create_invite", Access::TenantAdmin),
            ("orchestrator.resend_invite", Access::TenantAdmin),
            ("orchestrator.revoke_invite", Access::TenantAdmin),
            ("orchestrator.create_project_invite", Access::Project(Admin)),
            ("orchestrator.list_project_grants", Access::Project(Admin)),
            ("orchestrator.put_project_grant", Access::Project(Admin)),
            ("orchestrator.delete_project_grant", Access::Project(Admin)),
            ("orchestrator.put_grant_override", Access::Project(Admin)),
            ("orchestrator.delete_grant_override", Access::Project(Admin)),
            ("orchestrator.list_access_events", Access::TenantAdmin),
            ("orchestrator.list_project_access_events", Access::Project(Admin)),
            ("orchestrator.get_instance_settings", Access::Instance),
            ("orchestrator.update_instance_settings", Access::Instance),
            ("orchestrator.get_invite_delivery", Access::TenantMember),
            ("orchestrator.update_tenant", Access::TenantAdmin),
            // Data API.
            ("rest.query", Access::DataKey),
            ("rest.schema", Access::DataKey),
            // Studio sign-in.
            ("auth.auth_request", Access::Public),
            ("auth.sign_in", Access::Public),
            ("auth.sign_up", Access::Public),
            ("auth.accept_invite", Access::Public),
            ("auth.describe_invite", Access::Public),
            ("auth.verify_email", Access::Public),
            ("auth.resend_code", Access::Public),
            ("auth.password_reset", Access::Public),
            ("auth.password_reset_confirm", Access::Public),
            ("auth.logout", Access::Public),
            // App users' own flows.
            ("auth.app_sign_up", Access::DataKey),
            ("auth.app_sign_in", Access::DataKey),
            ("auth.app_refresh", Access::DataKey),
            ("auth.app_sign_out", Access::DataKey),
            ("auth.app_user", Access::DataKey),
            ("auth.app_verify_email", Access::DataKey),
            ("auth.app_resend_code", Access::DataKey),
            ("auth.app_password_reset", Access::DataKey),
            ("auth.app_password_reset_confirm", Access::DataKey),
            ("auth.app_change_email", Access::DataKey),
            ("auth.app_change_password", Access::DataKey),
            ("auth.app_jwks", Access::Public),
        ];
        for (name, access) in table {
            let ep = ENDPOINTS
                .iter()
                .find(|e| e.name == *name)
                .unwrap_or_else(|| panic!("{name} is not in ENDPOINTS"));
            assert_eq!(ep.policy.access, *access, "{name}");
        }
        for ep in ENDPOINTS {
            assert!(table.iter().any(|(n, _)| *n == ep.name), "{} has no row in the table", ep.name);
        }
        assert_eq!(table.len(), ENDPOINTS.len());
    }
}
