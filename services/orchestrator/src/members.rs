//! The member management API (members and roles, parts 4 and 5): members, invites, project
//! grants and their per-database overrides, the access log, the invite delivery probe and the
//! tenant's display name.
//!
//! - Every access change is one metadata transaction that writes one `access_events` row, plus
//!   an `info` log line here.
//! - Grants are validated before they are stored: no reserved role name (R7), and below Admin
//!   only plain NOLOGIN groups whose whole chain is (R16, R21–R23).
//! - After a grant, override or removal change, every branch of the projects concerned is asked
//!   to reconcile its member roles (`chelabase.member_roles.reconcile`); the worker's sweep is
//!   the backstop for a lost message.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chelabase_metadata::members::{
    AccessEvent as EventRecord, AccessWriteError, GrantRow, InviteState, MemberListing, MemberRecord,
    MemberStatus, NewInvite, Observed, OpenInvite,
    OverrideRow, ProjectAccess, TeamSeen,
};
use chelabase_metadata::personal_logins::{
    AccessEventInput, GrantVia, LoginEndpoint, LoginWriteError, MemberProjectRole, PersonalLogin,
    RevokeReason,
};
use chelabase_metadata::{
    BranchStatus, DatabaseRecord, DatabaseStatus, MetadataClient, MetadataError,
};
use chelabase_nats::{MemberRolesReconcileRequest, SUBJECT_MEMBER_ROLES_ENQUEUE};
use chelabase_routes::orchestrator as ep;
use chelabase_types::access::{PgRole, PlatformRole, RoleSet, act_as_reserved};
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::role_name::check_any_role_name;
use chelabase_types::{ApiJson, ApiPath, BranchId, DatabaseId, ProjectId, TenantId};
use chelabase_zitadel::client::{EmailMatch, UserState, ZError, ZUser, Zitadel};
use chelabase_zitadel::refusals;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{error, info, warn};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::access::{RoleGraph, Target, authorize, authorize_project};
use crate::app_user_admin::{zitadel_failed, zitadel_unavailable};
use crate::{
    AllRoles, ApiBytes, AppState, AuthPrincipal, BoxError, InstancePrincipal, JobPort, MeRoles,
    MemberStatusView, MetadataPort, NatsPorts, roles,
};

/// The metadata the member API reads and writes ([`MetadataClient`]'s methods of the same
/// names, so tests can use a fake).
pub trait MembersPort: Send + Sync + 'static {
    /// A member by id, whatever its status or tenant.
    fn member(
        &self,
        member: Uuid,
    ) -> impl Future<Output = Result<Option<MemberRecord>, MetadataError>> + Send;

    /// The tenant's invited or active member holding `email` (a NULL email never matches).
    fn member_by_email(
        &self,
        tenant: TenantId,
        email: &str,
    ) -> impl Future<Output = Result<Option<MemberRecord>, MetadataError>> + Send;

    /// The tenant's invited and active members, with what the list adds.
    fn list_members(
        &self,
        tenant: TenantId,
    ) -> impl Future<Output = Result<Vec<MemberListing>, MetadataError>> + Send;

    /// One member as the list shows it, whatever its status.
    fn member_listing(
        &self,
        member: Uuid,
    ) -> impl Future<Output = Result<Option<MemberListing>, MetadataError>> + Send;

    /// The tenant's open invites.
    fn open_invites(
        &self,
        tenant: TenantId,
    ) -> impl Future<Output = Result<Vec<OpenInvite>, MetadataError>> + Send;

    /// An invite's member, expiry, and when it was revoked or accepted.
    fn invite(
        &self,
        invite: Uuid,
    ) -> impl Future<Output = Result<Option<InviteState>, MetadataError>> + Send;

    /// The project's grants and the overrides on its databases.
    fn project_access(
        &self,
        project: Uuid,
    ) -> impl Future<Output = Result<ProjectAccess, MetadataError>> + Send;

    /// Every live branch of every live database of the project.
    fn project_branches(
        &self,
        project: Uuid,
    ) -> impl Future<Output = Result<Vec<Uuid>, MetadataError>> + Send;

    fn open_signup(
        &self,
        tenant: TenantId,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// Whether the tenant's admin was claimed after `since` (the fresh-install race, M7).
    fn admin_claimed_since(
        &self,
        tenant: TenantId,
        since: DateTime<Utc>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// The tenant's access log, newest first; `member` keeps the events whose target or actor
    /// is that member.
    fn access_events(
        &self,
        tenant: TenantId,
        project: Option<Uuid>,
        member: Option<Uuid>,
        before_id: Option<i64>,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<EventRecord>, MetadataError>> + Send;

    /// The best level of each of the tenant's members on each project among their sources, and
    /// where it comes from (P13; the roster's `access`).
    fn member_project_roles(
        &self,
        tenant: TenantId,
    ) -> impl Future<Output = Result<Vec<MemberProjectRole>, MetadataError>> + Send;

    /// Writes one event as given (the role routes').
    fn record_access_event(
        &self,
        event: &AccessEventInput<'_>,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;

    /// Whether `name` is a personal login of the Chelabase database `database` (any status):
    /// the privilege ops refuse it as a grantee (MH-18, MH-28).
    fn is_personal_login_name(
        &self,
        database: Uuid,
        name: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// Members hub S12: every personal-login name (any status) of every database of `project`.
    fn personal_login_names(
        &self,
        project: Uuid,
    ) -> impl Future<Output = Result<BTreeSet<String>, MetadataError>> + Send;

    /// The member's own personal login on `database` (MH-15, D3, P6): `NotQualified`, `Exists`,
    /// `NameTaken`; a revoked or expired record is replaced.
    fn create_personal_login(
        &self,
        member: Uuid,
        database: Uuid,
        role_name: &str,
        scram_verifier: &str,
        valid_until: DateTime<Utc>,
    ) -> impl Future<Output = Result<PersonalLogin, LoginWriteError>> + Send;

    /// A live login's new verifier and `valid_until`, on the record and every row (P8).
    fn reset_personal_login(
        &self,
        member: Uuid,
        database: Uuid,
        scram_verifier: &str,
        valid_until: DateTime<Utc>,
    ) -> impl Future<Output = Result<(PersonalLogin, Vec<LoginEndpoint>), LoginWriteError>> + Send;

    /// Deletes the member's record on `database` (any status); returns where its rows were.
    fn delete_personal_login(
        &self,
        actor: Uuid,
        member: Uuid,
        database: Uuid,
    ) -> impl Future<Output = Result<Vec<LoginEndpoint>, LoginWriteError>> + Send;

    /// The member's logins, any status.
    fn member_personal_logins(
        &self,
        member: Uuid,
    ) -> impl Future<Output = Result<Vec<PersonalLogin>, MetadataError>> + Send;

    /// The database's logins, any status (P24).
    fn database_personal_logins(
        &self,
        database: Uuid,
    ) -> impl Future<Output = Result<Vec<PersonalLogin>, MetadataError>> + Send;

    /// Every personal row of the member, whatever its record's status (the proxy cache).
    fn member_login_endpoints(
        &self,
        member: Uuid,
    ) -> impl Future<Output = Result<Vec<LoginEndpoint>, MetadataError>> + Send;

    /// D5: suspended, logins revoked, member roles marked for rotation; the touched projects.
    /// Already suspended: `Ok`, no write (S7).
    fn suspend_member(
        &self,
        actor: Uuid,
        member: Uuid,
    ) -> impl Future<Output = Result<Vec<Uuid>, AccessWriteError>> + Send;

    /// Suspended → active. Already active: `Ok`, no write (S7). Returns the projects the
    /// member reaches (own and team grants, P16), the no-op's too.
    fn reinstate_member(
        &self,
        actor: Uuid,
        member: Uuid,
    ) -> impl Future<Output = Result<Vec<Uuid>, AccessWriteError>> + Send;

    /// P16 (b): logins revoked, member roles marked for rotation; the touched projects.
    fn revoke_postgres_access(
        &self,
        actor: Uuid,
        member: Uuid,
    ) -> impl Future<Output = Result<Vec<Uuid>, AccessWriteError>> + Send;

    /// Everything the member holds (own grants, overrides, team grants), one `Observed` per
    /// non-deleted project they reach, sorted by project: what a demotion is checked against.
    fn member_observed(
        &self,
        member: Uuid,
    ) -> impl Future<Output = Result<Vec<Observed>, MetadataError>> + Send;

    /// A demotion with `expect` (R17, P6, P25) compares, under the tenant lock then the
    /// member's, everything the member keeps with what the check read: `Err(Changed)` on any
    /// difference, nothing written. A promotion or an unchanged flag compares nothing.
    fn set_tenant_admin(
        &self,
        actor: Option<Uuid>,
        member: Uuid,
        admin: bool,
        expect: Option<&[Observed]>,
    ) -> impl Future<Output = Result<(), AccessWriteError>> + Send;

    /// Returns the projects whose branches need a member-role reconcile.
    fn remove_member(
        &self,
        actor: Uuid,
        member: Uuid,
    ) -> impl Future<Output = Result<Vec<Uuid>, AccessWriteError>> + Send;

    /// I4: `expect` is what the handler checked the grant against; the write is
    /// `Err(Changed)` if, under the member's lock, the access differs.
    fn put_project_grant(
        &self,
        actor: Uuid,
        member: Uuid,
        project: Uuid,
        grant: &GrantRow,
        expect: &Observed,
    ) -> impl Future<Output = Result<bool, AccessWriteError>> + Send;

    /// Several grants in one transaction (all or none), each after the check of its `expect`,
    /// and the memberships of `teams` (P12; `Changed` when a team isn't what the check read,
    /// P25); returns the projects whose access changed.
    fn put_project_grants(
        &self,
        actor: Uuid,
        member: Uuid,
        grants: &[(Uuid, GrantRow)],
        teams: &[TeamSeen],
        expect: &[Observed],
    ) -> impl Future<Output = Result<Vec<Uuid>, AccessWriteError>> + Send;

    /// I4, MH-4: `expect` is what the handler checked the member's remaining access against;
    /// `Err(Changed)` if, under the member's lock, the access differs.
    fn delete_project_grant(
        &self,
        actor: Uuid,
        member: Uuid,
        project: Uuid,
        expect: &Observed,
    ) -> impl Future<Output = Result<(), AccessWriteError>> + Send;

    fn put_override(
        &self,
        actor: Uuid,
        member: Uuid,
        database: Uuid,
        o: &OverrideRow,
        expect: &Observed,
    ) -> impl Future<Output = Result<bool, AccessWriteError>> + Send;

    fn delete_override(
        &self,
        actor: Uuid,
        member: Uuid,
        database: Uuid,
        expect: &Observed,
    ) -> impl Future<Output = Result<(), AccessWriteError>> + Send;

    /// Returns the invited member (as the list shows it) and the invite's id.
    fn create_invited_member(
        &self,
        actor: Uuid,
        tenant: TenantId,
        new: NewInvite<'_>,
    ) -> impl Future<Output = Result<(MemberListing, Uuid), AccessWriteError>> + Send;

    /// A person already in the tenant's org, added active, in `teams` (P12, P25).
    #[allow(clippy::too_many_arguments)]
    fn add_existing_user(
        &self,
        actor: Uuid,
        tenant: TenantId,
        zitadel_user_id: &str,
        email: &str,
        name: &str,
        grants: &[(Uuid, GrantRow)],
        teams: &[TeamSeen],
    ) -> impl Future<Output = Result<MemberListing, AccessWriteError>> + Send;

    /// Also replaces the invite's token (by its hash).
    fn resend_invite(
        &self,
        actor: Uuid,
        invite: Uuid,
        expires_at: DateTime<Utc>,
        token_hash: [u8; 32],
    ) -> impl Future<Output = Result<(), AccessWriteError>> + Send;

    /// Returns the never-activated Zitadel user to delete.
    fn revoke_invite(
        &self,
        actor: Uuid,
        invite: Uuid,
    ) -> impl Future<Output = Result<String, AccessWriteError>> + Send;

    /// [`Self::revoke_invite`] for an invite whose code couldn't be sent: the event says so
    /// (`after.reason: "delivery_failed"`).
    fn revoke_undelivered_invite(
        &self,
        actor: Uuid,
        invite: Uuid,
    ) -> impl Future<Output = Result<String, AccessWriteError>> + Send;

    fn set_open_signup(
        &self,
        actor: Uuid,
        tenant: TenantId,
        on: bool,
    ) -> impl Future<Output = Result<(), AccessWriteError>> + Send;

    /// Approves a pending member with its first grants and teams: active, one transaction, one
    /// event (and one `team_member_added` per team, P12, P25).
    fn approve_member(
        &self,
        actor: Uuid,
        member: Uuid,
        grants: &[(Uuid, GrantRow)],
        teams: &[TeamSeen],
    ) -> impl Future<Output = Result<MemberListing, AccessWriteError>> + Send;

    /// Declines a pending member: removed. Returns its Zitadel user, to deactivate.
    fn decline_member(
        &self,
        actor: Uuid,
        member: Uuid,
    ) -> impl Future<Output = Result<String, AccessWriteError>> + Send;

    /// Sets (or, with `None`, clears) the tenant's display name; an unchanged name writes
    /// nothing.
    fn set_tenant_display_name(
        &self,
        actor: Uuid,
        tenant: TenantId,
        display_name: Option<&str>,
    ) -> impl Future<Output = Result<(), AccessWriteError>> + Send;
}

// Each method delegates to `MetadataClient`'s inherent method of the same name (spelled out
// as `MetadataClient::name`, so a rename can't make it call itself).
impl MembersPort for MetadataClient {
    async fn member(&self, member: Uuid) -> Result<Option<MemberRecord>, MetadataError> {
        MetadataClient::member(self, member).await
    }

    async fn member_by_email(
        &self,
        tenant: TenantId,
        email: &str,
    ) -> Result<Option<MemberRecord>, MetadataError> {
        MetadataClient::member_by_email(self, tenant, email).await
    }

    async fn list_members(&self, tenant: TenantId) -> Result<Vec<MemberListing>, MetadataError> {
        MetadataClient::list_member_listings(self, tenant).await
    }

    async fn member_listing(&self, member: Uuid) -> Result<Option<MemberListing>, MetadataError> {
        MetadataClient::member_listing(self, member).await
    }

    async fn open_invites(&self, tenant: TenantId) -> Result<Vec<OpenInvite>, MetadataError> {
        MetadataClient::open_invites(self, tenant).await
    }

    async fn invite(
        &self,
        invite: Uuid,
    ) -> Result<Option<InviteState>, MetadataError> {
        MetadataClient::invite(self, invite).await
    }

    async fn project_access(&self, project: Uuid) -> Result<ProjectAccess, MetadataError> {
        MetadataClient::project_access(self, project).await
    }

    async fn project_branches(&self, project: Uuid) -> Result<Vec<Uuid>, MetadataError> {
        MetadataClient::project_branches(self, project).await
    }

    async fn open_signup(&self, tenant: TenantId) -> Result<bool, MetadataError> {
        MetadataClient::open_signup(self, tenant).await
    }

    async fn admin_claimed_since(&self, tenant: TenantId, since: DateTime<Utc>) -> Result<bool, MetadataError> {
        MetadataClient::admin_claimed_since(self, tenant, since).await
    }

    async fn access_events(
        &self,
        tenant: TenantId,
        project: Option<Uuid>,
        member: Option<Uuid>,
        before_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<EventRecord>, MetadataError> {
        MetadataClient::access_events(self, tenant, project, member, before_id, limit).await
    }

    async fn member_project_roles(
        &self,
        tenant: TenantId,
    ) -> Result<Vec<MemberProjectRole>, MetadataError> {
        MetadataClient::member_project_roles(self, tenant).await
    }

    async fn record_access_event(&self, event: &AccessEventInput<'_>) -> Result<(), MetadataError> {
        MetadataClient::record_access_event(self, event).await
    }

    async fn is_personal_login_name(&self, database: Uuid, name: &str) -> Result<bool, MetadataError> {
        MetadataClient::is_personal_login_name(self, database, name).await
    }

    async fn personal_login_names(&self, project: Uuid) -> Result<BTreeSet<String>, MetadataError> {
        MetadataClient::personal_login_names(self, project).await
    }

    async fn create_personal_login(
        &self,
        member: Uuid,
        database: Uuid,
        role_name: &str,
        scram_verifier: &str,
        valid_until: DateTime<Utc>,
    ) -> Result<PersonalLogin, LoginWriteError> {
        MetadataClient::create_personal_login(self, member, database, role_name, scram_verifier, valid_until)
            .await
    }

    async fn reset_personal_login(
        &self,
        member: Uuid,
        database: Uuid,
        scram_verifier: &str,
        valid_until: DateTime<Utc>,
    ) -> Result<(PersonalLogin, Vec<LoginEndpoint>), LoginWriteError> {
        MetadataClient::reset_personal_login(self, member, database, scram_verifier, valid_until).await
    }

    async fn delete_personal_login(
        &self,
        actor: Uuid,
        member: Uuid,
        database: Uuid,
    ) -> Result<Vec<LoginEndpoint>, LoginWriteError> {
        MetadataClient::delete_personal_login(self, actor, member, database).await
    }

    async fn member_personal_logins(&self, member: Uuid) -> Result<Vec<PersonalLogin>, MetadataError> {
        MetadataClient::member_personal_logins(self, member).await
    }

    async fn database_personal_logins(&self, database: Uuid) -> Result<Vec<PersonalLogin>, MetadataError> {
        MetadataClient::database_personal_logins(self, database).await
    }

    async fn member_login_endpoints(&self, member: Uuid) -> Result<Vec<LoginEndpoint>, MetadataError> {
        MetadataClient::member_login_endpoints(self, member).await
    }

    async fn suspend_member(&self, actor: Uuid, member: Uuid) -> Result<Vec<Uuid>, AccessWriteError> {
        MetadataClient::suspend_member(self, actor, member).await
    }

    async fn reinstate_member(&self, actor: Uuid, member: Uuid) -> Result<Vec<Uuid>, AccessWriteError> {
        MetadataClient::reinstate_member(self, actor, member).await
    }

    async fn revoke_postgres_access(&self, actor: Uuid, member: Uuid) -> Result<Vec<Uuid>, AccessWriteError> {
        MetadataClient::revoke_postgres_access(self, actor, member).await
    }

    async fn member_observed(&self, member: Uuid) -> Result<Vec<Observed>, MetadataError> {
        MetadataClient::member_observed(self, member).await
    }

    async fn set_tenant_admin(
        &self,
        actor: Option<Uuid>,
        member: Uuid,
        admin: bool,
        expect: Option<&[Observed]>,
    ) -> Result<(), AccessWriteError> {
        MetadataClient::set_tenant_admin_checked(self, actor, member, admin, expect).await
    }

    async fn remove_member(
        &self,
        actor: Uuid,
        member: Uuid,
    ) -> Result<Vec<Uuid>, AccessWriteError> {
        MetadataClient::remove_member(self, actor, member).await
    }

    async fn put_project_grant(
        &self,
        actor: Uuid,
        member: Uuid,
        project: Uuid,
        grant: &GrantRow,
        expect: &Observed,
    ) -> Result<bool, AccessWriteError> {
        MetadataClient::put_project_grant_checked(self, actor, member, project, grant, expect).await
    }

    async fn put_project_grants(
        &self,
        actor: Uuid,
        member: Uuid,
        grants: &[(Uuid, GrantRow)],
        teams: &[TeamSeen],
        expect: &[Observed],
    ) -> Result<Vec<Uuid>, AccessWriteError> {
        MetadataClient::put_project_grants_checked(self, actor, member, grants, teams, expect).await
    }

    async fn delete_project_grant(
        &self,
        actor: Uuid,
        member: Uuid,
        project: Uuid,
        expect: &Observed,
    ) -> Result<(), AccessWriteError> {
        MetadataClient::delete_project_grant_checked(self, actor, member, project, Some(expect)).await
    }

    async fn put_override(
        &self,
        actor: Uuid,
        member: Uuid,
        database: Uuid,
        o: &OverrideRow,
        expect: &Observed,
    ) -> Result<bool, AccessWriteError> {
        MetadataClient::put_override_checked(self, actor, member, database, o, Some(expect)).await
    }

    async fn delete_override(
        &self,
        actor: Uuid,
        member: Uuid,
        database: Uuid,
        expect: &Observed,
    ) -> Result<(), AccessWriteError> {
        MetadataClient::delete_override_checked(self, actor, member, database, Some(expect)).await
    }

    async fn create_invited_member(
        &self,
        actor: Uuid,
        tenant: TenantId,
        new: NewInvite<'_>,
    ) -> Result<(MemberListing, Uuid), AccessWriteError> {
        MetadataClient::create_invited_member(self, actor, tenant, new).await
    }

    async fn add_existing_user(
        &self,
        actor: Uuid,
        tenant: TenantId,
        zitadel_user_id: &str,
        email: &str,
        name: &str,
        grants: &[(Uuid, GrantRow)],
        teams: &[TeamSeen],
    ) -> Result<MemberListing, AccessWriteError> {
        MetadataClient::add_existing_user(self, actor, tenant, zitadel_user_id, email, name, grants, teams)
            .await
    }

    async fn resend_invite(
        &self,
        actor: Uuid,
        invite: Uuid,
        expires_at: DateTime<Utc>,
        token_hash: [u8; 32],
    ) -> Result<(), AccessWriteError> {
        MetadataClient::resend_invite(self, actor, invite, expires_at, token_hash).await
    }

    async fn revoke_invite(&self, actor: Uuid, invite: Uuid) -> Result<String, AccessWriteError> {
        MetadataClient::revoke_invite(self, actor, invite).await
    }

    async fn revoke_undelivered_invite(&self, actor: Uuid, invite: Uuid) -> Result<String, AccessWriteError> {
        MetadataClient::revoke_invite_because(self, actor, invite, Some("delivery_failed")).await
    }

    async fn set_open_signup(
        &self,
        actor: Uuid,
        tenant: TenantId,
        on: bool,
    ) -> Result<(), AccessWriteError> {
        MetadataClient::set_open_signup(self, actor, tenant, on).await
    }

    async fn approve_member(
        &self,
        actor: Uuid,
        member: Uuid,
        grants: &[(Uuid, GrantRow)],
        teams: &[TeamSeen],
    ) -> Result<MemberListing, AccessWriteError> {
        MetadataClient::approve_member(self, actor, member, grants, teams).await
    }

    async fn decline_member(&self, actor: Uuid, member: Uuid) -> Result<String, AccessWriteError> {
        MetadataClient::decline_member(self, actor, member).await
    }

    async fn set_tenant_display_name(
        &self,
        actor: Uuid,
        tenant: TenantId,
        display_name: Option<&str>,
    ) -> Result<(), AccessWriteError> {
        MetadataClient::set_tenant_display_name(self, actor, tenant, display_name).await
    }
}

/// Asks the worker to reconcile a branch's member roles soon.
pub trait MemberRolesQueue: Send + Sync + 'static {
    /// A core-NATS publish on `chelabase.member_roles.reconcile` (the worker batches a burst).
    fn enqueue_member_roles(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<(), BoxError>> + Send;
}

impl MemberRolesQueue for NatsPorts {
    async fn enqueue_member_roles(&self, branch: BranchId) -> Result<(), BoxError> {
        let body = serde_json::to_vec(&MemberRolesReconcileRequest {
            branch_id: branch.as_uuid(),
            member_id: None,
            from_ddl: false,
        })?;
        self.client
            .publish(SUBJECT_MEMBER_ROLES_ENQUEUE, body.into())
            .await?;
        Ok(())
    }
}

/// How long an invite code lives. Zitadel's default invite-code lifetime is 72 hours; the stored
/// `expires_at` mirrors it (Zitadel itself refuses an older code).
const INVITE_LIFETIME_HOURS: i64 = 72;

/// Access-log pages: the default and the most a page may hold.
const EVENTS_DEFAULT: u32 = 50;
const EVENTS_MAX: u32 = 200;

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_members))
        .routes(routes!(update_member, remove_member))
        .routes(routes!(approve_member))
        .routes(routes!(decline_member))
        .routes(routes!(suspend_member))
        .routes(routes!(reinstate_member))
        .routes(routes!(revoke_member_postgres_access))
        .routes(routes!(create_invite))
        .routes(routes!(resend_invite))
        .routes(routes!(revoke_invite))
        .routes(routes!(create_project_invite))
        .routes(routes!(list_project_grants))
        .routes(routes!(put_project_grant, delete_project_grant))
        .routes(routes!(put_grant_override, delete_grant_override))
        .routes(routes!(list_access_events))
        .routes(routes!(list_project_access_events))
        .routes(routes!(get_instance_settings, update_instance_settings))
        .routes(routes!(get_invite_delivery))
        .routes(routes!(update_tenant))
}

// --- Shapes ------------------------------------------------------------------------------------

/// A member of the tenant: a person in its Zitadel organisation.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct Member {
    id: Uuid,
    /// Lowercased. `null` while unknown, or when another member claimed the address since
    /// (the newest claim wins; it comes back with the member's next request).
    #[schema(required = true)]
    email: Option<String>,
    /// Empty until the member's first sign-in brings their name (an invitee's is their email's
    /// local part).
    display_name: String,
    tenant_admin: bool,
    status: MemberStatusView,
    /// Whether the member can reach anything: a tenant admin, or holding at least one project
    /// grant (an override alone doesn't count). A member without it sees "no access yet".
    has_access: bool,
    /// What the member can reach, over the projects the caller can see.
    access: MemberAccess,
    /// When the member row was made (an invited member's: when they were invited).
    created_at: DateTime<Utc>,
    /// When an invited member first signed in.
    #[schema(required = true)]
    accepted_at: Option<DateTime<Utc>>,
    /// When the member was suspended; `null` unless suspended (or removed after a suspension).
    #[schema(required = true)]
    suspended_at: Option<DateTime<Utc>>,
    /// The member who suspended it; `null` unless suspended, and for a member suspended before
    /// this was recorded.
    #[schema(required = true)]
    suspended_by: Option<Uuid>,
    /// The open invite of an invited member (resend or revoke it by its id); `null` otherwise.
    #[schema(required = true)]
    invite: Option<Invite>,
}

/// The highest platform role a member holds over the projects the caller can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccessLevel {
    /// A tenant admin: admin on every project.
    TenantAdmin,
    Admin,
    Developer,
    Viewer,
    /// Access only on projects the caller can't see.
    Other,
    /// No project grant anywhere (an override alone is none).
    None,
}

/// What a member can reach, as the caller may see it: only projects the caller can see are
/// named (a Developer never learns of grants elsewhere), and never a Postgres role or override.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct MemberAccess {
    level: AccessLevel,
    /// The member's platform role on each project the caller can see, by project id.
    projects: Vec<MemberProjectAccess>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct MemberProjectAccess {
    project_id: ProjectId,
    platform_role: PlatformRole,
    /// Where the role comes from: `own` (the member's own grant; a tenant admin's admin is
    /// listed the same way), `team` (only through teams) or `both`. The role is the best
    /// source's.
    via: AccessVia,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccessVia {
    Own,
    Team,
    Both,
}

impl From<GrantVia> for AccessVia {
    fn from(via: GrantVia) -> Self {
        match via {
            GrantVia::Own => Self::Own,
            GrantVia::Team => Self::Team,
            GrantVia::Both => Self::Both,
        }
    }
}

/// One member's level on one project with its source (the roster's row, P13).
type RosterRole = (Uuid, PlatformRole, AccessVia);

/// What the roster shows of one member: `visible` is the projects the caller can see, `all` the
/// tenant's levels `(project, role, via)` by member (own and team grants, P13).
fn member_access(
    member: &MemberRecord,
    visible: &HashSet<Uuid>,
    all: &HashMap<Uuid, Vec<RosterRole>>,
) -> MemberAccess {
    let row = |project: Uuid, platform_role, via| MemberProjectAccess {
        project_id: ProjectId::from(project),
        platform_role,
        via,
    };
    if member.tenant_admin {
        let mut projects: Vec<Uuid> = visible.iter().copied().collect();
        projects.sort();
        return MemberAccess {
            level: AccessLevel::TenantAdmin,
            projects: projects
                .into_iter()
                .map(|p| row(p, PlatformRole::Admin, AccessVia::Own))
                .collect(),
        };
    }
    let mut grants = all.get(&member.id).cloned().unwrap_or_default();
    let any = !grants.is_empty();
    // R1: a project the caller can't see is never named, nor counted in the level.
    grants.retain(|(p, _, _)| visible.contains(p));
    grants.sort_by_key(|(p, _, _)| *p);
    let level = match grants.iter().map(|(_, r, _)| *r).max() {
        Some(PlatformRole::Admin) => AccessLevel::Admin,
        Some(PlatformRole::Developer) => AccessLevel::Developer,
        Some(PlatformRole::Viewer) => AccessLevel::Viewer,
        None if any => AccessLevel::Other,
        None => AccessLevel::None,
    };
    MemberAccess { level, projects: grants.into_iter().map(|(p, r, via)| row(p, r, via)).collect() }
}

/// An open invite.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub(crate) struct Invite {
    id: Uuid,
    /// When its code stops working (72 hours after it was sent); resend it for a new one.
    expires_at: DateTime<Utc>,
}

impl Member {
    fn new(l: MemberListing, invite: Option<Invite>, access: MemberAccess) -> Self {
        let m = l.member;
        Self {
            id: m.id,
            email: m.email,
            display_name: m.display_name,
            tenant_admin: m.tenant_admin,
            status: m.status.into(),
            has_access: l.has_access,
            access,
            created_at: l.created_at,
            accepted_at: m.accepted_at,
            suspended_at: m.suspended_at,
            suspended_by: m.suspended_by,
            invite: invite.filter(|_| m.status == MemberStatus::Invited),
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct UpdateMemberRequest {
    /// Only an active member may become a tenant admin; the last active tenant admin can't
    /// step down.
    tenant_admin: bool,
}

/// A grant an invite carries: a platform role on a project and the Postgres roles it may act
/// as there.
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct InviteGrant {
    project_id: ProjectId,
    platform_role: PlatformRole,
    /// Postgres role names; for `admin`, which holds every role (R17), checked and dropped.
    /// Never `owner` or the database's owner (the admin platform role includes it), nor a
    /// reserved name. Below admin, only NOLOGIN groups whose memberships reach no owner-level,
    /// LOGIN or reserved role.
    #[serde(default)]
    pg_roles: Vec<String>,
}

/// The body of `approveMember`: the first grants of the person who signed up.
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct ApproveMemberRequest {
    /// One grant per project at most; none is fine (the person starts with no access). Checked
    /// as an invite's grants are.
    #[serde(default)]
    grants: Vec<InviteGrant>,
    /// Teams of the tenant the member joins (at most 50 after duplicates are dropped); empty is
    /// none. Checked with the grants on the member's union, as an invite's are.
    #[serde(default)]
    team_ids: Vec<Uuid>,
}

/// How an invite reaches the invitee.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Delivery {
    /// Zitadel mails the link (needs an active email provider, see `getInviteDelivery`).
    #[default]
    Email,
    /// The response carries the link (`invite_url`) once; the admin hands it over. Nothing is
    /// mailed.
    Link,
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct CreateInviteRequest {
    /// Trimmed and lowercased.
    email: String,
    /// One grant per project at most; none is fine (the person starts with no access).
    #[serde(default)]
    grants: Vec<InviteGrant>,
    /// Teams of the tenant the person joins (at most 50 after duplicates are dropped; each
    /// must be a team of the tenant, else 422 naming it in `team_id`); empty is none. The
    /// grants and the teams are checked together on the person's union, per project.
    #[serde(default)]
    team_ids: Vec<Uuid>,
    /// How the invitee gets the link. Default `email`.
    #[serde(default)]
    delivery: Delivery,
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct CreateProjectInviteRequest {
    /// Trimmed and lowercased.
    email: String,
    platform_role: PlatformRole,
    /// As in a project grant.
    #[serde(default)]
    pg_roles: Vec<String>,
    /// How the invitee gets the link. Default `email`.
    #[serde(default)]
    delivery: Delivery,
    /// Refused in any form, `[]` and `null` included (422 `invalid_request`): only a tenant
    /// admin's invite (`createInvite`) attaches teams (MH-8).
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<Vec<Uuid>>)]
    team_ids: Option<Value>,
}

/// P31: any value present, `null` included, is `Some` (a missing field is `None` through
/// `#[serde(default)]`).
fn present<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

/// The body of `resendInvite`; the whole body may be left out (then `email`).
#[derive(Debug, Default, Deserialize, ToSchema)]
pub(crate) struct ResendInviteRequest {
    /// How the invitee gets the new link. Default `email`.
    #[serde(default)]
    delivery: Delivery,
}

/// What an invite did.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InviteOutcome {
    /// A new person: Zitadel mailed them an invite link (or, with `link` delivery, the response
    /// carries it); they are `invited` until they accept.
    Invited,
    /// A person already in the organisation: now an active member, no mail sent.
    Added,
    /// Already a member: they got the grants.
    ExistingMember,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct InviteResult {
    outcome: InviteOutcome,
    member: Member,
    /// With `delivery: "link"` and outcome `invited`: the full `/accept-invite` link. It is
    /// shown this once, never stored or logged, and the response is `Cache-Control: no-store`.
    #[serde(skip_serializing_if = "Option::is_none")]
    invite_url: Option<String>,
}

/// A resent invite.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct ResentInvite {
    id: Uuid,
    /// When its new code stops working.
    expires_at: DateTime<Utc>,
    /// With `delivery: "link"`: the new `/accept-invite` link. It is shown this once, never
    /// stored or logged, and the response is `Cache-Control: no-store`.
    #[serde(skip_serializing_if = "Option::is_none")]
    invite_url: Option<String>,
}

/// Why invites can't be emailed.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeliveryReason {
    /// Zitadel has no active email provider.
    NoEmailProvider,
    /// Zitadel couldn't be asked.
    ZitadelUnavailable,
    /// This instance has no Zitadel admin access.
    ZitadelNotConfigured,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct InviteDelivery {
    /// Whether Zitadel has an active email provider, so `email` delivery works.
    email_available: bool,
    /// Why not; `null` when it is available.
    #[schema(required = true)]
    reason: Option<DeliveryReason>,
}

/// The tenant's name.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct TenantName {
    /// `null` when it has none: show the slug.
    #[schema(required = true)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct UpdateTenantRequest {
    /// Trimmed; at most 80 characters, no control characters. An empty or `null` value clears
    /// it. The field itself is required.
    #[serde(deserialize_with = "Option::deserialize")]
    #[schema(required = true)]
    display_name: Option<String>,
}

/// A member's grant on a project, with its narrowing overrides on the project's databases.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct ProjectGrant {
    member_id: Uuid,
    /// `admin` includes the database owner.
    platform_role: PlatformRole,
    /// `"all"` for `admin`: every role on the project (narrowed per database by `overrides`),
    /// except the data API's `anon` and `authenticated` for acting as them, which need a grant
    /// naming them. Below `admin` the names, sorted; a granted role includes every role it can
    /// `SET ROLE` to.
    pg_roles: MeRoles,
    overrides: Vec<GrantOverride>,
}

/// A narrowing of a grant on one Chelabase database. A `null` field doesn't narrow.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct GrantOverride {
    database_id: DatabaseId,
    #[schema(required = true)]
    platform_role: Option<PlatformRole>,
    #[schema(required = true)]
    pg_roles: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct PutGrantRequest {
    platform_role: PlatformRole,
    /// Postgres role names; for `admin`, which holds every role (R17), checked and dropped.
    /// Never `owner` or the database's owner (the admin platform role includes it), nor a
    /// reserved name. Below admin, only NOLOGIN groups whose memberships reach no owner-level,
    /// LOGIN or reserved role.
    #[serde(default)]
    pg_roles: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct PutGrantOverrideRequest {
    /// A database of this project.
    database_id: DatabaseId,
    /// At most the grant's role.
    #[serde(default)]
    platform_role: Option<PlatformRole>,
    /// Some of the grant's roles.
    #[serde(default)]
    pg_roles: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct OverrideQuery {
    database_id: Uuid,
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    before: Option<i64>,
    limit: Option<u32>,
    member_id: Option<Uuid>,
}

/// One access change.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct AccessEvent {
    id: i64,
    /// Who made it; `null` for the system (the bootstrap, the first sign-in's admin claim).
    #[schema(required = true)]
    actor_member_id: Option<Uuid>,
    /// `member_joined`, `member_added`, `admin_claimed`, `admin_set`, `admin_unset`,
    /// `member_removed`, `grant_put`, `grant_deleted`, `override_put`, `override_deleted`,
    /// `invite_created`, `invite_resent`, `invite_revoked`, `invite_accepted`,
    /// `open_signup_set`, `tenant_renamed`, `member_approved`, `member_declined`,
    /// `member_suspended`, `member_reinstated`, `personal_logins_revoked`,
    /// `personal_login_created`, `personal_login_reset`, `personal_login_deleted`,
    /// `role_created`, `role_updated`, `role_deleted`, `team_created`, `team_updated`,
    /// `team_deleted`, `team_grant_put`, `team_grant_deleted`, `team_member_added` or
    /// `team_member_removed`. A team event's `before`/`after` carries the team, the grants it
    /// concerns and `affected_member_ids` (empty in a project's log).
    action: String,
    #[schema(required = true)]
    target_member_id: Option<Uuid>,
    #[schema(required = true)]
    project_id: Option<Uuid>,
    #[schema(required = true)]
    database_id: Option<Uuid>,
    /// The branch of a per-branch change (a role's); `null` for the others.
    #[schema(required = true)]
    branch_id: Option<Uuid>,
    /// The actor's email now (a removed member's too); `null` for the system, or when their
    /// address went stale.
    #[schema(required = true)]
    actor_email: Option<String>,
    /// The target's email now; `null` when there is no target or its address went stale.
    #[schema(required = true)]
    target_email: Option<String>,
    /// The project's name now (a deleted project's last name); `null` when the event has no
    /// project or its row is gone.
    #[schema(required = true)]
    project_name: Option<String>,
    #[schema(value_type = Option<Object>, required = true)]
    before: Option<Value>,
    #[schema(value_type = Option<Object>, required = true)]
    after: Option<Value>,
    at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct AccessEventPage {
    /// Newest first.
    events: Vec<AccessEvent>,
    /// Pass as `before` for the next page; `null` on the last one.
    #[schema(required = true)]
    next_before: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct InstanceSettings {
    /// Whether anyone may sign up (they start with no access). Off by default.
    open_signup: bool,
    /// Whether this instance lets it be switched on (self-hosted only).
    open_signup_switchable: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct UpdateInstanceSettingsRequest {
    open_signup: bool,
}

// --- Helpers -----------------------------------------------------------------------------------

pub(crate) fn unprocessable(message: impl Into<String>) -> ApiError {
    ApiError::invalid_request(422, message)
}

pub(crate) fn internal(what: &str, err: impl std::fmt::Display) -> ApiError {
    error!("{what}: {err}");
    ApiError::internal()
}

/// A refused write's answer; `not_found` is the message for `NotFound`.
pub(crate) fn write_failed(what: &str, err: AccessWriteError, not_found: &str) -> ApiError {
    match err {
        AccessWriteError::LastTenantAdmin => ApiError::new(
            409,
            ErrorCode::LastTenantAdmin,
            "the tenant must keep at least one active tenant admin",
        ),
        AccessWriteError::NotFound => ApiError::not_found(not_found),
        AccessWriteError::MemberNotFound => ApiError::not_found("member not found"),
        AccessWriteError::Conflict(message) => ApiError::new(409, ErrorCode::Conflict, message),
        AccessWriteError::Changed => ApiError::new(
            409,
            ErrorCode::Conflict,
            "the member's access changed while this was being checked; try again",
        ),
        AccessWriteError::Db(err) => internal(what, err),
    }
}

/// A checked write is tried twice (I4): when the first finds the member's access changed since
/// the check read it, the check runs again on the new state, and a second change is a 409.
pub(crate) const CHECKED_WRITE_TRIES: usize = 2;

/// The caller's member id (an access check that passed means there is one).
pub(crate) fn actor(member: Option<&MemberRecord>) -> Result<Uuid, ApiError> {
    member
        .map(|m| m.id)
        .ok_or_else(|| internal("member route", "allowed a caller with no member row"))
}

/// A member of the tenant by id, whatever its status; another tenant's is 404 like a missing one.
pub(crate) async fn tenant_member<M, J>(
    state: &AppState<M, J>,
    tenant: TenantId,
    id: Uuid,
) -> Result<MemberRecord, ApiError>
where
    M: MetadataPort,
{
    match state.metadata.member(id).await {
        Ok(Some(m)) if m.tenant_id == tenant => Ok(m),
        Ok(_) => Err(ApiError::not_found("member not found")),
        Err(err) => Err(internal("load member", err)),
    }
}

/// [`tenant_member`], refusing a removed one.
async fn live_member<M, J>(
    state: &AppState<M, J>,
    tenant: TenantId,
    id: Uuid,
) -> Result<MemberRecord, ApiError>
where
    M: MetadataPort,
{
    let m = tenant_member(state, tenant, id).await?;
    if m.status == MemberStatus::Removed {
        return Err(ApiError::not_found("member not found"));
    }
    Ok(m)
}

/// The ids of the projects the caller can see: all of a tenant admin's tenant, else the ones it
/// holds a grant on (R1).
async fn visible_projects<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
) -> Result<HashSet<Uuid>, ApiError>
where
    M: MetadataPort,
{
    let Some(caller) = principal.member.as_ref() else {
        return Err(ApiError::internal());
    };
    if caller.tenant_admin {
        let projects = state
            .metadata
            .list_projects(principal.tenant_id)
            .await
            .map_err(|err| internal("list projects", err))?;
        return Ok(projects.into_iter().map(|p| p.id.as_uuid()).collect());
    }
    // T2-4: every access source.
    let granted = state
        .metadata
        .member_grant_sources(principal.tenant_id, caller.id)
        .await
        .map_err(|err| internal("caller projects", err))?;
    Ok(granted.into_iter().map(|(project, _)| project).collect())
}

/// The tenant's levels by member: `(project, best level, via)` (P13).
async fn grants_by_member<M, J>(
    state: &AppState<M, J>,
    tenant: TenantId,
) -> Result<HashMap<Uuid, Vec<RosterRole>>, ApiError>
where
    M: MetadataPort,
{
    let mut out: HashMap<Uuid, Vec<RosterRole>> = HashMap::new();
    for r in state
        .metadata
        .member_project_roles(tenant)
        .await
        .map_err(|err| internal("member project roles", err))?
    {
        out.entry(r.member).or_default().push((r.project, r.platform_role, r.via.into()));
    }
    Ok(out)
}

/// One member's `access` as the caller sees it.
async fn access_for<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    m: &MemberRecord,
) -> Result<MemberAccess, ApiError>
where
    M: MetadataPort,
{
    let visible = visible_projects(state, principal).await?;
    let all = grants_by_member(state, principal.tenant_id).await?;
    Ok(member_access(m, &visible, &all))
}

/// The levels a write just gave: per project of `rows` and `teams`, the best level among the
/// own grant and the teams' grants there, and where it comes from (P13), by project.
fn written_levels(rows: &[(Uuid, GrantRow)], teams: &[TeamSeen]) -> Vec<RosterRole> {
    invite_projects(rows, teams)
        .into_iter()
        .filter_map(|p| {
            let own = rows.iter().find(|(x, _)| *x == p).map(|(_, g)| g.platform_role);
            let team = teams
                .iter()
                .flat_map(|t| t.grants.iter())
                .filter(|(x, _)| *x == p)
                .map(|(_, g)| g.platform_role)
                .max();
            match (own, team) {
                (Some(o), Some(t)) => Some((p, o.max(t), AccessVia::Both)),
                (Some(o), None) => Some((p, o, AccessVia::Own)),
                (None, Some(t)) => Some((p, t, AccessVia::Team)),
                (None, None) => None,
            }
        })
        .collect()
}

/// T9-I1: the `access` of a member whose every grant and team a write just made (`rows` and
/// `teams`: an invite, an addition, an approval), as the caller sees it (`visible`, read before
/// the write). Nothing is read after the commit, so no failed read can turn a committed write
/// into a 500.
fn written_access(
    m: &MemberRecord,
    visible: &HashSet<Uuid>,
    rows: &[(Uuid, GrantRow)],
    teams: &[TeamSeen],
) -> MemberAccess {
    let all = HashMap::from([(m.id, written_levels(rows, teams))]);
    member_access(m, visible, &all)
}

/// The member as the API shows it, with its open invite.
async fn member_view<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    m: MemberRecord,
) -> Result<Member, ApiError>
where
    M: MetadataPort,
{
    let listing = state
        .metadata
        .member_listing(m.id)
        .await
        .map_err(|err| internal("member listing", err))?
        .ok_or_else(|| ApiError::not_found("member not found"))?;
    let invite = if listing.status == MemberStatus::Invited {
        state
            .metadata
            .open_invites(listing.tenant_id)
            .await
            .map_err(|err| internal("open invites", err))?
            .into_iter()
            .find(|i| i.member_id == listing.id)
            .map(|i| Invite {
                id: i.id,
                expires_at: i.expires_at,
            })
    } else {
        None
    };
    let access = access_for(state, principal, &listing.member).await?;
    Ok(Member::new(listing, invite, access))
}

/// P10/W18: an access write committed; realtime re-checks `member_sub`'s channels (the whole
/// tenant's for `None`, S28) within a second.
fn access_changed<M, J>(state: &AppState<M, J>, tenant: TenantId, member_sub: Option<&str>) {
    crate::internal::realtime::access_changed(&state.rt_events, tenant, member_sub);
}

/// Every access write's side effects once it committed (members hub P4, MH-5), in this order:
/// 1. realtime re-checks each member's channels ([`access_changed`]);
/// 2. the proxies forget every personal-login row of each member ([`invalidate_member_logins`];
///    the metadata write already took the verifier of any login it revoked, so a lookup the
///    proxy makes now is refused even when this message is lost);
/// 3. every branch of `projects` reconciles its member roles ([`reconcile_projects`]).
///
/// `members` are `(member id, Zitadel sub)` pairs. Grant, override, tenant-admin, remove,
/// suspend, reinstate and revoke-now handlers call it; step 2's team writes will too.
pub(crate) async fn access_changed_for<M, J>(
    state: &AppState<M, J>,
    tenant: TenantId,
    members: &[(Uuid, String)],
    projects: Vec<Uuid>,
) where
    M: MetadataPort,
    J: JobPort,
{
    for (_, sub) in members {
        access_changed(state, tenant, Some(sub));
    }
    for (member, _) in members {
        invalidate_member_logins(state, *member).await;
    }
    reconcile_projects(state, projects).await;
}

/// Tells the proxies to forget every personal-login row of `member`, revoked records' rows
/// included (P4). A failed read is logged: the verifiers are already gone from the metadata.
pub(crate) async fn invalidate_member_logins<M, J>(state: &AppState<M, J>, member: Uuid)
where
    M: MetadataPort,
{
    match state.metadata.member_login_endpoints(member).await {
        Ok(rows) => publish_login_rows(state, rows),
        Err(err) => {
            warn!(member_id = %member, "personal logins: the proxy cache wasn't told, their rows aren't readable: {err}")
        }
    }
}

/// `password_updated` for each row, grouped by endpoint, now and again 1 s later (spawned:
/// the answer doesn't wait for Valkey).
pub(crate) fn publish_login_rows<M, J>(state: &AppState<M, J>, rows: Vec<LoginEndpoint>) {
    crate::internal::publish_login_rows(&state.proxy_cache, rows);
}

/// Asks every branch of these projects to reconcile its member roles. Failures are logged:
/// the worker's sweep reconciles every branch within minutes anyway.
async fn reconcile_projects<M, J>(state: &AppState<M, J>, projects: Vec<Uuid>)
where
    M: MetadataPort,
    J: JobPort,
{
    for project in projects {
        let branches = match state.metadata.project_branches(project).await {
            Ok(branches) => branches,
            Err(err) => {
                warn!(%project, "member roles: listing the project's branches failed; the sweep will reconcile them: {err}");
                continue;
            }
        };
        for branch in branches {
            if let Err(err) = state.js.enqueue_member_roles(BranchId(branch)).await {
                warn!(branch_id = %branch, "member roles: enqueue failed; the sweep will reconcile it: {err}");
            }
        }
    }
}

/// [`reconcile_projects`] for every project of the tenant (a tenant admin's roles cover them).
async fn reconcile_tenant<M, J>(state: &AppState<M, J>, tenant: TenantId)
where
    M: MetadataPort,
    J: JobPort,
{
    match state.metadata.list_projects(tenant).await {
        Ok(projects) => {
            let ids: Vec<Uuid> = projects.into_iter().map(|p| p.id.as_uuid()).collect();
            reconcile_projects(state, ids).await
        }
        Err(err) => {
            warn!(%tenant, "member roles: listing the tenant's projects failed; the sweep will reconcile them: {err}")
        }
    }
}

/// A live branch of a database with its roles (the refusal of a grant names the branch it found).
struct BranchGraph {
    id: Uuid,
    name: String,
    main: bool,
    graph: RoleGraph,
}

/// One Chelabase database's Postgres side: its owner's name and each live branch's roles as
/// 6a records them.
struct DbRoles {
    id: Uuid,
    name: String,
    owner: String,
    graphs: Vec<BranchGraph>,
}

/// The Postgres side grant validation reads, per database of the project (or of the one
/// database an override names).
///
/// The invariant it keeps (T7 review, MH-4): for every member and every Chelabase database,
/// the effective access (the member's own grant ∪ their teams' grants, narrowed by that
/// database's override) passes [`RoleContext::check_effective`]. Every write that changes a
/// grant, an override, a team's grant or a team's members checks the state it would leave for
/// every member it reaches, and is refused (422, naming the database) before anything is
/// written when that state breaks it.
pub(crate) struct RoleContext {
    dbs: Vec<DbRoles>,
    /// MH-21, S12: personal-login names no grant or override may hold (any status): every
    /// database's of the project for a grant, the one database's for an override.
    personal: BTreeSet<String>,
}

impl RoleContext {
    pub(crate) async fn of<M, J>(
        state: &AppState<M, J>,
        databases: &[DatabaseRecord],
    ) -> Result<Self, ApiError>
    where
        M: MetadataPort,
    {
        let mut dbs = Vec::new();
        for db in databases {
            let owner = roles::owner_name(db).to_string();
            let branches = state
                .metadata
                .list_branches(db.database_id)
                .await
                .map_err(|err| internal("grant check: list branches", err))?;
            let mut graphs = Vec::new();
            for branch in branches {
                if matches!(
                    branch.status,
                    BranchStatus::Deleting | BranchStatus::Deleted
                ) {
                    continue;
                }
                // A branch with no endpoint (being created, a backup) has no role records to
                // read; its catalog is checked by the reconciler, which excludes any chain
                // reaching an owner-level, LOGIN or reserved role whenever it runs there.
                let Some(endpoint) = branch.endpoint_id else {
                    continue;
                };
                let rows = state
                    .metadata
                    .list_roles(&endpoint)
                    .await
                    .map_err(|err| internal("grant check: list roles", err))?;
                graphs.push(BranchGraph {
                    id: branch.id.as_uuid(),
                    name: branch.name.clone(),
                    main: branch.parent_branch_id.is_none(),
                    graph: RoleGraph::new(&owner, rows),
                });
            }
            dbs.push(DbRoles {
                id: db.database_id.as_uuid(),
                name: db.name.clone(),
                owner,
                graphs,
            });
        }
        Ok(Self { dbs, personal: BTreeSet::new() })
    }

    /// The context of an override on one database, with the personal logins among `names`
    /// (S12: `is_personal_login_name` on that database).
    async fn database<M, J>(
        state: &AppState<M, J>,
        database: &DatabaseRecord,
        names: &[String],
    ) -> Result<Self, ApiError>
    where
        M: MetadataPort,
    {
        let mut context = Self::of(state, std::slice::from_ref(database)).await?;
        for name in names {
            let personal = state
                .metadata
                .is_personal_login_name(database.database_id.as_uuid(), name)
                .await
                .map_err(|err| internal("grant check: personal logins", err))?;
            if personal {
                context.personal.insert(name.clone());
            }
        }
        Ok(context)
    }

    /// Every live database of the project.
    pub(crate) async fn project<M, J>(state: &AppState<M, J>, project: ProjectId) -> Result<Self, ApiError>
    where
        M: MetadataPort,
    {
        let ids = state
            .metadata
            .project_databases(project)
            .await
            .map_err(|err| internal("grant check: project databases", err))?;
        let mut databases = Vec::new();
        for (id, status, _) in ids {
            if matches!(status, DatabaseStatus::Deleting | DatabaseStatus::Deleted) {
                continue;
            }
            if let Some(db) = state
                .metadata
                .get_database(id)
                .await
                .map_err(|err| internal("grant check: load database", err))?
            {
                databases.push(db);
            }
        }
        let mut context = Self::of(state, &databases).await?;
        // S12: a project grant's roles apply on every database of the project, so a login of
        // any of them (deleted ones' too: their names are cheap to refuse) is refused.
        context.personal = state
            .metadata
            .personal_login_names(project.as_uuid())
            .await
            .map_err(|err| internal("grant check: personal logins", err))?;
        Ok(context)
    }

    /// R7: names no grant or override may hold, at any level: `owner` and the databases'
    /// owners (the admin platform role includes them), `cb_m_*`, `chelabase_*`, `pg_*` and the
    /// platform's own roles, and anything that isn't a Postgres role name.
    pub(crate) fn check_names(&self, pg_roles: &[String]) -> Result<(), ApiError> {
        for name in pg_roles {
            let refused = |why: &str| {
                unprocessable(format!("`{name}` can't be granted: {why}"))
                    .with_extra("role", json!(name))
            };
            if check_any_role_name(name).is_err() {
                return Err(refused("it isn't a Postgres role name"));
            }
            let owner_of = self.dbs.iter().find(|d| &d.owner == name);
            if name.eq_ignore_ascii_case("owner") || owner_of.is_some() {
                let err = refused("it is the database owner, which the admin platform role includes");
                // The database whose owner it is, so the refusal can be shown beside it.
                return Err(match owner_of {
                    Some(d) => err.with_extra("database_id", json!(d.id)),
                    None => err,
                });
            }
            if act_as_reserved(name) {
                return Err(refused(
                    "the name is reserved for Chelabase and the platform",
                ));
            }
            // MH-21: a personal login is one member's own identity, never a granted role.
            if self.personal.contains(name) {
                return Err(refused("it is a member's personal login"));
            }
        }
        Ok(())
    }

    /// The invariant (R16, R21–R23, MH-4) for the state `after` a write would leave: on each
    /// database (only `only`, when given), the member's effective access there
    /// (`after.sources(Some(db)).effective()`: the own grant ∪ the team grants, each role at the
    /// best level of a source naming it, narrowed by the override) holds below Admin only roles
    /// whose whole chain on every branch is plain NOLOGIN groups. Nothing is checked without
    /// access or at Admin. The first break is 422 `invalid_request` naming the role, its chain
    /// and the database. A chain holding ADMIN is refused here too (R30 as changed by I2: 6a
    /// records each membership's SET and ADMIN options).
    pub(crate) fn check_effective(&self, after: &Observed, only: Option<Uuid>) -> Result<(), ApiError> {
        for db in self.dbs.iter().filter(|d| only.is_none_or(|o| o == d.id)) {
            let Some(effective) = after.sources(Some(db.id)).effective() else {
                continue;
            };
            let role = effective.platform_role;
            if role == PlatformRole::Admin {
                continue;
            }
            // P29: below Admin the set is `Only`; the owner is never held there.
            let held: Vec<&str> = match &effective.roles {
                RoleSet::Only(set) => set
                    .iter()
                    .filter_map(|r| match r {
                        PgRole::Named(name) => Some(name.as_str()),
                        PgRole::Owner => None,
                    })
                    .collect(),
                RoleSet::All { .. } => Vec::new(),
            };
            for name in held {
                if let Some((branch, (chain, why))) =
                    db.graphs.iter().find_map(|b| b.graph.refusal(name).map(|r| (b, r)))
                {
                    let last = chain.last().cloned().unwrap_or_default();
                    let path = chain.join(" → ");
                    let reason = if chain.len() > 1 {
                        format!("through {path} it reaches `{last}`, which is {}", why.as_str())
                    } else {
                        format!("`{last}` is {}", why.as_str())
                    };
                    return Err(unprocessable(format!(
                        "`{name}` can be held only with the admin platform role: on database `{}` the member would be {} with it, and {reason}",
                        db.name,
                        role.as_str(),
                    ))
                    .with_extra("role", json!(name))
                    .with_extra("why", json!(why.code()))
                    .with_extra("chain", json!(chain))
                    .with_extra("database_id", json!(db.id))
                    // The branch whose roles broke it (M16): a role can be fine on main and not
                    // on a branch, which the picker (it reads each main) can't see.
                    .with_extra("branch_id", json!(branch.id))
                    .with_extra("branch", json!(branch.name))
                    .with_extra("branch_main", json!(branch.main)));
                }
            }
        }
        Ok(())
    }
}

/// What a member's access on a project is now: their grant and their overrides on its
/// databases (I4). A checked write passes it to the metadata, which writes only while it still
/// holds.
async fn observe<M, J>(
    state: &AppState<M, J>,
    project: Uuid,
    member: Uuid,
) -> Result<Observed, ApiError>
where
    M: MetadataPort,
{
    Ok(state
        .metadata
        .project_access(project)
        .await
        .map_err(|err| internal("load access", err))?
        .observed(project, member))
}

/// An invite's grant, its project's role context loaded.
struct CheckedGrant {
    project: Uuid,
    grant: GrantRow,
    context: RoleContext,
}

/// The most teams an invite or an approval attaches (P12), once duplicates are dropped.
const INVITE_TEAMS_MAX: usize = 50;

/// An invite's or approval's `team_ids` (P12): duplicates dropped (the first stays), at most
/// [`INVITE_TEAMS_MAX`], each a team of the tenant (else 422 naming it in `team_id`). Read
/// before anything is made, so a refused id leaves no Zitadel user behind.
async fn invite_team_ids<M, J>(
    state: &AppState<M, J>,
    tenant: TenantId,
    ids: Vec<Uuid>,
) -> Result<Vec<Uuid>, ApiError>
where
    M: MetadataPort,
{
    let mut seen = HashSet::new();
    let ids: Vec<Uuid> = ids.into_iter().filter(|id| seen.insert(*id)).collect();
    if ids.len() > INVITE_TEAMS_MAX {
        return Err(unprocessable(format!("at most {INVITE_TEAMS_MAX} teams")));
    }
    teams_seen(state, tenant, &ids).await?;
    Ok(ids)
}

/// Each team as the check reads it (P25): its grants on non-deleted projects. A team that
/// isn't (or is no longer) one of the tenant's is 422 naming it.
async fn teams_seen<M, J>(
    state: &AppState<M, J>,
    tenant: TenantId,
    ids: &[Uuid],
) -> Result<Vec<TeamSeen>, ApiError>
where
    M: MetadataPort,
{
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        match state.metadata.team(tenant, *id).await {
            Ok(Some(t)) => out.push(TeamSeen { team: *id, grants: t.grants }),
            Ok(None) => {
                return Err(unprocessable("no such team in the tenant").with_extra("team_id", json!(id)));
            }
            Err(err) => return Err(internal("invite: load team", err)),
        }
    }
    Ok(out)
}

/// The projects an invite's grants and teams reach, by id (P25: the reconcile's and the
/// check's set).
fn invite_projects(grants: &[(Uuid, GrantRow)], teams: &[TeamSeen]) -> Vec<Uuid> {
    let mut out: Vec<Uuid> = grants
        .iter()
        .map(|(p, _)| *p)
        .chain(teams.iter().flat_map(|t| t.grants.iter().map(|(p, _)| *p)))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The invariant for an invite's grants and teams together, on the person's union (P7, P12):
/// per project of either, the member's access (none for a new person) with the invite's grant
/// and every team's grant there, checked once as written; a refusal names the project, and the
/// team whose grant is why when it is a team's ([`refusal_team`]). Returns what each check read (one per project, empty for a new
/// person), to pass to the write as its `expect`.
async fn check_invite<M, J>(
    state: &AppState<M, J>,
    member: Option<Uuid>,
    grants: &[CheckedGrant],
    teams: &[TeamSeen],
) -> Result<Vec<Observed>, ApiError>
where
    M: MetadataPort,
{
    let mut contexts: HashMap<Uuid, RoleContext> = HashMap::new();
    let mut observed = Vec::new();
    for project in invite_projects(&grant_rows(grants), teams) {
        let on_project = |err: ApiError| err.with_extra("project_id", json!(project));
        let now = match member {
            Some(member) => {
                let o = observe(state, project, member).await?;
                observed.push(o.clone());
                o
            }
            None => Observed { project, ..Observed::default() },
        };
        let granted = grants.iter().find(|g| g.project == project);
        let context = match granted {
            Some(g) => &g.context,
            None => {
                match contexts.entry(project) {
                    std::collections::hash_map::Entry::Occupied(e) => &*e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        &*e.insert(RoleContext::project(state, ProjectId::from(project)).await?)
                    }
                }
            }
        };
        // Defect 3: only the union written is checked (a team's Admin may make an own grant's
        // role fine, R17), never the steps towards it.
        let mut base = now;
        if let Some(g) = granted {
            base = base.with_own(Some(g.grant.clone()));
        }
        let joined: Vec<(Uuid, &GrantRow)> = teams
            .iter()
            .filter_map(|t| t.grants.iter().find(|(p, _)| *p == project).map(|(_, g)| (t.team, g)))
            .collect();
        let after = joined
            .iter()
            .fold(base.clone(), |after, (team, g)| after.with_team(*team, Some((*g).clone())));
        if let Err(err) = context.check_effective(&after, None) {
            // The grant's project, so a form with several grants can show the refusal on the
            // right one; and the team whose grant is why, when it is a team's.
            return Err(match refusal_team(context, &base, &joined, &err) {
                Some(team) => on_project(err).with_extra("team_id", json!(team)),
                None => on_project(err),
            });
        }
    }
    Ok(observed)
}

/// The team an invite's refused union is attributed to. None when the state without the
/// invite's teams (`base`: the member's access with the invite's own grant) is refused already:
/// the own grant is why. Otherwise a team is: the first whose grant alone with `base` is
/// refused, else (only their union breaks it) the first naming the refused role, else the first.
fn refusal_team(context: &RoleContext, base: &Observed, joined: &[(Uuid, &GrantRow)], err: &ApiError) -> Option<Uuid> {
    if context.check_effective(base, None).is_err() {
        return None;
    }
    let alone = joined
        .iter()
        .find(|(team, g)| context.check_effective(&base.with_team(*team, Some((*g).clone())), None).is_err());
    let role = err.extra.iter().find(|(k, _)| k == "role").and_then(|(_, v)| v.as_str());
    let naming = || joined.iter().find(|(_, g)| role.is_some_and(|r| g.pg_roles.iter().any(|n| n == r)));
    alone.or_else(naming).or(joined.first()).map(|(team, _)| *team)
}

/// A demotion's check (R17): everything the member keeps (`kept`, one per project they reach)
/// is checked on every database of its project, as the union it is once they are no longer
/// Admin everywhere. A refusal names the project, and the team whose grant is why when it is a
/// team's ([`refusal_team`]).
async fn check_demotion<M, J>(state: &AppState<M, J>, kept: &[Observed]) -> Result<(), ApiError>
where
    M: MetadataPort,
{
    for now in kept {
        let context = RoleContext::project(state, ProjectId::from(now.project)).await?;
        if let Err(err) = context.check_effective(now, None) {
            let own = Observed { teams: BTreeMap::new(), ..now.clone() };
            let teams: Vec<(Uuid, &GrantRow)> = now.teams.iter().map(|(t, g)| (*t, g)).collect();
            let err = match refusal_team(&context, &own, &teams, &err) {
                Some(team) => err.with_extra("team_id", json!(team)),
                None => err,
            };
            return Err(err.with_extra("project_id", json!(now.project)));
        }
    }
    Ok(())
}

fn grant_rows(grants: &[CheckedGrant]) -> Vec<(Uuid, GrantRow)> {
    grants
        .iter()
        .map(|g| (g.project, g.grant.clone()))
        .collect()
}

/// An invite's email: trimmed, lowercased, one `@` with text on both sides, no spaces, 200
/// characters at most (Zitadel's limit).
fn invite_email(raw: &str) -> Result<String, ApiError> {
    let email = chelabase_zitadel::rules::normalize_email(raw);
    let shaped = email.len() <= 200
        && !email.chars().any(char::is_whitespace)
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
        });
    if shaped {
        Ok(email)
    } else {
        Err(unprocessable("email isn't an email address"))
    }
}

/// An invite's grants, checked as far as the person doesn't matter: each project at most once,
/// active in the caller's tenant, with names [`RoleContext::check_names`] allows. The invariant
/// ([`check_invite_grants`]) is checked once the person is known.
async fn invite_grants<M, J>(
    state: &AppState<M, J>,
    tenant: TenantId,
    grants: Vec<InviteGrant>,
) -> Result<Vec<CheckedGrant>, ApiError>
where
    M: MetadataPort,
{
    let mut seen = HashSet::new();
    if !grants.iter().all(|g| seen.insert(g.project_id)) {
        return Err(unprocessable("a project is named in more than one grant"));
    }
    let mut out = Vec::with_capacity(grants.len());
    for g in grants {
        // Every refusal names the grant's project (`extra.project_id`).
        let on_project = |err: ApiError| err.with_extra("project_id", json!(g.project_id));
        match state.metadata.get_project(tenant, g.project_id).await {
            Ok(Some(p)) if p.status == chelabase_metadata::ProjectStatus::Active => {}
            Ok(Some(_)) => {
                return Err(on_project(ApiError::new(
                    409,
                    ErrorCode::ProjectDeleting,
                    "project is being deleted",
                )));
            }
            Ok(None) => return Err(on_project(ApiError::not_found("project not found"))),
            Err(err) => return Err(internal("invite: load project", err)),
        }
        let context = RoleContext::project(state, g.project_id).await?;
        // As a project grant: names no grant may hold are refused, then an Admin grant is every
        // role (R17).
        context.check_names(&g.pg_roles).map_err(on_project)?;
        let grant = GrantRow::new(g.platform_role, &g.pg_roles);
        out.push(CheckedGrant {
            project: g.project_id.as_uuid(),
            grant,
            context,
        });
    }
    Ok(out)
}

/// The invite link Zitadel mails: Studio's accept page with the user id, the code and the
/// invite's own token (`t`, which lets the page describe the invite before the code is spent).
fn invite_template<M, J>(state: &AppState<M, J>, token: &InviteToken) -> String {
    format!(
        "{}/accept-invite?user={{{{.UserID}}}}&code={{{{.Code}}}}&t={}",
        state.studio_url.trim_end_matches('/'),
        token.secret
    )
}

/// The link a password-reset mail carries for an invite (a person who already has a way to sign
/// in, whom Zitadel gives no invite code): the invite's link, marked `reset=1` so accept-invite
/// sets the password with the code.
fn reset_template<M, J>(state: &AppState<M, J>, token: &InviteToken) -> String {
    format!("{}&reset=1", invite_template(state, token))
}

/// Whether Zitadel refuses `user` an invite code because they already have a way to sign in (a
/// password, also one set at creation in the Initial state, a passkey, an OTP or U2F factor, an
/// IdP link): Zitadel answers "User is already initialized". Such a person is mailed a
/// password-reset code instead, which Zitadel makes for any user, a deactivated one included.
async fn needs_reset_code(z: &Zitadel, user: &ZUser) -> Result<bool, ZError> {
    if user.has_password {
        return Ok(true);
    }
    z.has_auth_methods(&user.id).await
}

/// Mails `user` the invite's code: an invite code, or a password-reset code (`reset`) for a
/// person Zitadel gives no invite code.
async fn mail_code<M, J>(
    state: &AppState<M, J>,
    z: &Zitadel,
    user: &str,
    reset: bool,
    token: &InviteToken,
) -> Result<(), ZError> {
    if reset {
        z.send_password_reset(user, &reset_template(state, token)).await
    } else {
        z.create_invite_code(user, &invite_template(state, token)).await
    }
}

/// The link `Delivery::Link` hands the admin: what the mail's template would give, with the
/// user and the code filled in. The code is spent on first use; Chelabase never stores or logs it.
fn invite_link<M, J>(state: &AppState<M, J>, user: &str, code: &str, token: &InviteToken) -> String {
    format!(
        "{}/accept-invite?user={user}&code={code}&t={}",
        state.studio_url.trim_end_matches('/'),
        token.secret
    )
}

/// The response of a call that may carry an invite link: nothing may keep it.
fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
    response
}

/// A fresh invite token: 32 random bytes, base64url, of which only the SHA-256 is stored. Minted
/// per send, so a resend replaces the earlier link's token.
struct InviteToken {
    secret: String,
    hash: [u8; 32],
}

impl InviteToken {
    fn mint() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("OS random number generator");
        let secret = URL_SAFE_NO_PAD.encode(bytes);
        let hash = Sha256::digest(secret.as_bytes()).into();
        Self { secret, hash }
    }
}

/// 409 `link_not_allowed`: a link invite for a user who has a way to sign in. It says whether
/// email delivery works now, so Studio can offer it (a Zitadel that can't be asked counts as no).
async fn link_refused(z: &Zitadel) -> ApiError {
    let email_available = z.email_provider_active().await.unwrap_or(false);
    ApiError::new(
        409,
        ErrorCode::LinkNotAllowed,
        "this person already has a way to sign in: send the invite by email (delivery: \"email\")",
    )
    .with_extra("email_available", json!(email_available))
}

/// 409 `invite_closed`: revoked, accepted or replaced since.
fn invite_closed() -> ApiError {
    ApiError::new(409, ErrorCode::InviteClosed, "the invite is no longer open")
}

fn invite_expiry() -> DateTime<Utc> {
    Utc::now() + chrono::Duration::hours(INVITE_LIFETIME_HOURS)
}

fn admin_zitadel<M, J>(state: &AppState<M, J>) -> Result<&Zitadel, ApiError> {
    state.zitadel.as_ref().ok_or_else(zitadel_unavailable)
}

/// The invite itself (spec part 5). The person is matched by Zitadel user id first, then by a
/// member's non-NULL email (R8, R18):
/// 1. a live member already: they get the grants (all or none);
/// 2. an Active user of the tenant's Zitadel org who has a password and was never a member: an
///    active member with the grants, no mail;
/// 3. anyone else (R29): an `invited` member with its (inert) grants and an invite, then
///    Zitadel mails a code. That covers a removed member and an org user who is deactivated,
///    in another state, or never set a password (a deactivated user stays so until
///    accept-invite verifies the code, I1), and a new person (a passwordless Zitadel user is
///    made).
///
/// The grants keep the invariant ([`RoleContext::check_effective`]) before anything is written.
async fn invite<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    email: String,
    grants: Vec<CheckedGrant>,
    teams: Vec<Uuid>,
    delivery: Delivery,
) -> Result<(StatusCode, InviteResult), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let actor = actor(principal.member.as_ref())?;
    let tenant = principal.tenant_id;
    let found = match &state.zitadel {
        Some(z) => Some(
            z.find_user_in_org(&principal.org_id, &email)
                .await
                .map_err(|err| zitadel_failed("invite: find user", &err))?,
        ),
        None => None,
    };
    let existing = match found {
        Some(EmailMatch::Many) => {
            return Err(ApiError::new(
                409,
                ErrorCode::EmailAmbiguous,
                "more than one user of the organisation has this email; resolve it in Zitadel first",
            ));
        }
        Some(EmailMatch::One(user)) => {
            match state.metadata.member_by_zitadel(tenant, &user.id).await {
                Ok(Some(m)) if m.status != MemberStatus::Removed => m,
                Ok(row) => {
                    let usable = user.state == UserState::Active && user.has_password;
                    return if usable && row.is_none() {
                        add_user(state, principal, actor, user, &email, &grants, &teams).await
                    } else {
                        invite_with_code(state, principal, actor, &email, &grants, &teams, Some(user), delivery).await
                    };
                }
                Err(err) => return Err(internal("invite: member by user", err)),
            }
        }
        Some(EmailMatch::None) | None => {
            match state.metadata.member_by_email(tenant, &email).await {
                Ok(Some(m)) => m,
                Ok(None) => {
                    return invite_with_code(state, principal, actor, &email, &grants, &teams, None, delivery).await;
                }
                Err(err) => return Err(internal("invite: member by email", err)),
            }
        }
    };
    // A sign-up waiting for approval is answered by approving it, with the grants chosen there.
    if existing.status == MemberStatus::Pending {
        return Err(ApiError::new(
            409,
            ErrorCode::SignupPending,
            "this person's sign-up is waiting for approval: approve or decline it instead",
        )
        .with_extra("member_id", json!(existing.id)));
    }
    // T9-I1: what the answer needs is read before the write, so a failed read after it can't
    // turn the committed grants into a 500 (the answer is then built from these).
    let visible = visible_projects(state, principal).await?;
    let mut all = grants_by_member(state, principal.tenant_id).await?;
    let listing_before = state
        .metadata
        .member_listing(existing.id)
        .await
        .map_err(|err| internal("member listing", err))?
        .ok_or_else(|| ApiError::not_found("member not found"))?;
    // 1. A member already: the grants and the teams checked on their union, then all put in
    // one transaction (P25: a team that changed since is `Changed`, checked again).
    let mut tries = 0;
    let mut seen;
    let changed = loop {
        tries += 1;
        seen = teams_seen(state, principal.tenant_id, &teams).await?;
        let observed = check_invite(state, Some(existing.id), &grants, &seen).await?;
        match state
            .metadata
            .put_project_grants(actor, existing.id, &grant_rows(&grants), &seen, &observed)
            .await
        {
            // I4: their access moved between the check and the write: check again.
            Err(AccessWriteError::Changed) if tries < CHECKED_WRITE_TRIES => continue,
            other => break other.map_err(|err| write_failed("invite: put grants", err, "project not found"))?,
        }
    };
    info!(request_id = %principal.request_id, member_id = %existing.id, changed = changed.len(), "invite: granted to an existing member");
    if !changed.is_empty() {
        // A grant replaced by a lower one takes access away like a grant put (P4).
        let who = [(existing.id, existing.zitadel_user_id.clone())];
        access_changed_for(state, principal.tenant_id, &who, changed).await;
    }
    let member = match member_view(state, principal, existing.clone()).await {
        Ok(member) => member,
        Err(err) => {
            warn!(member_id = %existing.id, "invite: the member isn't readable after its grants were put; answered from the reads before: {err:?}");
            let held = all.entry(existing.id).or_default();
            let written = written_levels(&grant_rows(&grants), &seen);
            held.retain(|(p, _, _)| !written.iter().any(|(w, _, _)| w == p));
            held.extend(written);
            let access = member_access(&listing_before.member, &visible, &all);
            Member::new(MemberListing { has_access: true, ..listing_before }, None, access)
        }
    };
    Ok((
        StatusCode::OK,
        InviteResult {
            outcome: InviteOutcome::ExistingMember,
            member,
            invite_url: None,
        },
    ))
}

/// Invite step 2: an Active org user with a password, never a member, becomes an active member.
async fn add_user<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    actor: Uuid,
    user: chelabase_zitadel::client::ZUser,
    email: &str,
    checked: &[CheckedGrant],
    teams: &[Uuid],
) -> Result<(StatusCode, InviteResult), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let email = Some(user.email.as_str())
        .filter(|e| !e.trim().is_empty())
        .unwrap_or(email);
    let grants = grant_rows(checked);
    // T9-I1: what the caller sees is read before the write.
    let visible = visible_projects(state, principal).await?;
    // P25: a team changed between the check and the write is checked again.
    let mut tries = 0;
    let (listing, seen) = loop {
        tries += 1;
        let seen = teams_seen(state, principal.tenant_id, teams).await?;
        check_invite(state, None, checked, &seen).await?;
        match state
            .metadata
            .add_existing_user(actor, principal.tenant_id, &user.id, email, &user.display_name, &grants, &seen)
            .await
        {
            Err(AccessWriteError::Changed) if tries < CHECKED_WRITE_TRIES => continue,
            other => {
                break (
                    other.map_err(|err| write_failed("invite: add user", err, "project not found"))?,
                    seen,
                );
            }
        }
    };
    info!(request_id = %principal.request_id, member_id = %listing.id, "invite: a user of the organisation was added");
    reconcile_projects(state, invite_projects(&grants, &seen)).await;
    let access = written_access(&listing.member, &visible, &grants, &seen);
    Ok((
        StatusCode::CREATED,
        InviteResult {
            outcome: InviteOutcome::Added,
            member: Member::new(listing, None, access),
            invite_url: None,
        },
    ))
}

/// Invite step 3 (R29): an `invited` member and a code (mailed, or with `Delivery::Link` handed
/// back to the admin inside the link, R35), for `existing` (an org user who
/// may not be added straight away: reactivated first when deactivated) or a new passwordless
/// user. Each step undoes the earlier ones when it fails, so a failed invite leaves no new
/// user, member or mail behind.
#[allow(clippy::too_many_arguments)]
async fn invite_with_code<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    actor: Uuid,
    email: &str,
    checked: &[CheckedGrant],
    teams: &[Uuid],
    existing: Option<chelabase_zitadel::client::ZUser>,
    delivery: Delivery,
) -> Result<(StatusCode, InviteResult), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let grants = grant_rows(checked);
    // P7, P12: the grants and teams are checked on the new member's union before any user is
    // made; P25: a team that changes before the write is read and checked again there.
    let mut seen = teams_seen(state, principal.tenant_id, teams).await?;
    check_invite(state, None, checked, &seen).await?;
    let z = admin_zitadel(state)?;
    // T9-I1: what the caller sees is read before anything is made (a failed read here leaves
    // nothing behind).
    let visible = visible_projects(state, principal).await?;
    let created = existing.is_none();
    // A link carries the code and nothing else: it is for a user who is made now, or one who has
    // never had any way to sign in (password, passkey, OTP or U2F, IdP link). Anyone else is told
    // by email, which only they can read.
    if delivery == Delivery::Link
        && let Some(user) = &existing
        && (user.has_password
            || z.has_auth_methods(&user.id).await.map_err(|err| zitadel_failed("invite: read auth methods", &err))?)
    {
        return Err(link_refused(z).await);
    }
    let reset = match &existing {
        Some(user) if delivery == Delivery::Email => needs_reset_code(z, user)
            .await
            .map_err(|err| zitadel_failed("invite: read auth methods", &err))?,
        _ => false,
    };
    let user = match existing {
        // I1: a deactivated user stays deactivated; accept-invite reactivates them once the
        // emailed code is verified, so an invite that expires or is revoked never lets an old
        // password work again.
        Some(user) => user.id,
        None => match z
            .create_user_without_password(&principal.org_id, email)
            .await
        {
            Ok(user) => user,
            Err(err) if err.is(refusals::USER_ALREADY_EXISTS) => {
                return Err(ApiError::new(
                    409,
                    ErrorCode::EmailInUseElsewhere,
                    "a user with this email already exists outside the tenant's organisation",
                ));
            }
            Err(err) => return Err(zitadel_failed("invite: create user", &err)),
        },
    };
    // Undoes what this invite did to the Zitadel user: a new one is deleted, an existing one is
    // left deactivated (as a removal leaves it).
    let undo = |why: &'static str| {
        let user = user.clone();
        async move {
            let result = if created {
                z.delete_user(&user).await
            } else {
                z.deactivate_user_settled(&user).await
            };
            if let Err(err) = result {
                error!(user_id = %user, "invite: undoing the Zitadel user after {why} failed: {err}");
            }
        }
    };
    let expires_at = invite_expiry();
    let token = InviteToken::mint();
    let mut tries = 0;
    let (listing, invite) = loop {
        tries += 1;
        let stored = state
            .metadata
            .create_invited_member(
                actor,
                principal.tenant_id,
                NewInvite {
                    zitadel_user_id: &user,
                    email,
                    expires_at,
                    token_hash: token.hash,
                    grants: &grants,
                    teams: &seen,
                },
            )
            .await;
        match stored {
            Ok(created) => break created,
            Err(AccessWriteError::Changed) if tries < CHECKED_WRITE_TRIES => {
                let again = match teams_seen(state, principal.tenant_id, teams).await {
                    Ok(again) => again,
                    Err(err) => {
                        undo("a team gone").await;
                        return Err(err);
                    }
                };
                if let Err(err) = check_invite(state, None, checked, &again).await {
                    undo("a refused team").await;
                    return Err(err);
                }
                seen = again;
            }
            Err(err) => {
                undo("a refused member").await;
                return Err(write_failed("invite: store", err, "project not found"));
            }
        }
    };
    let sent = match delivery {
        Delivery::Email => mail_code(state, z, &user, reset, &token).await.map(|()| None),
        Delivery::Link => z
            .return_invite_code(&user)
            .await
            .map(|code| Some(invite_link(state, &user, &code, &token))),
    };
    let invite_url = match sent {
        Ok(url) => url,
        Err(err) => {
            if let Err(failed) = state.metadata.revoke_undelivered_invite(actor, invite).await {
                error!(%invite, "invite: revoking after a failed mail failed: {failed}");
            }
            undo("a failed mail").await;
            return Err(zitadel_failed("invite: send code", &err));
        }
    };
    info!(request_id = %principal.request_id, member_id = %listing.id, %invite, ?delivery, "invite: created");
    // P25: the reconcile covers the grants' and the teams' projects.
    reconcile_projects(state, invite_projects(&grants, &seen)).await;
    // Built from what the write returned and the grants it made: nothing is read after the
    // mail (T9-I1: a link invite's URL is never lost to a failed read).
    let access = written_access(&listing.member, &visible, &grants, &seen);
    let member = Member::new(
        listing,
        Some(Invite {
            id: invite,
            expires_at,
        }),
        access,
    );
    Ok((
        StatusCode::CREATED,
        InviteResult {
            outcome: InviteOutcome::Invited,
            member,
            invite_url,
        },
    ))
}

/// After a revoke (I4: metadata first): a never-activated Zitadel user (no password) is
/// deleted; one that once had a password (a removed member invited again, R29) is deactivated
/// instead. One already gone, or already inactive, is fine, so this can be retried.
async fn clean_up_revoked_user(z: &Zitadel, user: &str) -> Result<(), ZError> {
    match z.get_user(user).await? {
        None => Ok(()),
        Some(u) if u.has_password => z.deactivate_user_settled(user).await,
        Some(_) => z.delete_user(user).await,
    }
}

/// An open invite of the caller's tenant: its member, or the answer to send.
async fn open_invite<M, J>(
    state: &AppState<M, J>,
    tenant: TenantId,
    id: Uuid,
) -> Result<MemberRecord, ApiError>
where
    M: MetadataPort,
{
    let not_found = || ApiError::not_found("invite not found");
    let (member, _, revoked, accepted) = match state.metadata.invite(id).await {
        Ok(Some(row)) => row,
        Ok(None) => return Err(not_found()),
        Err(err) => return Err(internal("load invite", err)),
    };
    let m = match state.metadata.member(member).await {
        Ok(Some(m)) if m.tenant_id == tenant => m,
        Ok(_) => return Err(not_found()),
        Err(err) => return Err(internal("load invite member", err)),
    };
    if revoked.is_some() || accepted.is_some() || m.status != MemberStatus::Invited {
        return Err(invite_closed());
    }
    Ok(m)
}

/// The member's grant on the project with its overrides, as the API shows it.
async fn project_grant<M, J>(
    state: &AppState<M, J>,
    project: Uuid,
    member: Uuid,
) -> Result<Option<ProjectGrant>, ApiError>
where
    M: MetadataPort,
{
    let access = state
        .metadata
        .project_access(project)
        .await
        .map_err(|err| internal("project access", err))?;
    Ok(grants_view(access)
        .into_iter()
        .find(|g| g.member_id == member))
}

fn grants_view(access: ProjectAccess) -> Vec<ProjectGrant> {
    let mut overrides: BTreeMap<Uuid, Vec<GrantOverride>> = BTreeMap::new();
    for (member, database, o) in access.overrides {
        overrides.entry(member).or_default().push(GrantOverride {
            database_id: DatabaseId(database),
            platform_role: o.platform_role,
            pg_roles: o.pg_roles,
        });
    }
    access
        .grants
        .into_iter()
        .map(|(member, g)| ProjectGrant {
            member_id: member,
            platform_role: g.platform_role,
            // R17: an Admin grant is every role.
            pg_roles: if g.every_role() { MeRoles::All(AllRoles::All) } else { MeRoles::Only(g.pg_roles) },
            overrides: overrides.remove(&member).unwrap_or_default(),
        })
        .collect()
}

/// `?before&limit&member_id`, checked.
fn events_query(
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Result<(Option<i64>, u32, Option<Uuid>), ApiError> {
    let Query(q) = query.map_err(|rej| {
        ApiError::invalid_request(400, format!("invalid query: {}", rej.body_text()))
    })?;
    let limit = q.limit.unwrap_or(EVENTS_DEFAULT);
    if !(1..=EVENTS_MAX).contains(&limit) {
        return Err(ApiError::invalid_request(
            400,
            format!("limit must be 1 to {EVENTS_MAX}"),
        ));
    }
    Ok((q.before, limit, q.member_id))
}

async fn events_page<M, J>(
    state: &AppState<M, J>,
    tenant: TenantId,
    project: Option<Uuid>,
    member: Option<Uuid>,
    before: Option<i64>,
    limit: u32,
) -> Result<AccessEventPage, ApiError>
where
    M: MetadataPort,
{
    let rows = state
        .metadata
        .access_events(tenant, project, member, before, limit)
        .await
        .map_err(|err| internal("access events", err))?;
    let next_before = (rows.len() == limit as usize)
        .then(|| rows.last().map(|e| e.id))
        .flatten();
    // T9-H1, EOS-5: a project's log shows only the project's part of an event.
    let scope = match project {
        Some(project) => {
            let databases: HashSet<Uuid> = state
                .metadata
                .project_databases(ProjectId::from(project))
                .await
                .map_err(|err| internal("access events: the project's databases", err))?
                .into_iter()
                .map(|(d, _, _)| d.as_uuid())
                .collect();
            let branches: HashSet<Uuid> = state
                .metadata
                .project_branches(project)
                .await
                .map_err(|err| internal("access events: the project's branches", err))?
                .into_iter()
                .collect();
            Some(ProjectScope { project, databases, branches })
        }
        None => None,
    };
    let scoped = |v: Option<Value>| match (&scope, v) {
        (Some(scope), Some(mut v)) => {
            scope.apply(&mut v);
            Some(v)
        }
        (_, v) => v,
    };
    let events = rows
        .into_iter()
        .map(|e| AccessEvent {
            id: e.id,
            actor_member_id: e.actor_member_id,
            action: e.action,
            target_member_id: e.target_member_id,
            project_id: e.project_id,
            database_id: e.database_id,
            branch_id: e.branch_id,
            actor_email: e.actor_email,
            target_email: e.target_email,
            project_name: e.project_name,
            before: scoped(e.before),
            after: scoped(e.after),
            at: e.at,
        })
        .collect();
    Ok(AccessEventPage {
        events,
        next_before,
    })
}

/// T9-H1, EOS-5: what of an event's `before`/`after` a project's log may show: its own grants
/// (`grants`), the overrides and personal logins on its databases (`overrides`, `logins`, whose
/// `count` follows), and the branches of its databases (`rotated_branches`). An entry that
/// doesn't name a value of the project is left out (fails closed).
struct ProjectScope {
    project: Uuid,
    databases: HashSet<Uuid>,
    branches: HashSet<Uuid>,
}

impl ProjectScope {
    fn apply(&self, v: &mut Value) {
        let Some(fields) = v.as_object_mut() else {
            return;
        };
        let id = |item: &Value, key: Option<&str>| {
            let value = match key {
                Some(key) => &item[key],
                None => item,
            };
            value.as_str().and_then(|s| Uuid::parse_str(s).ok())
        };
        let keep = |fields: &mut serde_json::Map<String, Value>, field: &str, ok: &dyn Fn(&Value) -> bool| {
            if let Some(Value::Array(items)) = fields.get_mut(field) {
                items.retain(|item| ok(item));
            }
        };
        keep(fields, "grants", &|g| id(g, Some("project_id")) == Some(self.project));
        keep(fields, "overrides", &|o| {
            id(o, Some("database_id")).is_some_and(|d| self.databases.contains(&d))
        });
        keep(fields, "rotated_branches", &|b| id(b, None).is_some_and(|b| self.branches.contains(&b)));
        // P9: a project Admin sees a team event, not who else the team reaches.
        keep(fields, "affected_member_ids", &|_| false);
        let logins = match fields.get_mut("logins") {
            Some(Value::Array(items)) => {
                items.retain(|l| id(l, Some("database_id")).is_some_and(|d| self.databases.contains(&d)));
                Some(items.len())
            }
            _ => None,
        };
        if let (Some(n), Some(count)) = (logins, fields.get_mut("count")) {
            *count = json!(n);
        }
    }
}

/// Unwraps a handler step, answering its error.
macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(err) => return err.into_response(),
        }
    };
}

// --- Members -----------------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/v1/members",
    operation_id = "listMembers",
    tag = "members",
    summary = "List the tenant's members",
    description = "Invited, pending, active and suspended members, by email: names and emails, for grant pickers. An invited member carries its open invite; a pending one (signed up, waiting for approval) can be approved or declined. Each member carries `access`: the highest platform role over the projects the caller can see and the member's role on each of those projects (`other`: access only on projects the caller can't see; `none`: no project grant). No Postgres roles or overrides.",
    responses(
        (status = 200, description = "The members", body = Vec<Member>),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_members<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize(
            &state,
            &principal,
            ep::LIST_MEMBERS.policy.access,
            Target::None
        )
        .await
    );
    let members = tri!(
        state
            .metadata
            .list_members(principal.tenant_id)
            .await
            .map_err(|err| internal("list members", err))
    );
    let invites = tri!(
        state
            .metadata
            .open_invites(principal.tenant_id)
            .await
            .map_err(|err| internal("open invites", err))
    );
    let visible = tri!(visible_projects(&state, &principal).await);
    let all = tri!(grants_by_member(&state, principal.tenant_id).await);
    let out: Vec<Member> = members
        .into_iter()
        .map(|m| {
            let invite = invites
                .iter()
                .find(|i| i.member_id == m.member.id)
                .map(|i| Invite {
                    id: i.id,
                    expires_at: i.expires_at,
                });
            let access = member_access(&m.member, &visible, &all);
            Member::new(m, invite, access)
        })
        .collect();
    Json(out).into_response()
}

#[utoipa::path(
    patch,
    path = "/v1/members/{member_id}",
    operation_id = "updateMember",
    tag = "members",
    summary = "Make a member a tenant admin, or not",
    description = "A tenant admin manages people and projects and is admin on every project. Demoting one checks what they keep (their own grants, overrides and teams' grants) as any grant is checked. Every branch of the tenant reconciles its member roles.",
    params(("member_id" = Uuid, Path, description = "Member id")),
    request_body = UpdateMemberRequest,
    responses(
        (status = 200, description = "The member", body = Member),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON, or a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such member (another tenant's, or a removed one, answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`last_tenant_admin`: the tenant would have no active tenant admin; `conflict`: only an active member may become a tenant admin, or the member's access changed twice while a demotion was being checked", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: `tenant_admin` is missing or isn't a boolean; or a demotion would leave the member, below Admin, a role they can't hold (`role` names it, with `project_id`, `database_id`, `chain`, `branch_id`, `branch` and `branch_main`, and `team_id` when a team's grant is why)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn update_member<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<UpdateMemberRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize(
            &state,
            &principal,
            ep::UPDATE_MEMBER.policy.access,
            Target::None
        )
        .await
    );
    let actor = tri!(actor(principal.member.as_ref()));
    let m = tri!(live_member(&state, principal.tenant_id, id).await);
    // R9 (an invited member isn't a person in the tenant yet) is checked under the member's
    // lock by `set_tenant_admin`: `conflict`.
    let changed = m.tenant_admin != body.tenant_admin;
    if changed && !body.tenant_admin {
        // R17: no longer Admin everywhere, the member keeps only what their own grants,
        // overrides and teams give, which must pass the union check; the write compares it
        // under the tenant lock (P6, P25), and a change since is checked again, then 409.
        let mut tries = 0;
        loop {
            tries += 1;
            let kept = tri!(
                state
                    .metadata
                    .member_observed(id)
                    .await
                    .map_err(|err| internal("demote: load access", err))
            );
            tri!(check_demotion(&state, &kept).await);
            match state.metadata.set_tenant_admin(Some(actor), id, false, Some(&kept)).await {
                Err(AccessWriteError::Changed) if tries < CHECKED_WRITE_TRIES => continue,
                other => break tri!(other.map_err(|err| write_failed("set tenant admin", err, "member not found"))),
            }
        }
    } else {
        tri!(
            state
                .metadata
                .set_tenant_admin(Some(actor), id, body.tenant_admin, None)
                .await
                .map_err(|err| write_failed("set tenant admin", err, "member not found"))
        );
    }
    if changed {
        info!(request_id = %principal.request_id, member_id = %id, tenant_admin = body.tenant_admin, "member: tenant admin changed");
        access_changed_for(&state, principal.tenant_id, &[(id, m.zitadel_user_id.clone())], Vec::new()).await;
        reconcile_tenant(&state, principal.tenant_id).await;
    }
    let m = tri!(tenant_member(&state, principal.tenant_id, id).await);
    Json(tri!(member_view(&state, &principal, m).await)).into_response()
}

#[utoipa::path(
    delete,
    path = "/v1/members/{member_id}",
    operation_id = "removeMember",
    tag = "members",
    summary = "Remove a member",
    description = "The member's grants, overrides and open invites go, their Postgres member roles are dropped on every branch, and their Zitadel user is deactivated with its sessions ended (one that never finished sign-up can't be deactivated, so it is locked instead); a still-valid token is refused at once. Removing a removed member again retries the Zitadel steps.",
    params(("member_id" = Uuid, Path, description = "Member id")),
    responses(
        (status = 204, description = "Removed"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such member (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`last_tenant_admin`: the tenant would have no active tenant admin", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: removed in Chelabase, but Zitadel couldn't deactivate the user or end their sessions (or isn't configured); remove again to retry", body = ErrorEnvelope),
    ),
)]
async fn remove_member<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize(
            &state,
            &principal,
            ep::REMOVE_MEMBER.policy.access,
            Target::None
        )
        .await
    );
    let actor = tri!(actor(principal.member.as_ref()));
    let m = tri!(tenant_member(&state, principal.tenant_id, id).await);
    let touched = tri!(
        state
            .metadata
            .remove_member(actor, id)
            .await
            .map_err(|err| write_failed("remove member", err, "member not found"))
    );
    info!(request_id = %principal.request_id, member_id = %id, "member: removed");
    access_changed_for(&state, principal.tenant_id, &[(id, m.zitadel_user_id.clone())], touched).await;
    if m.tenant_admin {
        reconcile_tenant(&state, principal.tenant_id).await;
    }
    let z = tri!(admin_zitadel(&state));
    // Already inactive (a retry), already gone, or Initial and locked, is done; any other
    // refusal is 503.
    if let Err(err) = deactivate_for_removal(z, &m.zitadel_user_id).await {
        return zitadel_failed("remove member: deactivate user", &err).into_response();
    }
    if let Err(err) = z.delete_user_sessions(&m.zitadel_user_id).await {
        return zitadel_failed("remove member: end sessions", &err).into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

// --- Sign-ups waiting for approval (R40) ------------------------------------------------------

#[utoipa::path(
    post,
    path = "/v1/members/{member_id}/approve",
    operation_id = "approveMember",
    tag = "members",
    summary = "Approve a sign-up",
    description = "A person who signed up under open sign-up is `pending` and can do nothing (`pending_approval`) until a tenant admin approves them here, with their first grants. The grants are checked exactly as an invite's are; the member becomes `active`, the grants are stored and one `member_approved` event is written in one step.",
    params(("member_id" = Uuid, Path, description = "Member id")),
    request_body = ApproveMemberRequest,
    responses(
        (status = 200, description = "The approved member", body = Member),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON, or a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant; `forbidden`: only a tenant admin may", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such member (another tenant's or a removed one answers the same), or a grant's project doesn't exist in the tenant", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: the member isn't waiting for approval; `project_deleting`: a grant's project is being deleted", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: a project is named twice, or a role can't be granted (`role` names it; `chain`, `database_id`, `branch_id`, `branch` and `branch_main` too when its memberships on a branch are why)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn approve_member<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<ApproveMemberRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize(
            &state,
            &principal,
            ep::APPROVE_MEMBER.policy.access,
            Target::None
        )
        .await
    );
    let actor = tri!(actor(principal.member.as_ref()));
    let m = tri!(tenant_member(&state, principal.tenant_id, id).await);
    if m.status == MemberStatus::Removed {
        return ApiError::not_found("member not found").into_response();
    }
    let grants = tri!(invite_grants(&state, principal.tenant_id, body.grants).await);
    let teams = tri!(invite_team_ids(&state, principal.tenant_id, body.team_ids).await);
    let rows = grant_rows(&grants);
    // T9-I1: what the caller sees is read before the write.
    let visible = tri!(visible_projects(&state, &principal).await);
    // Checked with the member's existing overrides, as any grant on a member is, together with
    // the teams on the member's union (P12); P25: a team changed since is checked again.
    let mut tries = 0;
    let (listing, seen) = loop {
        tries += 1;
        let seen = tri!(teams_seen(&state, principal.tenant_id, &teams).await);
        tri!(check_invite(&state, Some(id), &grants, &seen).await);
        match state.metadata.approve_member(actor, id, &rows, &seen).await {
            Err(AccessWriteError::Changed) if tries < CHECKED_WRITE_TRIES => continue,
            other => {
                break (
                    tri!(other.map_err(|err| write_failed("approve member", err, "project not found"))),
                    seen,
                );
            }
        }
    };
    info!(request_id = %principal.request_id, member_id = %id, grants = rows.len(), teams = seen.len(), "member: approved");
    access_changed_for(&state, principal.tenant_id, &[(id, m.zitadel_user_id.clone())], invite_projects(&rows, &seen)).await;
    let access = written_access(&listing.member, &visible, &rows, &seen);
    Json(Member::new(listing, None, access)).into_response()
}

#[utoipa::path(
    post,
    path = "/v1/members/{member_id}/decline",
    operation_id = "declineMember",
    tag = "members",
    summary = "Decline a sign-up",
    description = "The pending member becomes `removed` (one `member_declined` event), then their Zitadel user is deactivated and its sessions ended (one that never finished sign-up can't be deactivated, so it is locked instead); a still-valid token is refused at once. Declining a removed member again retries the Zitadel steps.",
    params(("member_id" = Uuid, Path, description = "Member id")),
    responses(
        (status = 204, description = "Declined"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant; `forbidden`: only a tenant admin may", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such member (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: the member isn't waiting for approval", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: declined in Chelabase, but Zitadel couldn't deactivate the user or end their sessions (or isn't configured); decline again to retry", body = ErrorEnvelope),
    ),
)]
async fn decline_member<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize(
            &state,
            &principal,
            ep::DECLINE_MEMBER.policy.access,
            Target::None
        )
        .await
    );
    let actor = tri!(actor(principal.member.as_ref()));
    let m = tri!(tenant_member(&state, principal.tenant_id, id).await);
    // A removed member is a retry of the Zitadel steps; anything else must be pending.
    if m.status != MemberStatus::Removed {
        tri!(
            state
                .metadata
                .decline_member(actor, id)
                .await
                .map_err(|err| write_failed("decline member", err, "member not found"))
        );
        info!(request_id = %principal.request_id, member_id = %id, "member: declined");
        access_changed(&state, principal.tenant_id, Some(&m.zitadel_user_id));
    }
    let z = tri!(admin_zitadel(&state));
    if let Err(err) = deactivate_for_removal(z, &m.zitadel_user_id).await {
        return zitadel_failed("decline member: deactivate user", &err).into_response();
    }
    if let Err(err) = z.delete_user_sessions(&m.zitadel_user_id).await {
        return zitadel_failed("decline member: end sessions", &err).into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

// --- Suspension and "Revoke Postgres access now" (members hub D5, P14–P16) ----------------

/// What "Revoke Postgres access now" did.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct RevokeResult {
    /// The member's personal logins revoked (those that weren't already).
    revoked: usize,
    /// The branches whose member role of this member gets a new password (its sessions end).
    rotated_branches: usize,
}

/// 503 after the Chelabase side of a suspension or reinstatement is done: Zitadel didn't lock
/// (or unlock) the user or end its sessions. The same call again completes it (S7).
fn zitadel_step_failed(what: &str, err: &ZError, retry: &str) -> ApiError {
    warn!("{what}: {err}");
    ApiError::new(
        503,
        ErrorCode::Unavailable,
        format!("done in Chelabase, but the user service couldn't finish it; {retry} again to retry"),
    )
}

/// Locks a suspended member's Zitadel user. An inactive user (removed earlier) isn't locked,
/// and Zitadel refuses to lock it (`LOCK_NOT_ACTIVE`, the same answer as for a locked one); left
/// inactive, accept-invite with a reset code would reactivate it. So it is reactivated, then
/// locked: it ends active and locked, and a reinstate's unlock gives back a working sign-in. The
/// caller ends the sessions whatever this returns, so none made in between survives; a failure
/// leaves the user active or inactive, never locked, and the same call again re-runs the steps.
async fn lock_for_suspension(z: &Zitadel, user: &str) -> Result<(), ZError> {
    match z.lock_user(user).await {
        Err(err) if err.is(refusals::LOCK_NOT_ACTIVE) => match z.get_user(user).await? {
            Some(u) if u.state == UserState::Inactive => {
                z.reactivate_user_settled(user).await?;
                z.lock_user(user).await
            }
            _ => Err(err),
        },
        other => other,
    }
}

/// Deactivates a removed (or declined) member's Zitadel user, settled. One in the Initial state
/// can't be deactivated (`DEACTIVATE_INITIAL`, "can only be deleted"); it is locked instead, so
/// it can't finish sign-up, and that is done.
async fn deactivate_for_removal(z: &Zitadel, user: &str) -> Result<(), ZError> {
    match z.deactivate_user_settled(user).await {
        Err(err) if err.is(refusals::DEACTIVATE_INITIAL) => z.lock_user(user).await,
        other => other,
    }
}

#[utoipa::path(
    post,
    path = "/v1/members/{member_id}/suspend",
    operation_id = "suspendMember",
    tag = "members",
    summary = "Suspend a member",
    description = "The member is refused everywhere but `GET /v1/me` (`member_suspended`) and keeps their grants. Their personal Postgres logins are revoked at once (the proxies forget them), their member roles get new passwords on every branch they reach (open sessions end), and their Zitadel user is locked (an inactive one is reactivated first, so it ends locked) with its sessions ended, even when the lock fails. Suspending a suspended member again writes nothing new; the proxies, the member roles and the Zitadel steps are redone.",
    params(("member_id" = Uuid, Path, description = "Member id")),
    responses(
        (status = 200, description = "The suspended member", body = Member),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant; `forbidden`: only a tenant admin may", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such member (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: yourself, or a member who isn't active (invited, pending or removed); `last_tenant_admin`: the tenant would have no active tenant admin", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: suspended in Chelabase, but Zitadel couldn't lock the user or end their sessions (or isn't configured); suspend again to retry", body = ErrorEnvelope),
    ),
)]
async fn suspend_member<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(authorize(&state, &principal, ep::SUSPEND_MEMBER.policy.access, Target::None).await);
    let actor = tri!(actor(principal.member.as_ref()));
    let m = tri!(tenant_member(&state, principal.tenant_id, id).await);
    // P15: before the last-admin check, which only a self-suspension could reach.
    if actor == id {
        return ApiError::new(409, ErrorCode::Conflict, "you can't suspend yourself").into_response();
    }
    let touched = tri!(
        state
            .metadata
            .suspend_member(actor, id)
            .await
            .map_err(|err| write_failed("suspend member", err, "member not found"))
    );
    // T8-3: the side effects are idempotent and run on every call, a retry's (S7) or one that
    // raced another suspension included: `m` was read before the write, so it can't tell which
    // call made it (the metadata writes the event once).
    if m.status == MemberStatus::Active {
        info!(request_id = %principal.request_id, member_id = %id, "member: suspended");
    }
    access_changed_for(&state, principal.tenant_id, &[(id, m.zitadel_user_id.clone())], touched).await;
    if m.tenant_admin {
        reconcile_tenant(&state, principal.tenant_id).await;
    }
    let z = tri!(admin_zitadel(&state));
    // The sessions end even when the lock failed: one made while the user was reactivated
    // doesn't survive the 503.
    let locked = lock_for_suspension(z, &m.zitadel_user_id).await;
    let ended = z.delete_user_sessions(&m.zitadel_user_id).await;
    if let Err(err) = locked {
        return zitadel_step_failed("suspend member: lock user", &err, "suspend").into_response();
    }
    if let Err(err) = ended {
        return zitadel_step_failed("suspend member: end sessions", &err, "suspend").into_response();
    }
    let m = tri!(tenant_member(&state, principal.tenant_id, id).await);
    Json(tri!(member_view(&state, &principal, m).await)).into_response()
}

#[utoipa::path(
    post,
    path = "/v1/members/{member_id}/reinstate",
    operation_id = "reinstateMember",
    tag = "members",
    summary = "Reinstate a suspended member",
    description = "The member is active again with their grants, and their Zitadel user is unlocked. Their personal logins stay revoked: they make new ones. Reinstating an active member again only redoes the unlock.",
    params(("member_id" = Uuid, Path, description = "Member id")),
    responses(
        (status = 200, description = "The reinstated member", body = Member),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant; `forbidden`: only a tenant admin may", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such member (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: the member isn't suspended (invited, pending or removed)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: reinstated in Chelabase, but Zitadel couldn't unlock the user (or isn't configured); reinstate again to retry", body = ErrorEnvelope),
    ),
)]
async fn reinstate_member<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(authorize(&state, &principal, ep::REINSTATE_MEMBER.policy.access, Target::None).await);
    let actor = tri!(actor(principal.member.as_ref()));
    let m = tri!(tenant_member(&state, principal.tenant_id, id).await);
    // P16: the projects the member reaches, own and team grants.
    let projects = tri!(
        state
            .metadata
            .reinstate_member(actor, id)
            .await
            .map_err(|err| write_failed("reinstate member", err, "member not found"))
    );
    if m.status == MemberStatus::Suspended {
        info!(request_id = %principal.request_id, member_id = %id, "member: reinstated");
        access_changed_for(&state, principal.tenant_id, &[(id, m.zitadel_user_id.clone())], projects).await;
        if m.tenant_admin {
            reconcile_tenant(&state, principal.tenant_id).await;
        }
    }
    let z = tri!(admin_zitadel(&state));
    if let Err(err) = z.unlock_user(&m.zitadel_user_id).await {
        return zitadel_step_failed("reinstate member: unlock user", &err, "reinstate").into_response();
    }
    let m = tri!(tenant_member(&state, principal.tenant_id, id).await);
    Json(tri!(member_view(&state, &principal, m).await)).into_response()
}

#[utoipa::path(
    post,
    path = "/v1/members/{member_id}/revoke-postgres-access",
    operation_id = "revokeMemberPostgresAccess",
    tag = "members",
    summary = "Revoke a member's Postgres access now",
    description = "Every personal login of the member is revoked at once (the proxies forget it), and their member role on every branch of every project they reach gets a new password, which ends its open sessions (the SQL editor and the console reconnect with the new one). Their Studio access and grants are untouched.",
    params(("member_id" = Uuid, Path, description = "Member id")),
    responses(
        (status = 200, description = "What was revoked", body = RevokeResult),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant; `forbidden`: only a tenant admin may", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such member (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn revoke_member_postgres_access<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(authorize(&state, &principal, ep::REVOKE_MEMBER_POSTGRES_ACCESS.policy.access, Target::None).await);
    let actor = tri!(actor(principal.member.as_ref()));
    let m = tri!(tenant_member(&state, principal.tenant_id, id).await);
    // T8-6: the count the answer reports is what the write revoked: every record revoked now
    // (`revoked_now`) that wasn't already revoked before it, a login made between this read and
    // the write included.
    let before = tri!(
        state
            .metadata
            .member_personal_logins(id)
            .await
            .map_err(|err| internal("revoke postgres access: logins", err))
    );
    let touched = tri!(
        state
            .metadata
            .revoke_postgres_access(actor, id)
            .await
            .map_err(|err| write_failed("revoke postgres access", err, "member not found"))
    );
    let live = match state.metadata.member_personal_logins(id).await {
        Ok(after) => after
            .iter()
            .filter(|l| l.revoked_reason == Some(RevokeReason::RevokedNow))
            .filter(|l| {
                !before.iter().any(|b| {
                    b.database_id == l.database_id
                        && b.created_at == l.created_at
                        && b.revoked_at.is_some()
                })
            })
            .count(),
        // The write committed: the answer counts what was live before it.
        Err(err) => {
            warn!(member_id = %id, "revoke postgres access: the logins aren't readable after the write: {err}");
            before.iter().filter(|l| l.revoked_at.is_none()).count()
        }
    };
    info!(request_id = %principal.request_id, member_id = %id, revoked = live, "member: postgres access revoked");
    let rotated_branches = rotated_branches(&state, id, &touched).await;
    access_changed_for(&state, principal.tenant_id, &[(id, m.zitadel_user_id.clone())], touched).await;
    if m.tenant_admin {
        reconcile_tenant(&state, principal.tenant_id).await;
    }
    Json(RevokeResult { revoked: live, rotated_branches }).into_response()
}

/// How many branches of `projects` hold a member role of `member` (each was just marked for a
/// new password). A failed read counts as none: the answer only reports it.
async fn rotated_branches<M, J>(state: &AppState<M, J>, member: Uuid, projects: &[Uuid]) -> usize
where
    M: MetadataPort,
{
    let mut n = 0;
    for project in projects {
        let Ok(branches) = state.metadata.project_branches(*project).await else {
            continue;
        };
        for branch in branches {
            if state
                .metadata
                .member_branch_roles(branch)
                .await
                .is_ok_and(|rows| rows.iter().any(|r| r.member_id == member))
            {
                n += 1;
            }
        }
    }
    n
}

// --- Invites -----------------------------------------------------------------------------------

#[utoipa::path(
    post,
    path = "/v1/invites",
    operation_id = "createInvite",
    tag = "members",
    summary = "Invite a person",
    description = "Matches the email to a person: an existing member just gets the grants, all or none (200); an active user of the organisation who has a password and was never a member becomes an active member (201, `added`, no mail); anyone else, a removed member or a deactivated or passwordless user of the organisation included, gets a Zitadel invite mail with a link to Studio's `/accept-invite` and is `invited` until they accept (201); one who already has a way to sign in (a removed member invited again) gets Zitadel's password-reset mail with the same link marked `reset=1`, since Zitadel makes no invite code for them. A person whose own sign-up is pending is 409 `signup_pending`: approve it instead. With `delivery: \"link\"` (only when the invite makes the Zitadel user, or the user has never had a way to sign in (password, passkey, OTP, U2F, IdP link); else 409 `link_not_allowed`, send by email) nothing is mailed (no email provider needed): the response carries the link as `invite_url`, once, with `Cache-Control: no-store`; it is never stored or logged. Every grant is checked first, as `putProjectGrant` checks it, with the member's overrides.",
    request_body = CreateInviteRequest,
    responses(
        (status = 200, description = "An existing member got the grants", body = InviteResult),
        (status = 201, description = "A new member, `invited` or `added`", body = InviteResult),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: a grant's project doesn't exist in the tenant", body = ErrorEnvelope),
        (status = 409, description = "`email_ambiguous`: more than one user of the organisation has the email; `email_in_use_elsewhere`: a user outside the organisation has it; `signup_pending`: their sign-up waits for approval; `link_not_allowed`: a link for someone who can sign in; `project_deleting`: a grant's project is being deleted", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the email isn't one, a project is named twice, or a role can't be granted (`role` names it; `chain`, `database_id`, `branch_id`, `branch` and `branch_main` too when its memberships on a branch are why)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: Zitadel can't be reached or isn't configured", body = ErrorEnvelope),
    ),
)]
async fn create_invite<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiJson(body): ApiJson<CreateInviteRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize(
            &state,
            &principal,
            ep::CREATE_INVITE.policy.access,
            Target::None
        )
        .await
    );
    let email = tri!(invite_email(&body.email));
    let grants = tri!(invite_grants(&state, principal.tenant_id, body.grants).await);
    let teams = tri!(invite_team_ids(&state, principal.tenant_id, body.team_ids).await);
    let (status, result) = tri!(invite(&state, &principal, email, grants, teams, body.delivery).await);
    // S28: an invite's writer names no member: the whole tenant re-checks.
    access_changed(&state, principal.tenant_id, None);
    no_store((status, Json(result)).into_response())
}

#[utoipa::path(
    post,
    path = "/v1/projects/{id}/invites",
    operation_id = "createProjectInvite",
    tag = "members",
    summary = "Invite a person into this project",
    description = "As `createInvite` (including `delivery`), with one grant on this project.",
    params(("id" = Uuid, Path, description = "Project id")),
    request_body = CreateProjectInviteRequest,
    responses(
        (status = 200, description = "An existing member got the grant", body = InviteResult),
        (status = 201, description = "A new member, `invited` or `added`", body = InviteResult),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON, or a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`email_ambiguous`: more than one user of the organisation has the email; `email_in_use_elsewhere`: a user outside the organisation has it; `signup_pending`: their sign-up waits for approval; `link_not_allowed`: a link for someone who can sign in; `project_deleting`: the project is being deleted", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the email isn't one, or a role can't be granted (`role` names it; `chain`, `database_id`, `branch_id`, `branch` and `branch_main` too when its memberships on a branch are why)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: Zitadel can't be reached or isn't configured", body = ErrorEnvelope),
    ),
)]
async fn create_project_invite<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<CreateProjectInviteRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let project = tri!(
        authorize_project(
            &state,
            &principal,
            ep::CREATE_PROJECT_INVITE.policy.access,
            id
        )
        .await
    );
    // MH-8, P31: before anything else is read or made.
    if body.team_ids.is_some() {
        return unprocessable("teams can be attached only by a tenant admin's invite").into_response();
    }
    let email = tri!(invite_email(&body.email));
    let grant = InviteGrant {
        project_id: project.id,
        platform_role: body.platform_role,
        pg_roles: body.pg_roles,
    };
    let grants = tri!(invite_grants(&state, principal.tenant_id, vec![grant]).await);
    let (status, result) = tri!(invite(&state, &principal, email, grants, Vec::new(), body.delivery).await);
    // S28: an invite's writer names no member: the whole tenant re-checks.
    access_changed(&state, principal.tenant_id, None);
    no_store((status, Json(result)).into_response())
}

#[utoipa::path(
    post,
    path = "/v1/invites/{invite_id}/resend",
    operation_id = "resendInvite",
    tag = "members",
    summary = "Send an invite again",
    description = "Zitadel mails a new code (the old one stops working), or with `delivery: \"link\"` (only for an invitee with no way to sign in yet; others get 409 `link_not_allowed`) the response carries the new link as `invite_url` (once, `Cache-Control: no-store`, never stored or logged); either way the link's token changes and the invite lives another 72 hours. An expired invite can be resent. The body may be left out (then `email`).",
    params(("invite_id" = Uuid, Path, description = "Invite id")),
    request_body = Option<ResendInviteRequest>,
    responses(
        (status = 200, description = "The invite with its new expiry", body = ResentInvite),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such invite (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`invite_closed`: the invite was revoked or accepted; `link_not_allowed`: `link` delivery for an invitee who can sign in", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body isn't a JSON object with a valid `delivery`", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: Zitadel can't be reached or isn't configured", body = ErrorEnvelope),
    ),
)]
async fn resend_invite<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize(
            &state,
            &principal,
            ep::RESEND_INVITE.policy.access,
            Target::None
        )
        .await
    );
    let delivery = tri!(resend_delivery(&body));
    let actor = tri!(actor(principal.member.as_ref()));
    let m = tri!(open_invite(&state, principal.tenant_id, id).await);
    let z = tri!(admin_zitadel(&state));
    let mut reset = false;
    if delivery == Delivery::Link {
        match z.get_user(&m.zitadel_user_id).await {
            Ok(Some(u)) if u.has_password => return link_refused(z).await.into_response(),
            Ok(_) => {}
            Err(err) => return zitadel_failed("resend invite: read user", &err).into_response(),
        }
        match z.has_auth_methods(&m.zitadel_user_id).await {
            Ok(true) => return link_refused(z).await.into_response(),
            // Gone: the code call below says so.
            Ok(false) => {}
            Err(err) if err.is(refusals::AUTH_METHODS_USER_NOT_FOUND) => {}
            Err(err) => return zitadel_failed("resend invite: read auth methods", &err).into_response(),
        }
    } else {
        // Gone: the code call below says so.
        match z.get_user(&m.zitadel_user_id).await {
            Ok(Some(u)) => match needs_reset_code(z, &u).await {
                Ok(r) => reset = r,
                Err(err) => return zitadel_failed("resend invite: read auth methods", &err).into_response(),
            },
            Ok(None) => {}
            Err(err) => return zitadel_failed("resend invite: read user", &err).into_response(),
        }
    }
    // A new token per send: the earlier link's stops describing the invite.
    let token = InviteToken::mint();
    let sent = match delivery {
        Delivery::Email => mail_code(&state, z, &m.zitadel_user_id, reset, &token).await.map(|()| None),
        Delivery::Link => z
            .return_invite_code(&m.zitadel_user_id)
            .await
            .map(|code| Some(invite_link(&state, &m.zitadel_user_id, &code, &token))),
    };
    let invite_url = match sent {
        Ok(url) => url,
        Err(err) => return zitadel_failed("resend invite", &err).into_response(),
    };
    let expires_at = invite_expiry();
    tri!(
        state
            .metadata
            .resend_invite(actor, id, expires_at, token.hash)
            .await
            .map_err(|err| write_failed("resend invite", err, "invite not found"))
    );
    info!(request_id = %principal.request_id, invite = %id, ?delivery, "invite: resent");
    no_store(Json(ResentInvite { id, expires_at, invite_url }).into_response())
}

/// `resendInvite`'s optional body: nothing (only whitespace) or `null` is the default.
fn resend_delivery(body: &[u8]) -> Result<Delivery, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Delivery::default());
    }
    serde_json::from_slice::<Option<ResendInviteRequest>>(body)
        .map(|r| r.unwrap_or_default().delivery)
        .map_err(|err| unprocessable(format!("invalid body: {err}")))
}

#[utoipa::path(
    delete,
    path = "/v1/invites/{invite_id}",
    operation_id = "revokeInvite",
    tag = "members",
    summary = "Revoke an invite",
    description = "The member becomes `removed` and its grants go; then the invitee's Zitadel user is deleted when it never set a password, or deactivated when it once had one (a removed member invited again). Revoking an invite whose member is still removed retries the Zitadel step.",
    params(("invite_id" = Uuid, Path, description = "Invite id")),
    responses(
        (status = 204, description = "Revoked"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such invite (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`invite_closed`: the invite was accepted, or its member was invited again since", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: Zitadel isn't configured (nothing changed), or can't be reached after the revoke (revoke again to retry)", body = ErrorEnvelope),
    ),
)]
async fn revoke_invite<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize(
            &state,
            &principal,
            ep::REVOKE_INVITE.policy.access,
            Target::None
        )
        .await
    );
    let actor = tri!(actor(principal.member.as_ref()));
    let z = tri!(admin_zitadel(&state));
    let not_found = || ApiError::not_found("invite not found").into_response();
    let (member, _, revoked, accepted) = match state.metadata.invite(id).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(),
        Err(err) => return internal("load invite", err).into_response(),
    };
    let m = match state.metadata.member(member).await {
        Ok(Some(m)) if m.tenant_id == principal.tenant_id => m,
        Ok(_) => return not_found(),
        Err(err) => return internal("load invite member", err).into_response(),
    };
    let closed = || invite_closed().into_response();
    // I4: the metadata revoke first, under the member's lock; then Zitadel. A revoked invite
    // whose member is still removed retries the Zitadel step; any other closed one is 409.
    let user = if revoked.is_some() && accepted.is_none() && m.status == MemberStatus::Removed {
        m.zitadel_user_id.clone()
    } else if revoked.is_some() || accepted.is_some() || m.status != MemberStatus::Invited {
        return closed();
    } else {
        let user = tri!(
            state
                .metadata
                .revoke_invite(actor, id)
                .await
                .map_err(|err| write_failed("revoke invite", err, "invite not found"))
        );
        info!(request_id = %principal.request_id, invite = %id, member_id = %m.id, "invite: revoked");
        // S28: an invite's writer names no member.
        access_changed(&state, principal.tenant_id, None);
        user
    };
    if let Err(err) = clean_up_revoked_user(z, &user).await {
        return zitadel_failed("revoke invite: Zitadel user", &err).into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

// --- Grants ------------------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/v1/projects/{id}/grants",
    operation_id = "listProjectGrants",
    tag = "members",
    summary = "List a project's grants",
    description = "Every member's grant on the project (an invited member's counts once they accept), with its overrides on the project's databases. Tenant admins are admin everywhere without a grant.",
    params(("id" = Uuid, Path, description = "Project id")),
    responses(
        (status = 200, description = "The grants, by member id", body = Vec<ProjectGrant>),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_project_grants<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize_project(
            &state,
            &principal,
            ep::LIST_PROJECT_GRANTS.policy.access,
            id
        )
        .await
    );
    let access = tri!(
        state
            .metadata
            .project_access(id)
            .await
            .map_err(|err| internal("project access", err))
    );
    Json(grants_view(access)).into_response()
}

#[utoipa::path(
    put,
    path = "/v1/projects/{id}/grants/{member_id}",
    operation_id = "putProjectGrant",
    tag = "members",
    summary = "Grant a member access to a project",
    description = "Creates or replaces the member's grant. `admin` holds every role on the project, the database owner included (the names in `pg_roles` are checked, then dropped, and it is answered as `\"all\"`; overrides still narrow it per database), so `owner` (or the owner's name) is never listed. Below `admin`, each role must be a NOLOGIN group whose memberships (every one, whatever its options) reach no owner-level role (the owner, `neon_superuser`, `cloud_admin`, or SUPERUSER, CREATEROLE, CREATEDB, REPLICATION, BYPASSRLS), LOGIN role, reserved role or role that holds the ADMIN option on another role, on any branch. The same holds on each database for what the member's overrides there leave of the grant; a grant that breaks it is refused naming the database. Every branch of the project then reconciles its member roles.",
    params(
        ("id" = Uuid, Path, description = "Project id"),
        ("member_id" = Uuid, Path, description = "Member id"),
    ),
    request_body = PutGrantRequest,
    responses(
        (status = 200, description = "The grant", body = ProjectGrant),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON, or a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project or member (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`: the project is being deleted", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: a role can't be granted (`role` names it; `chain`, `database_id`, `branch_id`, `branch` and `branch_main` too when its memberships on a branch are why)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn put_project_grant<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, member_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<PutGrantRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let project =
        tri!(authorize_project(&state, &principal, ep::PUT_PROJECT_GRANT.policy.access, id).await);
    let actor = tri!(actor(principal.member.as_ref()));
    let member = tri!(live_member(&state, principal.tenant_id, member_id).await);
    let context = tri!(RoleContext::project(&state, project.id).await);
    // Names no grant may hold are refused at any level; R17: then an Admin grant is every role
    // on the project, whatever (valid) roles the request names.
    tri!(context.check_names(&body.pg_roles));
    let grant = GrantRow::new(body.platform_role, &body.pg_roles);
    // I1, MH-4: the new grant with the member's teams and each of their overrides keeps the
    // invariant. I4: the write goes ahead only while that access is still what the check read.
    let mut tries = 0;
    let changed = loop {
        tries += 1;
        let observed = tri!(observe(&state, id, member_id).await);
        tri!(context.check_effective(&observed.with_own(Some(grant.clone())), None));
        match state.metadata.put_project_grant(actor, member_id, id, &grant, &observed).await {
            Err(AccessWriteError::Changed) if tries < CHECKED_WRITE_TRIES => continue,
            other => {
                break tri!(other.map_err(|err| match err {
                    // The member is live (checked above): the project went.
                    AccessWriteError::NotFound =>
                        ApiError::new(409, ErrorCode::ProjectDeleting, "project is being deleted"),
                    other => write_failed("put grant", other, "member not found"),
                }));
            }
        }
    };
    if changed {
        info!(request_id = %principal.request_id, project_id = %id, %member_id, platform_role = grant.platform_role.as_str(), "grant: put");
        access_changed_for(&state, principal.tenant_id, &[(member_id, member.zitadel_user_id.clone())], vec![id]).await;
    }
    match tri!(project_grant(&state, id, member_id).await) {
        Some(view) => Json(view).into_response(),
        None => internal("put grant", "the grant written is gone").into_response(),
    }
}

#[utoipa::path(
    delete,
    path = "/v1/projects/{id}/grants/{member_id}",
    operation_id = "deleteProjectGrant",
    tag = "members",
    summary = "Take a member's access to a project away",
    description = "Every branch of the project then reconciles its member roles. The member's overrides on the project's databases stay; they can only narrow a later grant.",
    params(
        ("id" = Uuid, Path, description = "Project id"),
        ("member_id" = Uuid, Path, description = "Member id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project, member or grant (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: the member's access kept changing while this was being checked; try again", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: what the member keeps through their teams, narrowed by their overrides, would hold a role that can be held only with the admin platform role (`role`, `database_id`; `chain`, `branch_id`, `branch` and `branch_main` too when its memberships on a branch are why)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_project_grant<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, member_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize_project(
            &state,
            &principal,
            ep::DELETE_PROJECT_GRANT.policy.access,
            id
        )
        .await
    );
    let actor = tri!(actor(principal.member.as_ref()));
    let member = tri!(live_member(&state, principal.tenant_id, member_id).await);
    // MH-4: what the member keeps (their teams' grants, narrowed by their overrides) keeps the
    // invariant: an own Admin grant may have been what made a team's role safe to hold.
    // I4: the delete goes ahead only while that access is still what the check read (a team
    // grant written meanwhile is checked again).
    let context = tri!(RoleContext::project(&state, ProjectId::from(id)).await);
    let mut tries = 0;
    loop {
        tries += 1;
        let observed = tri!(observe(&state, id, member_id).await);
        tri!(context.check_effective(&observed.with_own(None), None));
        match state.metadata.delete_project_grant(actor, member_id, id, &observed).await {
            Err(AccessWriteError::Changed) if tries < CHECKED_WRITE_TRIES => continue,
            other => break tri!(other.map_err(|err| write_failed("delete grant", err, "grant not found"))),
        }
    }
    info!(request_id = %principal.request_id, project_id = %id, %member_id, "grant: deleted");
    access_changed_for(&state, principal.tenant_id, &[(member_id, member.zitadel_user_id.clone())], vec![id]).await;
    StatusCode::NO_CONTENT.into_response()
}

#[utoipa::path(
    put,
    path = "/v1/projects/{id}/grants/{member_id}/overrides",
    operation_id = "putGrantOverride",
    tag = "members",
    summary = "Narrow a member's grant on one database",
    description = "An override only narrows: its role is at most the grant's, its roles some of the grant's (any roles for an `admin` grant, which holds every role: the member keeps only those there, with the owner while still `admin`). Narrowing below `admin` takes the database owner away there, and the roles left must pass the same checks as a grant below `admin`. Every branch of the project then reconciles its member roles.",
    params(
        ("id" = Uuid, Path, description = "Project id"),
        ("member_id" = Uuid, Path, description = "Member id"),
    ),
    request_body = PutGrantOverrideRequest,
    responses(
        (status = 200, description = "The override", body = GrantOverride),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON, or a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project, member or database of the project (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: it narrows nothing, the member has no grant on the project, it would widen the grant, or a role can't be granted (`role` names it; `chain`, `database_id`, `branch_id`, `branch` and `branch_main` too when its memberships on a branch are why)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn put_grant_override<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, member_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<PutGrantOverrideRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(authorize_project(&state, &principal, ep::PUT_GRANT_OVERRIDE.policy.access, id).await);
    let actor = tri!(actor(principal.member.as_ref()));
    let member = tri!(live_member(&state, principal.tenant_id, member_id).await);
    let database = match state.metadata.get_database(body.database_id).await {
        Ok(Some(db))
            if db.project_id.as_uuid() == id
                && !matches!(
                    db.status,
                    DatabaseStatus::Deleting | DatabaseStatus::Deleted
                ) =>
        {
            db
        }
        Ok(_) => return ApiError::not_found("database not found").into_response(),
        Err(err) => return internal("override: load database", err).into_response(),
    };
    if body.platform_role.is_none() && body.pg_roles.is_none() {
        return unprocessable("an override narrows the platform role, the roles or both")
            .into_response();
    }
    // The role context is read once; the grant it is checked against is read again when the
    // write finds it changed (I4).
    let names = body.pg_roles.as_deref().unwrap_or(&[]);
    let context = tri!(RoleContext::database(&state, &database, names).await);
    tri!(context.check_names(names));
    let o = OverrideRow {
        platform_role: body.platform_role,
        pg_roles: body.pg_roles.clone(),
    };
    let db = body.database_id.as_uuid();
    let mut tries = 0;
    let changed = loop {
        tries += 1;
        let observed = tri!(observe(&state, id, member_id).await);
        // MH-4: an override narrows the member's union (their own grant and their teams').
        let Some(granted) = observed.sources(None).effective() else {
            return unprocessable("the member has no grant on this project to narrow").into_response();
        };
        if body.platform_role.is_some_and(|r| r > granted.platform_role) {
            return unprocessable(format!(
                "an override can't widen the grant: its role is {}",
                granted.platform_role.as_str()
            ))
            .into_response();
        }
        // R17: Admin from any source holds every role, so any role narrows it.
        if let Some(extra) = body.pg_roles.iter().flatten().find(|r| match &granted.roles {
            RoleSet::All { .. } => false,
            RoleSet::Only(set) => !set.contains(&PgRole::Named((*r).clone())),
        }) {
            return unprocessable(format!(
                "an override can't widen the grant: `{extra}` isn't one of its roles"
            ))
            .with_extra("role", json!(extra))
            .into_response();
        }
        // The invariant on this database: what the union narrowed by the new override leaves.
        tri!(context.check_effective(&observed.with_override(db, Some(o.clone())), Some(db)));
        match state.metadata.put_override(actor, member_id, db, &o, &observed).await {
            Err(AccessWriteError::Changed) if tries < CHECKED_WRITE_TRIES => continue,
            other => {
                break tri!(other.map_err(|err| write_failed("put override", err, "database not found")));
            }
        }
    };
    if changed {
        info!(request_id = %principal.request_id, project_id = %id, %member_id, database_id = %body.database_id, "override: put");
        access_changed_for(&state, principal.tenant_id, &[(member_id, member.zitadel_user_id.clone())], vec![id]).await;
    }
    let mut pg_roles = o.pg_roles;
    if let Some(roles) = pg_roles.as_mut() {
        roles.sort();
        roles.dedup();
    }
    Json(GrantOverride {
        database_id: body.database_id,
        platform_role: o.platform_role,
        pg_roles,
    })
    .into_response()
}

#[utoipa::path(
    delete,
    path = "/v1/projects/{id}/grants/{member_id}/overrides",
    operation_id = "deleteGrantOverride",
    tag = "members",
    summary = "Stop narrowing a member's grant on a database",
    params(
        ("id" = Uuid, Path, description = "Project id"),
        ("member_id" = Uuid, Path, description = "Member id"),
        ("database_id" = Uuid, Query, description = "The database the override narrows"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or `database_id` is missing or isn't one", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project, member, database of the project or override (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_grant_override<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, member_id)): ApiPath<(Uuid, Uuid)>,
    query: Result<Query<OverrideQuery>, QueryRejection>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize_project(
            &state,
            &principal,
            ep::DELETE_GRANT_OVERRIDE.policy.access,
            id
        )
        .await
    );
    let Query(OverrideQuery { database_id }) = tri!(query.map_err(
        |rej| ApiError::invalid_request(400, format!("invalid query: {}", rej.body_text()))
    ));
    let actor = tri!(actor(principal.member.as_ref()));
    let member = tri!(live_member(&state, principal.tenant_id, member_id).await);
    let database = match state.metadata.get_database(DatabaseId(database_id)).await {
        Ok(Some(db)) if db.project_id.as_uuid() == id => db,
        Ok(_) => return ApiError::not_found("database not found").into_response(),
        Err(err) => return internal("override: load database", err).into_response(),
    };
    // The invariant on this database once the narrowing goes: the union alone (MH-4).
    let context = tri!(RoleContext::of(&state, std::slice::from_ref(&database)).await);
    let mut tries = 0;
    loop {
        tries += 1;
        let observed = tri!(observe(&state, id, member_id).await);
        tri!(context.check_effective(&observed.with_override(database_id, None), Some(database_id)));
        match state.metadata.delete_override(actor, member_id, database_id, &observed).await {
            Err(AccessWriteError::Changed) if tries < CHECKED_WRITE_TRIES => continue,
            other => {
                tri!(other.map_err(|err| write_failed("delete override", err, "override not found")));
                break;
            }
        }
    }
    info!(request_id = %principal.request_id, project_id = %id, %member_id, %database_id, "override: deleted");
    access_changed_for(&state, principal.tenant_id, &[(member_id, member.zitadel_user_id.clone())], vec![id]).await;
    StatusCode::NO_CONTENT.into_response()
}

// --- The access log ----------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/v1/access-events",
    operation_id = "listAccessEvents",
    tag = "members",
    summary = "Read the tenant's access log",
    description = "Every access change (members, grants, overrides, invites, open sign-up, the tenant's name), newest first. The log can't be changed.",
    params(
        ("before" = Option<i64>, Query, description = "Only events older than this id (`next_before` of the previous page)"),
        ("limit" = Option<u32>, Query, description = "1 to 200 (default 50)"),
        ("member_id" = Option<Uuid>, Query, description = "Only events whose target or actor is this member"),
    ),
    responses(
        (status = 200, description = "A page of events", body = AccessEventPage),
        (status = 400, description = "`invalid_request`: `before`, `limit` or `member_id` isn't valid", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_access_events<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize(
            &state,
            &principal,
            ep::LIST_ACCESS_EVENTS.policy.access,
            Target::None
        )
        .await
    );
    let (before, limit, member) = tri!(events_query(query));
    Json(tri!(
        events_page(&state, principal.tenant_id, None, member, before, limit).await
    ))
    .into_response()
}

#[utoipa::path(
    get,
    path = "/v1/projects/{id}/access-events",
    operation_id = "listProjectAccessEvents",
    tag = "members",
    summary = "Read a project's access log",
    description = "The access changes on this project (its grants and its databases' overrides), newest first.",
    params(
        ("id" = Uuid, Path, description = "Project id"),
        ("before" = Option<i64>, Query, description = "Only events older than this id (`next_before` of the previous page)"),
        ("limit" = Option<u32>, Query, description = "1 to 200 (default 50)"),
        ("member_id" = Option<Uuid>, Query, description = "Only events whose target or actor is this member"),
    ),
    responses(
        (status = 200, description = "A page of events", body = AccessEventPage),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or `before`, `limit` or `member_id` isn't valid", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_project_access_events<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize_project(
            &state,
            &principal,
            ep::LIST_PROJECT_ACCESS_EVENTS.policy.access,
            id
        )
        .await
    );
    let (before, limit, member) = tri!(events_query(query));
    Json(tri!(
        events_page(&state, principal.tenant_id, Some(id), member, before, limit).await
    ))
    .into_response()
}


// --- Instance settings -------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/v1/instance/settings",
    operation_id = "getInstanceSettings",
    tag = "members",
    summary = "Read the instance's settings",
    responses(
        (status = 200, description = "The settings", body = InstanceSettings),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant (and isn't the operator org)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn get_instance_settings<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: InstancePrincipal,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    Json(tri!(settings(&state, principal.tenant_id).await)).into_response()
}

async fn settings<M, J>(
    state: &AppState<M, J>,
    tenant: Option<TenantId>,
) -> Result<InstanceSettings, ApiError>
where
    M: MetadataPort,
{
    let open_signup = match tenant {
        Some(tenant) => state
            .metadata
            .open_signup(tenant)
            .await
            .map_err(|err| internal("open sign-up", err))?,
        None => false,
    };
    Ok(InstanceSettings {
        open_signup,
        open_signup_switchable: state.policy.open_signup_switchable(),
    })
}

#[utoipa::path(
    patch,
    path = "/v1/instance/settings",
    operation_id = "updateInstanceSettings",
    tag = "members",
    summary = "Change the instance's settings",
    description = "Open sign-up lets anyone sign up; they start with no access. Self-hosted only.",
    request_body = UpdateInstanceSettingsRequest,
    responses(
        (status = 200, description = "The settings", body = InstanceSettings),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant (and isn't the operator org); `not_available_in_platform_mode`: open sign-up can't be switched on in platform mode", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: `open_signup` is missing or isn't a boolean", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn update_instance_settings<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: InstancePrincipal,
    ApiJson(body): ApiJson<UpdateInstanceSettingsRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    if !state.policy.open_signup_switchable() {
        return ApiError::new(
            403,
            ErrorCode::NotAvailableInPlatformMode,
            "open sign-up can't be switched on in platform mode",
        )
        .into_response();
    }
    let Some(tenant) = principal.tenant_id else {
        return internal("instance settings", "a self-hosted caller without a tenant")
            .into_response();
    };
    let actor = tri!(actor(principal.member.as_ref()));
    tri!(
        state
            .metadata
            .set_open_signup(actor, tenant, body.open_signup)
            .await
            .map_err(|err| write_failed("set open sign-up", err, "tenant not found"))
    );
    info!(request_id = %principal.request_id, open_signup = body.open_signup, "instance: open sign-up set");
    Json(tri!(settings(&state, Some(tenant)).await)).into_response()
}

// --- Invite delivery ---------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/v1/invites/delivery",
    operation_id = "getInviteDelivery",
    tag = "members",
    summary = "Can invites be emailed?",
    description = "Asks Zitadel whether an email provider is active. Without one, invite by link (`delivery: \"link\"`). A Zitadel that can't be asked is not an error: the answer is `email_available: false` with the reason.",
    responses(
        (status = 200, description = "Whether invites can be emailed", body = InviteDelivery),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant; `no_access_yet` or `not_available_in_platform_mode`: the caller is no active member", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the caller was removed from the tenant", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn get_invite_delivery<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize(
            &state,
            &principal,
            ep::GET_INVITE_DELIVERY.policy.access,
            Target::None
        )
        .await
    );
    let (email_available, reason) = match &state.zitadel {
        None => (false, Some(DeliveryReason::ZitadelNotConfigured)),
        Some(z) => match z.email_provider_active().await {
            Ok(true) => (true, None),
            Ok(false) => (false, Some(DeliveryReason::NoEmailProvider)),
            Err(err) => {
                warn!(request_id = %principal.request_id, "invite delivery: asking Zitadel failed: {err}");
                (false, Some(DeliveryReason::ZitadelUnavailable))
            }
        },
    };
    Json(InviteDelivery { email_available, reason }).into_response()
}

// --- The tenant's name -------------------------------------------------------------------------

/// The longest display name, in characters.
const DISPLAY_NAME_MAX: usize = 80;

/// A display name as stored: trimmed, `None` when empty; at most [`DISPLAY_NAME_MAX`]
/// characters and no control characters.
fn display_name(raw: Option<String>) -> Result<Option<String>, ApiError> {
    let Some(raw) = raw else { return Ok(None) };
    let name = raw.trim();
    if name.chars().count() > DISPLAY_NAME_MAX {
        return Err(unprocessable(format!("display_name is at most {DISPLAY_NAME_MAX} characters")));
    }
    if name.chars().any(char::is_control) {
        return Err(unprocessable("display_name has a control character"));
    }
    Ok(Some(name.to_string()).filter(|n| !n.is_empty()))
}

#[utoipa::path(
    patch,
    path = "/v1/tenant",
    operation_id = "updateTenant",
    tag = "members",
    summary = "Rename the tenant",
    description = "Sets the tenant's display name (what Studio and invite pages call it); readers fall back to the slug while it is empty. One `tenant_renamed` access event records the change.",
    request_body = UpdateTenantRequest,
    responses(
        (status = 200, description = "The stored name", body = TenantName),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant; `forbidden`: only a tenant admin may (`required`: `tenant admin`)", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: `display_name` is missing, isn't a string or `null`, is over 80 characters or has a control character", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn update_tenant<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiJson(body): ApiJson<UpdateTenantRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    tri!(
        authorize(
            &state,
            &principal,
            ep::UPDATE_TENANT.policy.access,
            Target::None
        )
        .await
    );
    let actor = tri!(actor(principal.member.as_ref()));
    let name = tri!(display_name(body.display_name));
    tri!(
        state
            .metadata
            .set_tenant_display_name(actor, principal.tenant_id, name.as_deref())
            .await
            .map_err(|err| write_failed("rename tenant", err, "tenant not found"))
    );
    info!(request_id = %principal.request_id, "tenant: renamed");
    Json(TenantName { display_name: name }).into_response()
}

#[cfg(test)]
mod project_scope_tests {
    use super::*;

    // T9-H1, EOS-5 (coder's edges): a revocation's `logins` and its `count` follow the
    // project; an entry that names no value of the project (malformed, missing) is left out.
    #[test]
    fn a_project_scope_keeps_only_its_own_values_and_fails_closed() {
        let (project, db, other_db) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let scope = ProjectScope {
            project,
            databases: HashSet::from([db]),
            branches: HashSet::new(),
        };
        let mut v = json!({
            "count": 3,
            "logins": [{ "database_id": db, "role_name": "a" }, { "database_id": other_db, "role_name": "b" }, { "role_name": "c" }],
            "grants": [{ "project_id": "not-a-uuid" }, { "project_id": project }],
            "rotated_branches": [Uuid::new_v4()],
            "reason": "revoked_now",
        });
        scope.apply(&mut v);
        assert_eq!(v["count"], 1);
        assert_eq!(v["logins"], json!([{ "database_id": db, "role_name": "a" }]));
        assert_eq!(v["grants"], json!([{ "project_id": project }]));
        assert_eq!(v["rotated_branches"], json!([]));
        assert_eq!(v["reason"], "revoked_now");
        let mut scalar = json!("x");
        scope.apply(&mut scalar);
        assert_eq!(scalar, json!("x"));
    }
}
