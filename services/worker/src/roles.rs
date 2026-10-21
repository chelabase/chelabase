//! Roles made through the API (spec 2026-10-01-login-roles, revision 2, Part 3; step 6g U1):
//! create (LOGIN or NOLOGIN, with the U2 attributes and memberships), update, reset and
//! delete, each under the endpoint's lock and straight in SQL (forwarding is off for
//! `admin_sql`), with the metadata copy written alongside. NOLOGIN roles are made by
//! `admin_sql` like the others, never through the compute spec (which lists only the owner).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use chelabase_metadata::{
    ApiRoleChange, BranchKind, BranchStatus, InsertRoleOutcome, NewApiRole, RenameCopyOutcome,
    RoleAttributes, RoleCopy, RoleOrigin, RoleWriteOutcome, SecretChange,
};
use chelabase_nats::{
    KeyRef, RESET_NOT_APPLIED, ROLE_CREATE_NOT_APPLIED, ROLE_DELETE_NOT_APPLIED,
    ROLE_UPDATE_NOT_APPLIED, RoleCreateRequest, RoleReply, RoleRequest, RoleUpdateRequest,
    SUBJECT_ROLE_CREATE, SUBJECT_ROLE_DELETE, SUBJECT_ROLE_RESET, SUBJECT_ROLE_UPDATE,
};
use chelabase_neon::ComputeBackend;
use chelabase_neon::backend::admin_psql_command;
use chelabase_secrets::{open_role_password, seal_role_password};
use chelabase_types::BranchId;
use chelabase_types::data_key::DataKeyKind;
use chelabase_types::role_name::{
    RoleNameError, check_any_role_name, check_login_role_name, is_membership_reserved,
    is_reserved_for, is_reserved_target, mark_personal_login_sql, quote_ident, quote_literal,
};
use chrono::{DateTime, SubsecRound, Utc};
use tokio::time::timeout;
use tracing::{info, warn};

use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::resync::{
    ObservedRole, WantedRole, create_role_sql, parse_observed, password_sql, role_read_sql,
    set_password_sql, wanted_statements,
};
use crate::{OWNER_ROLE, Worker, credentials};

/// How a role operation ended; [`role_reply`] turns it into the RPC's reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleOutcome {
    Ok,
    /// The caller's fault: `(code, message)`.
    Refused(&'static str, String),
    /// Data API keys still use the role.
    InUse(Vec<KeyRef>),
    /// The branch isn't ready, or its compute can't be reached.
    NotReady,
    /// No such login role.
    NotFound,
    /// Stored, but not yet applied to the compute; the resync retries.
    NotApplied,
    /// Something broke on our side; the message is for logs.
    Failed(String),
}

/// `outcome` as the RPC's reply.
pub fn role_reply(outcome: RoleOutcome) -> RoleReply {
    let refused = |code: &str, error: String, keys: Vec<KeyRef>| RoleReply {
        ok: false,
        code: Some(code.to_string()),
        error: Some(error),
        keys,
    };
    match outcome {
        RoleOutcome::Ok => RoleReply {
            ok: true,
            code: None,
            error: None,
            keys: Vec::new(),
        },
        RoleOutcome::Refused(code, message) => refused(code, message, Vec::new()),
        RoleOutcome::InUse(keys) => refused(
            "role_in_use",
            "data API keys still use the role; revoke them first".into(),
            keys,
        ),
        RoleOutcome::NotReady => refused(
            "branch_not_ready",
            "the branch isn't ready, or its compute can't be reached".into(),
            Vec::new(),
        ),
        RoleOutcome::NotFound => refused(
            "not_found",
            "the branch has no login role of that name".into(),
            Vec::new(),
        ),
        RoleOutcome::NotApplied => refused("not_applied", RESET_NOT_APPLIED.into(), Vec::new()),
        RoleOutcome::Failed(message) => refused("failed", message, Vec::new()),
    }
}

/// The create rules (lowercase, not reserved), as a refusal.
fn check_name(name: &str) -> Result<(), RoleOutcome> {
    check_login_role_name(name).map_err(|e| match e {
        RoleNameError::Invalid => RoleOutcome::Refused(
            "invalid_role_name",
            format!(
                "{name:?} is not a valid role name: lowercase letters, digits and underscores, \
                 starting with a letter or an underscore, at most 63 characters"
            ),
        ),
        RoleNameError::Reserved => reserved(name),
    })
}

/// Any name Postgres can hold (R14: reset and delete reach roles made in SQL), as a refusal.
fn check_any_name(name: &str) -> Result<(), RoleOutcome> {
    check_any_role_name(name).map_err(|_| {
        RoleOutcome::Refused(
            "invalid_role_name",
            format!("{name:?} is not a valid role name: 1 to 63 bytes, without NUL"),
        )
    })
}

/// A role to create (U1): a LOGIN role gets a generated password, a NOLOGIN role none. Only
/// the attributes of U2 can be expressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleSpec {
    pub name: String,
    pub owner_level: bool,
    pub login: bool,
    pub inherit: bool,
    /// `-1`: no limit.
    pub connection_limit: i32,
    pub valid_until: Option<DateTime<Utc>>,
    pub bypass_rls: bool,
    /// Roles it's made a member of; each must exist, the owner must administer it, and none
    /// may be one of Chelabase's own (`is_membership_reserved`).
    pub member_of: Vec<String>,
}

impl RoleSpec {
    /// A LOGIN role with Postgres' default attributes.
    pub fn login(name: &str, owner_level: bool) -> Self {
        Self {
            name: name.to_string(),
            owner_level,
            login: true,
            inherit: true,
            connection_limit: -1,
            valid_until: None,
            bypass_rls: false,
            member_of: Vec::new(),
        }
    }

    /// Its attributes as stored: VALID UNTIL to the microsecond, memberships sorted once each.
    pub fn attributes(&self) -> RoleAttributes {
        RoleAttributes {
            inherit: self.inherit,
            connection_limit: self.connection_limit,
            valid_until: self.valid_until.map(to_micros),
            bypass_rls: self.bypass_rls,
            member_of: sorted(&self.member_of),
            // A role the API makes is granted its groups plainly (SET, no ADMIN).
            member_no_set: Vec::new(),
            member_admin: Vec::new(),
        }
    }
}

impl From<&RoleCreateRequest> for RoleSpec {
    fn from(r: &RoleCreateRequest) -> Self {
        Self {
            name: r.name.clone(),
            owner_level: r.owner_level,
            login: r.login,
            inherit: r.inherit,
            connection_limit: r.connection_limit,
            valid_until: r.valid_until,
            bypass_rls: r.bypass_rls,
            member_of: r.member_of.clone(),
        }
    }
}

/// A change to a role (U1): `None` leaves a field as it is; `valid_until: Some(None)` clears
/// the expiry; `member_of` is the whole new list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RolePatch {
    pub owner_level: Option<bool>,
    pub login: Option<bool>,
    pub inherit: Option<bool>,
    pub connection_limit: Option<i32>,
    pub valid_until: Option<Option<DateTime<Utc>>>,
    pub bypass_rls: Option<bool>,
    pub member_of: Option<Vec<String>>,
}

impl RolePatch {
    /// `current` with this change.
    pub fn apply(&self, current: &RoleAttributes) -> RoleAttributes {
        let member_of = match &self.member_of {
            Some(members) => sorted(members),
            None => current.member_of.clone(),
        };
        // A membership the change keeps keeps its options (I2); a new one is granted plainly.
        let kept = |options: &[String]| -> Vec<String> {
            options.iter().filter(|g| member_of.contains(g)).cloned().collect()
        };
        RoleAttributes {
            member_no_set: kept(&current.member_no_set),
            member_admin: kept(&current.member_admin),
            inherit: self.inherit.unwrap_or(current.inherit),
            connection_limit: self.connection_limit.unwrap_or(current.connection_limit),
            valid_until: match self.valid_until {
                Some(at) => at.map(to_micros),
                None => current.valid_until,
            },
            bypass_rls: self.bypass_rls.unwrap_or(current.bypass_rls),
            member_of,
        }
    }
}

impl From<&RoleUpdateRequest> for RolePatch {
    fn from(r: &RoleUpdateRequest) -> Self {
        Self {
            owner_level: r.owner_level,
            login: r.login,
            inherit: r.inherit,
            connection_limit: r.connection_limit,
            valid_until: r.valid_until,
            bypass_rls: r.bypass_rls,
            member_of: r.member_of.clone(),
        }
    }
}

/// `at` to the microsecond Postgres keeps.
fn to_micros(at: DateTime<Utc>) -> DateTime<Utc> {
    at.trunc_subsecs(6)
}

fn sorted(names: &[String]) -> Vec<String> {
    let set: std::collections::BTreeSet<&String> = names.iter().collect();
    set.into_iter().cloned().collect()
}

fn membership_refused(message: String) -> RoleOutcome {
    RoleOutcome::Refused("role_membership_not_allowed", message)
}

/// `-1` (no limit) or more.
fn check_connection_limit(limit: i32) -> Result<(), RoleOutcome> {
    if limit < -1 {
        return Err(RoleOutcome::Refused(
            "role_attribute_not_allowed",
            format!("connection_limit {limit} is out of range: -1 (no limit) or more"),
        ));
    }
    Ok(())
}

/// U2: never a membership in the owner, Chelabase's own roles, a `pg_` role, or the role
/// itself; every target a name Postgres can hold.
fn check_members(name: &str, members: &[String]) -> Result<(), RoleOutcome> {
    for target in members {
        if check_any_role_name(target).is_err() {
            return Err(membership_refused(format!("{target:?} is not a valid role name")));
        }
        if target == name || target == OWNER_ROLE || is_membership_reserved(target) {
            return Err(membership_refused(format!(
                "a role can't be made a member of {target:?}"
            )));
        }
    }
    Ok(())
}

/// Each of `targets` that exists, hex-encoded, whether the owner holds ADMIN on it, and
/// whether it is itself a member, at any depth, of a role no API role may reach
/// (`neon_superuser`, `cloud_admin`, the owner, the Data API roles, any `chelabase_…` or
/// `pg_…` role): `hex|t|f`, one per line. A superuser target reaches every role.
fn membership_check_sql(owner: &str, targets: &[String]) -> String {
    let names: Vec<String> = targets.iter().map(|t| quote_literal(t)).collect();
    let owner = quote_literal(owner);
    format!(
        "SET search_path = pg_catalog, pg_temp; SELECT encode(convert_to(r.rolname, 'UTF8'), \
         'hex'), pg_has_role({owner}, r.oid, 'MEMBER WITH ADMIN OPTION'), EXISTS (SELECT 1 \
         FROM pg_roles f WHERE f.oid <> r.oid AND (f.rolname IN ('neon_superuser', \
         'cloud_admin', 'authenticator', 'anon', 'authenticated', {owner}) OR lower(f.rolname) \
         LIKE 'chelabase\\_%' OR lower(f.rolname) LIKE 'pg\\_%') AND pg_has_role(r.oid, f.oid, \
         'MEMBER')) FROM pg_roles r WHERE r.rolname IN ({}) ORDER BY 1;",
        names.join(", ")
    )
}

/// The rules [`Worker::create_role`] checks before it reads anything: the name, the connection
/// limit and the memberships. A merge's rehearsal (step 6g, P5) checks them too, so it refuses
/// what the apply would.
pub(crate) fn check_spec(spec: &RoleSpec) -> Result<(), RoleOutcome> {
    check_name(&spec.name)?;
    check_connection_limit(spec.connection_limit)?;
    check_members(&spec.name, &spec.member_of)
}

/// The rules [`Worker::update_role`] checks before it reads anything.
pub(crate) fn check_patch(name: &str, patch: &RolePatch) -> Result<(), RoleOutcome> {
    check_any_name(name)?;
    if name == OWNER_ROLE || is_reserved_target(name, OWNER_ROLE) {
        return Err(reserved(name));
    }
    if let Some(limit) = patch.connection_limit {
        check_connection_limit(limit)?;
    }
    if let Some(members) = &patch.member_of {
        check_members(name, members)?;
    }
    Ok(())
}

/// What an update runs (as `cloud_admin`, memberships as the owner) to take the catalog's `o`
/// to `wanted`: the password (`verifier` when LOGIN was turned on; dropped when the patch turns
/// LOGIN off, and only then: a role made NOLOGIN in SQL keeps its password, E32), then
/// [`wanted_statements`]: the attributes, owner level turned on or off, the memberships.
/// `wanted` starts from the role's current state ([`current_role_state`]), so owner level
/// changes only when the patch (or an unconfirmed earlier change) asks.
pub(crate) fn update_statements(
    wanted: &WantedRole<'_>,
    o: &ObservedRole,
    verifier: Option<&str>,
    patch: &RolePatch,
) -> Vec<String> {
    let mut out = Vec::new();
    match verifier {
        Some(verifier) => out.push(password_sql(wanted.name, Some(verifier))),
        None if patch.login == Some(false) && o.password.is_some() => {
            out.push(password_sql(wanted.name, None))
        }
        None => {}
    }
    out.extend(wanted_statements(OWNER_ROLE, wanted, o));
    out
}

/// A role's state a change starts from (E32): `(owner_level, login, attributes)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoleState {
    pub owner_level: bool,
    pub login: bool,
    pub attributes: RoleAttributes,
}

/// Where a change to a role starts (E32, "SQL controls everything"): the catalog's state
/// (`observed`), except for a role made through the API whose last API change the compute
/// hasn't confirmed (`attributes_pending`) or that the compute doesn't have: then the stored
/// wanted state, so that change isn't lost. A role without an API row is the user's only when
/// the owner administers it (made by the owner, or granted to it with ADMIN); any other, or
/// one that exists nowhere, is [`RoleOutcome::NotFound`]. The merge's phase 0 (and its
/// rehearsal) use it the same way.
pub(crate) fn current_role_state(
    row: Option<&RoleCopy>,
    observed: Option<&ObservedRole>,
) -> Result<RoleState, RoleOutcome> {
    let api_row = row.filter(|r| r.origin == RoleOrigin::Api);
    match (api_row, observed) {
        (Some(r), Some(o)) if !r.attributes_pending => Ok(observed_state(o)),
        (Some(r), _) => Ok(RoleState {
            owner_level: r.api_owner_level,
            login: r.login,
            attributes: r.attributes.clone(),
        }),
        (None, Some(o)) if o.owner_admin => Ok(observed_state(o)),
        (None, _) => Err(RoleOutcome::NotFound),
    }
}

fn observed_state(o: &ObservedRole) -> RoleState {
    RoleState {
        owner_level: o.owner_attributes,
        login: o.login,
        attributes: o.attributes(),
    }
}

/// [`membership_check_sql`]'s answer: each existing target's name and `(owner holds ADMIN,
/// reaches a reserved role)`.
fn parse_membership_check(
    out: &str,
) -> Result<std::collections::HashMap<String, (bool, bool)>, String> {
    let flag = |f: &str| match f {
        "t" => Some(true),
        "f" => Some(false),
        _ => None,
    };
    let mut found = std::collections::HashMap::new();
    for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let fields: Vec<&str> = line.split('|').collect();
        let parsed = match fields[..] {
            [hex_name, owner_admin, reaches] => (|| {
                let name = String::from_utf8(hex::decode(hex_name).ok()?).ok()?;
                Some((name, (flag(owner_admin)?, flag(reaches)?)))
            })(),
            _ => None,
        };
        let (name, flags) = parsed.ok_or_else(|| "an unexpected answer".to_string())?;
        found.insert(name, flags);
    }
    Ok(found)
}

/// The verdict on each of `targets` from the check's answer: it exists, the owner holds ADMIN
/// on it, and it reaches no reserved role.
fn judge_member_targets(
    targets: &[String],
    found: &std::collections::HashMap<String, (bool, bool)>,
) -> Result<(), RoleOutcome> {
    for target in targets {
        match found.get(target) {
            Some((_, true)) => {
                return Err(membership_refused(format!(
                    "{target:?} is itself a member of a reserved role"
                )));
            }
            Some((true, false)) => {}
            Some((false, false)) => {
                return Err(membership_refused(format!(
                    "the owner can't grant membership in {target:?}"
                )));
            }
            None => {
                return Err(membership_refused(format!("no role named {target:?} exists")));
            }
        }
    }
    Ok(())
}

/// E29: a rename's names, before anything is read (`rename_role`, and the rehearsal's rename,
/// T14 F3 M5): both names Postgres can hold, neither the owner, a reserved name nor any
/// `chelabase_…` role, and not the same name.
pub(crate) fn check_rename(from: &str, to: &str) -> Result<(), RoleOutcome> {
    for name in [from, to] {
        check_any_name(name)?;
        if name == OWNER_ROLE || is_reserved_target(name, OWNER_ROLE) {
            return Err(reserved(name));
        }
    }
    if from == to {
        return Err(RoleOutcome::Refused(
            "invalid_role_name",
            format!("the role is already named {to:?}"),
        ));
    }
    Ok(())
}

/// E29: a renamed role is the user's when it exists (`observed`) and the owner administers it,
/// or it has an API row (`row`).
pub(crate) fn rename_is_ours(observed: Option<&ObservedRole>, row: Option<&RoleCopy>) -> bool {
    observed.is_some_and(|o| {
        o.owner_admin || row.is_some_and(|r| r.origin == RoleOrigin::Api)
    })
}

/// The refusal for a name that's taken.
pub(crate) fn name_taken(name: &str) -> RoleOutcome {
    exists(name)
}

fn reserved(name: &str) -> RoleOutcome {
    RoleOutcome::Refused(
        "reserved_role_name",
        format!("the role name {name:?} is reserved"),
    )
}

/// Every database a role's objects can live in, one hex-encoded name per line (a name may
/// hold any character, a newline included).
const DATABASES_SQL: &str = "SET search_path = pg_catalog, pg_temp; SELECT \
    encode(convert_to(datname, 'UTF8'), 'hex') FROM pg_database WHERE datallowconn AND NOT \
    datistemplate ORDER BY datname;";

/// [`DATABASES_SQL`]'s answer as names, each one psql can be given.
fn database_names(out: &str) -> anyhow::Result<Vec<String>> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|line| {
            let name = hex::decode(line)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .with_context(|| format!("unexpected database name {line:?} in the listing"))?;
            admin_psql_command(&name)?;
            Ok(name)
        })
        .collect()
}

/// Which of `names` exist, hex-encoded, one per line (a rename's outcome when its answer was
/// lost). The comment is the fakes' needle.
pub(crate) fn role_names_sql(names: &[&str]) -> String {
    let names: Vec<String> = names.iter().map(|n| quote_literal(n)).collect();
    format!(
        "SET search_path = pg_catalog, pg_temp; SELECT /* chelabase: role names */ \
         encode(convert_to(rolname, 'UTF8'), 'hex') FROM pg_roles WHERE rolname IN ({});",
        names.join(", ")
    )
}

/// [`role_names_sql`]'s answer.
pub(crate) fn parse_role_names(out: &str) -> Result<std::collections::HashSet<String>, String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|line| {
            hex::decode(line)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .ok_or_else(|| "an unexpected role name in the answer".to_string())
        })
        .collect()
}

/// Whether `name` exists in SQL (`admin_sql` prints `1` for a hit). On the superuser session
/// the search path is pinned first: the owner can set the database's `search_path` and create
/// a `public.=` that would otherwise run as superuser.
pub(crate) fn exists_sql(name: &str) -> String {
    format!(
        "SET search_path = pg_catalog, pg_temp; SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = {}",
        quote_literal(name)
    )
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Makes a personal login on `endpoint` (RT4-4): `CREATE ROLE name LOGIN PASSWORD <verifier>`
    /// and the marker in one script, run in one transaction by a superuser session, so no other
    /// session sees the role unmarked and the owner never holds ADMIN on it. A plain login with
    /// none of the owner's attributes. The name is never adopted: `CREATE ROLE` on an existing
    /// one fails and nothing else runs. Takes no endpoint lock and writes no metadata row (the
    /// Members step records it). Reserved names and any password that isn't a SCRAM verifier are
    /// refused before any SQL runs.
    pub async fn create_personal_login(
        &self,
        endpoint: &str,
        name: &str,
        scram_verifier: &str,
    ) -> anyhow::Result<()> {
        if check_any_role_name(name).is_err()
            || name == OWNER_ROLE
            || is_reserved_target(name, OWNER_ROLE)
        {
            anyhow::bail!("the role name {name:?} can't be a personal login");
        }
        // A whole verifier only: anything else would be taken by `PASSWORD '…'` as a plaintext
        // password (and the error never echoes it).
        if !chelabase_neon::scram::is_verifier(scram_verifier) {
            anyhow::bail!("a personal login needs a whole SCRAM-SHA-256 verifier");
        }
        let script = format!(
            "BEGIN;\nSET LOCAL search_path = pg_catalog, pg_temp;\nCREATE ROLE {} LOGIN PASSWORD {};\n{}\nCOMMIT;",
            quote_ident(name),
            quote_literal(scram_verifier),
            mark_personal_login_sql(name),
        );
        self.computes
            .admin_sql(endpoint, crate::resync::SYSTEM_DATABASE, &script)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("making the personal login {name}: {e:#}"))
    }

    /// Creates a role (spec Part 3, U1): the name rules and the attribute and membership
    /// rules, a ready `normal` branch, then under the endpoint's lock the SQL existence check,
    /// the membership targets' check, the `pending` row, the role made in SQL as the owner, the
    /// row confirmed and the proxies told. A LOGIN role gets a generated password, which is
    /// never returned or logged; a NOLOGIN role has none.
    pub async fn create_role(&self, branch: BranchId, spec: &RoleSpec) -> RoleOutcome {
        self.create_role_tracked(branch, spec, &AtomicBool::new(false))
            .await
    }

    /// [`Self::create_role`], setting `stored` once the role's row is stored (a create cut
    /// short after that is `not_applied`; before it, nothing happened).
    async fn create_role_tracked(
        &self,
        branch: BranchId,
        spec: &RoleSpec,
        stored: &AtomicBool,
    ) -> RoleOutcome {
        let name = spec.name.as_str();
        if let Err(refusal) = check_name(name) {
            return refusal;
        }
        if let Err(refusal) = check_connection_limit(spec.connection_limit)
            .and_then(|()| check_members(name, &spec.member_of))
        {
            return refusal;
        }
        let (ep, database) = match self.role_target(branch).await {
            Ok(target) => target,
            Err(outcome) => return outcome,
        };
        let _lock = match self.metadata.lock_endpoint(&ep).await {
            Ok(lock) => lock,
            Err(e) => return failed(branch, "locking the endpoint", &e),
        };

        // A role made in SQL is never adopted: its password isn't ours to reveal.
        match self
            .computes
            .admin_sql(&ep, &database, &exists_sql(name))
            .await
        {
            Ok(out) if !out.trim().is_empty() => return exists(name),
            Ok(_) => {}
            Err(e) => {
                warn!(branch_id = %branch, endpoint_id = %ep, "checking whether role {name} exists: {e:#}");
                return RoleOutcome::NotReady;
            }
        }
        if let Err(outcome) = self
            .check_member_targets(&ep, &database, &spec.member_of)
            .await
        {
            return outcome;
        }

        let secret = spec.login.then(|| {
            let password = credentials::generate_password();
            let verifier = chelabase_neon::scram::verifier(&password);
            let sealed = seal_role_password(&self.key, &branch.to_string(), name, &password);
            (verifier, sealed)
        });
        let attributes = spec.attributes();
        let cap = self.roles.role_cap;
        let row = NewApiRole {
            name,
            owner_level: spec.owner_level,
            login: spec.login,
            attributes: &attributes,
            verifier: secret.as_ref().map(|(v, _)| v.as_str()),
            password_enc: secret.as_ref().map(|(_, s)| s.as_str()),
            change_id: uuid::Uuid::new_v4(),
        };
        match self.metadata.insert_api_role(&ep, &row, cap).await {
            Ok(InsertRoleOutcome::Inserted) => stored.store(true, Ordering::SeqCst),
            Ok(InsertRoleOutcome::Exists) => return exists(name),
            Ok(InsertRoleOutcome::CapReached) => {
                return RoleOutcome::Refused(
                    "role_cap_reached",
                    format!("the branch already has {cap} login roles, the most it may have"),
                );
            }
            Err(e) => return failed(branch, "storing the role", &e),
        }

        let wanted = WantedRole {
            name,
            owner_level: spec.owner_level,
            login: spec.login,
            attributes: &attributes,
        };
        let script = format!(
            "SET search_path = pg_catalog, pg_temp;\n{}",
            create_role_sql(OWNER_ROLE, &wanted, row.verifier)
        );
        if let Err(outcome) = self
            .apply_and_confirm(branch, &ep, &database, name, &script, Some(row.change_id))
            .await
        {
            return outcome;
        }
        info!(
            branch_id = %branch,
            endpoint_id = %ep,
            role = name,
            owner_level = spec.owner_level,
            login = spec.login,
            "role created"
        );
        // A grant may name it (or a tenant admin gets every role): the members' roles follow.
        self.member_role_requests.request(branch);
        RoleOutcome::Ok
    }

    /// Changes a role (U1): its LOGIN, owner-level, attributes or memberships. Refused for the
    /// owner, a reserved name, or a membership or attribute the rules refuse. Under the
    /// endpoint's lock, the role's catalog state is read; an API-made role's row takes the
    /// wanted state first (`pending`), then the SQL that differs from the catalog runs and the
    /// row is confirmed. A role made in SQL changes in SQL only (the database wins for it, and
    /// the resync records it), except that turning LOGIN on gives it a generated password,
    /// stored sealed so the Connection panel reveals it. Turning LOGIN off drops the password.
    pub async fn update_role(&self, branch: BranchId, name: &str, patch: &RolePatch) -> RoleOutcome {
        self.update_role_tracked(branch, name, patch, &AtomicBool::new(false))
            .await
    }

    /// [`Self::update_role`], setting `stored` once the row's change is stored.
    async fn update_role_tracked(
        &self,
        branch: BranchId,
        name: &str,
        patch: &RolePatch,
        stored: &AtomicBool,
    ) -> RoleOutcome {
        if let Err(refusal) = check_any_name(name) {
            return refusal;
        }
        if name == OWNER_ROLE || is_reserved_target(name, OWNER_ROLE) {
            return reserved(name);
        }
        let mut checks = Ok(());
        if let Some(limit) = patch.connection_limit {
            checks = checks.and_then(|()| check_connection_limit(limit));
        }
        if let Some(members) = &patch.member_of {
            checks = checks.and_then(|()| check_members(name, members));
        }
        if let Err(refusal) = checks {
            return refusal;
        }
        let (ep, database) = match self.role_target(branch).await {
            Ok(target) => target,
            Err(outcome) => return outcome,
        };
        let _lock = match self.metadata.lock_endpoint(&ep).await {
            Ok(lock) => lock,
            Err(e) => return failed(branch, "locking the endpoint", &e),
        };
        let row = match self.metadata.role_copies(&ep).await {
            Ok((rows, _)) => rows
                .into_iter()
                .find(|r| r.origin != RoleOrigin::Owner && r.name == name),
            Err(e) => return failed(branch, "reading the roles", &e),
        };
        let observed = match self
            .computes
            .admin_sql(&ep, &database, &role_read_sql(OWNER_ROLE, name))
            .await
            .map_err(|e| format!("{e:#}"))
            .and_then(|out| parse_observed(&out).map_err(|e| e.to_string()))
        {
            Ok(o) => o.roles.into_iter().find(|o| o.name == name),
            Err(e) => {
                warn!(branch_id = %branch, endpoint_id = %ep, role = name, "reading the role: {e}");
                return RoleOutcome::NotReady;
            }
        };
        let api_row = row.as_ref().filter(|r| r.origin == RoleOrigin::Api);
        // What the role is now (E32): the catalog's state, or an API row's unconfirmed one.
        let current = match current_role_state(row.as_ref(), observed.as_ref()) {
            Ok(current) => current,
            Err(outcome) => return outcome,
        };
        let (owner_level, login) = (current.owner_level, current.login);
        let mut attributes = current.attributes;
        // T14 M1: a stored group dropped in SQL since isn't wanted any more (the catalog's
        // state only lists groups that exist).
        let unchecked = patch.member_of.is_none()
            && api_row.is_some_and(|r| r.attributes_pending || observed.is_none());
        if unchecked && !attributes.member_of.is_empty() {
            let found = match self
                .membership_targets(&ep, &database, &attributes.member_of)
                .await
            {
                Ok(found) => found,
                Err(outcome) => return outcome,
            };
            attributes.member_of.retain(|g| found.contains_key(g));
        }
        if let Some(members) = &patch.member_of
            && let Err(outcome) = self.check_member_targets(&ep, &database, members).await
        {
            return outcome;
        }
        let wanted_attributes = patch.apply(&attributes);
        let wanted_login = patch.login.unwrap_or(login);
        let wanted_owner_level = patch.owner_level.unwrap_or(owner_level);
        // Turning LOGIN on gives the role a password, and a row without a verifier a new copy,
        // which the cap counts (I2: checked by the store, under the endpoint's lock).
        let secret = (wanted_login && !login).then(|| {
            let password = credentials::generate_password();
            let verifier = chelabase_neon::scram::verifier(&password);
            let sealed = seal_role_password(&self.key, &branch.to_string(), name, &password);
            (verifier, sealed)
        });
        let cap = self.roles.role_cap;
        let cap_reached = || {
            RoleOutcome::Refused(
                "role_cap_reached",
                format!("the branch already has {cap} login roles, the most it may have"),
            )
        };

        // The row first, so a change the compute misses is retried by the resync.
        let change_id = api_row.map(|_| uuid::Uuid::new_v4());
        let has_row = if let Some(change_id) = change_id {
            let change = ApiRoleChange {
                change_id,
                owner_level: wanted_owner_level,
                login: wanted_login,
                attributes: wanted_attributes.clone(),
                secret: match &secret {
                    Some((verifier, sealed)) => SecretChange::Set(verifier.clone(), sealed.clone()),
                    // Only turning LOGIN off drops the password; a role SQL made NOLOGIN keeps
                    // it (E32).
                    None if patch.login == Some(false) => SecretChange::Clear,
                    None => SecretChange::Keep,
                },
            };
            match self.metadata.update_api_role(&ep, name, &change, cap).await {
                Ok(RoleWriteOutcome::Done) => true,
                Ok(RoleWriteOutcome::NotFound) => return RoleOutcome::NotFound,
                Ok(RoleWriteOutcome::CapReached) => return cap_reached(),
                Err(e) => return failed(branch, "storing the role's change", &e),
            }
        } else if let Some((verifier, sealed)) = &secret {
            match self
                .metadata
                .store_role_secret(&ep, name, verifier, sealed, cap)
                .await
            {
                Ok(RoleWriteOutcome::Done) => true,
                Ok(RoleWriteOutcome::NotFound) => return RoleOutcome::NotFound,
                Ok(RoleWriteOutcome::CapReached) => return cap_reached(),
                Err(e) => return failed(branch, "storing the new password", &e),
            }
        } else {
            false
        };
        stored.store(has_row, Ordering::SeqCst);

        let Some(o) = observed else {
            // An API-made role the compute doesn't have yet: the resync makes it as stored.
            return RoleOutcome::NotApplied;
        };
        let wanted = WantedRole {
            name,
            owner_level: wanted_owner_level,
            login: wanted_login,
            attributes: &wanted_attributes,
        };
        let statements = update_statements(&wanted, &o, secret.as_ref().map(|(v, _)| v.as_str()), patch);
        let script = if statements.is_empty() {
            String::new()
        } else {
            format!(
                "SET search_path = pg_catalog, pg_temp;\n{}",
                statements.join("\n")
            )
        };
        if script.is_empty() && !has_row {
            return RoleOutcome::Ok;
        }
        if has_row {
            if let Err(outcome) = self
                .apply_and_confirm(branch, &ep, &database, name, &script, change_id)
                .await
            {
                return outcome;
            }
        } else {
            if let Err(e) = self.computes.admin_sql(&ep, &database, &script).await {
                warn!(branch_id = %branch, endpoint_id = %ep, role = name, "changing the role: {e:#}");
                return RoleOutcome::Failed(format!("changing the role {name}: {e:#}"));
            }
            self.publish_twice(&ep, &[name.to_string()]).await;
        }
        // A role made in SQL that lost its password has no copy any more.
        if !wanted_login
            && row.as_ref().is_some_and(|r| r.origin == RoleOrigin::Sql)
            && let Err(e) = self.metadata.delete_role_copy(&ep, name).await
        {
            warn!(branch_id = %branch, endpoint_id = %ep, role = name, "removing the row; the resync will: {e}");
        }
        info!(branch_id = %branch, endpoint_id = %ep, role = name, "role changed");
        // An owner-level or membership change can make the role unsafe for member roles (R16).
        self.member_role_requests.request(branch);
        RoleOutcome::Ok
    }

    /// I4 (for the merge's rehearsal too): every membership target exists, the owner holds
    /// ADMIN on it (so the owner can grant it), and it isn't itself a member, at any depth, of
    /// a reserved role (so no grant reaches one indirectly); otherwise
    /// `role_membership_not_allowed`. SQL only ([`membership_check_sql`] as `cloud_admin`, read
    /// only, no lock and no metadata), so it runs on any compute: a branch's or a rehearsal's.
    /// A compute that can't be asked is [`RoleOutcome::NotReady`]; an answer it can't read is
    /// [`RoleOutcome::Failed`].
    pub(crate) async fn check_member_targets(
        &self,
        ep: &str,
        database: &str,
        targets: &[String],
    ) -> Result<(), RoleOutcome> {
        if targets.is_empty() {
            return Ok(());
        }
        let found = self.membership_targets(ep, database, targets).await?;
        judge_member_targets(targets, &found)
    }

    /// The check's answer for `targets` on `ep`: each existing target and its flags.
    async fn membership_targets(
        &self,
        ep: &str,
        database: &str,
        targets: &[String],
    ) -> Result<std::collections::HashMap<String, (bool, bool)>, RoleOutcome> {
        let out = match self
            .computes
            .admin_sql(ep, database, &membership_check_sql(OWNER_ROLE, targets))
            .await
        {
            Ok(out) => out,
            Err(e) => {
                warn!(endpoint_id = %ep, "checking the membership targets: {e:#}");
                return Err(RoleOutcome::NotReady);
            }
        };
        parse_membership_check(&out).map_err(|e| {
            warn!(endpoint_id = %ep, "checking the membership targets: {e}");
            RoleOutcome::Failed(format!("checking the membership targets: {e}"))
        })
    }

    /// A new password for a login role; the owner's name takes the owner's reset. Any
    /// non-owner row works, one made in SQL too (it becomes revealable).
    pub async fn reset_role(&self, branch: BranchId, name: &str) -> RoleOutcome {
        if name == OWNER_ROLE {
            return match self.reset_password(branch).await {
                Ok(()) => RoleOutcome::Ok,
                Err(e) if e == RESET_NOT_APPLIED => RoleOutcome::NotApplied,
                Err(e)
                    if matches!(
                        e.as_str(),
                        "not ready" | "not found" | "credentials unavailable"
                    ) =>
                {
                    RoleOutcome::NotReady
                }
                Err(e) => RoleOutcome::Failed(e),
            };
        }
        if let Err(refusal) = check_any_name(name) {
            return refusal;
        }
        let (ep, database) = match self.role_target(branch).await {
            Ok(target) => target,
            Err(outcome) => return outcome,
        };
        let _lock = match self.metadata.lock_endpoint(&ep).await {
            Ok(lock) => lock,
            Err(e) => return failed(branch, "locking the endpoint", &e),
        };
        let password = credentials::generate_password();
        let verifier = chelabase_neon::scram::verifier(&password);
        let sealed = seal_role_password(&self.key, &branch.to_string(), name, &password);
        match self
            .metadata
            .replace_role_secret(&ep, name, &verifier, &sealed)
            .await
        {
            Ok(true) => {}
            Ok(false) => return RoleOutcome::NotFound,
            Err(e) => return failed(branch, "storing the new password", &e),
        }
        let sql = set_password_sql(name, &verifier);
        match self
            .apply_and_confirm(branch, &ep, &database, name, &sql, None)
            .await
        {
            Ok(()) => {}
            Err(RoleOutcome::NotApplied) => {
                // Gone from SQL meanwhile: the resync removes (or re-creates) its row.
                return match self
                    .computes
                    .admin_sql(&ep, &database, &exists_sql(name))
                    .await
                {
                    Ok(out) if out.trim().is_empty() => RoleOutcome::NotFound,
                    _ => RoleOutcome::NotApplied,
                };
            }
            Err(outcome) => return outcome,
        }
        info!(branch_id = %branch, endpoint_id = %ep, role = name, "login role password reset");
        RoleOutcome::Ok
    }

    /// Deletes a role (spec Part 3): refused for the owner, a reserved name, or while a live
    /// secret data key uses it; then under the endpoint's lock the role is dropped in SQL (its
    /// objects reassigned to the owner in every database) and its row deleted.
    pub async fn delete_role(&self, branch: BranchId, name: &str) -> RoleOutcome {
        if name == OWNER_ROLE {
            return reserved(name);
        }
        if let Err(refusal) = check_any_name(name) {
            return refusal;
        }
        if is_reserved_for(name, OWNER_ROLE) {
            return reserved(name);
        }
        let (ep, database) = match self.role_target(branch).await {
            Ok(target) => target,
            Err(outcome) => return outcome,
        };
        let _lock = match self.metadata.lock_endpoint(&ep).await {
            Ok(lock) => lock,
            Err(e) => return failed(branch, "locking the endpoint", &e),
        };
        match self.metadata.role_copies(&ep).await {
            Ok((rows, _))
                if rows
                    .iter()
                    .any(|r| r.origin != RoleOrigin::Owner && r.name == name) => {}
            Ok(_) => return RoleOutcome::NotFound,
            Err(e) => return failed(branch, "reading the roles", &e),
        }
        let keys: Vec<KeyRef> = match self.metadata.list_data_keys(branch).await {
            Ok(keys) => keys
                .into_iter()
                .filter(|k| k.kind == DataKeyKind::Secret && k.role == name)
                .map(|k| KeyRef {
                    id: k.id,
                    name: k.name,
                })
                .collect(),
            Err(e) => return failed(branch, "reading the data API keys", &e),
        };
        if !keys.is_empty() {
            return RoleOutcome::InUse(keys);
        }

        if let Err(e) = self.drop_role_sql(&ep, &database, name).await {
            warn!(branch_id = %branch, endpoint_id = %ep, role = name, "dropping the role; the row stays: {e:#}");
            return RoleOutcome::Failed(format!("dropping the role {name}: {e:#}"));
        }
        match self.metadata.delete_role_copy(&ep, name).await {
            // `false`: the row went meanwhile (a forwarded DROP ROLE); the role is gone either way.
            Ok(_) => {}
            Err(e) => return failed(branch, "deleting the role's row", &e),
        }
        self.publish_twice(&ep, &[name.to_string()]).await;
        info!(branch_id = %branch, endpoint_id = %ep, role = name, "login role deleted");
        self.member_role_requests.request(branch);
        RoleOutcome::Ok
    }

    /// E29: renames a role through the control plane (a merge's phase 0 calls it for a
    /// source role renamed in SQL, `RoleOp::Rename`). Both names must be ones Postgres can
    /// hold, and neither the owner, a reserved name nor any `chelabase_…` role
    /// (`reserved_role_name`); `to` must differ from `from`. On a ready `normal` branch, under
    /// the endpoint's lock: the role must exist and be the user's (an API row, or a role the
    /// owner administers; else [`RoleOutcome::NotFound`]), and `to` must be free in SQL and in
    /// the metadata (`role_exists`). Then the role's row moves to `to` (its sealed password
    /// resealed under the new name, its OID-bound data keys following, OID and timeline kept),
    /// the role is renamed in SQL with forwarding off (the DDL receiver never sees it), and the
    /// proxies drop both names' cached passwords. When the SQL rename answers an error, the
    /// catalog decides (F2 fix round I-2): the new name there and not the old means the rename
    /// committed and its answer was lost ([`RoleOutcome::Ok`]); the old name still there means
    /// the row and keys move back (best effort, the row's unreadable flag restored); neither
    /// name means the role went meanwhile and the move stays; both are
    /// [`RoleOutcome::Failed`]. A catalog that can't be read leaves the move as it is and is
    /// [`RoleOutcome::NotReady`]. An endpoint gone meanwhile is [`RoleOutcome::NotReady`].
    pub async fn rename_role(&self, branch: BranchId, from: &str, to: &str) -> RoleOutcome {
        if let Err(refusal) = check_rename(from, to) {
            return refusal;
        }
        let (ep, database) = match self.role_target(branch).await {
            Ok(target) => target,
            Err(outcome) => return outcome,
        };
        let _lock = match self.metadata.lock_endpoint(&ep).await {
            Ok(lock) => lock,
            Err(e) => return failed(branch, "locking the endpoint", &e),
        };
        let row = match self.metadata.role_copies(&ep).await {
            Ok((rows, _)) => rows
                .into_iter()
                .find(|r| r.origin != RoleOrigin::Owner && r.name == from),
            Err(e) => return failed(branch, "reading the roles", &e),
        };
        let observed = match self
            .computes
            .admin_sql(&ep, &database, &role_read_sql(OWNER_ROLE, from))
            .await
            .map_err(|e| format!("{e:#}"))
            .and_then(|out| parse_observed(&out).map_err(|e| e.to_string()))
        {
            Ok(o) => o.roles.into_iter().find(|o| o.name == from),
            Err(e) => {
                warn!(branch_id = %branch, endpoint_id = %ep, role = from, "reading the role: {e}");
                return RoleOutcome::NotReady;
            }
        };
        if !rename_is_ours(observed.as_ref(), row.as_ref()) {
            return RoleOutcome::NotFound;
        }
        match self
            .computes
            .admin_sql(&ep, &database, &exists_sql(to))
            .await
        {
            Ok(out) if !out.trim().is_empty() => return exists(to),
            Ok(_) => {}
            Err(e) => {
                warn!(branch_id = %branch, endpoint_id = %ep, "checking whether role {to} exists: {e:#}");
                return RoleOutcome::NotReady;
            }
        }
        let aad = branch.to_string();
        let sealed = row.as_ref().and_then(|r| r.password_enc.clone());
        let resealed = sealed.as_deref().and_then(|sealed| {
            match open_role_password(&self.key, &aad, from, sealed) {
                Ok(password) => Some(seal_role_password(&self.key, &aad, to, &password)),
                Err(e) => {
                    warn!(branch_id = %branch, role = from, "a renamed role's sealed password won't open ({e}); dropped");
                    None
                }
            }
        });
        let was_unreadable = match self
            .metadata
            .rename_role_copy(&ep, branch, from, to, resealed.as_deref(), None)
            .await
        {
            Ok(RenameCopyOutcome::Renamed { was_unreadable, .. }) => was_unreadable,
            Ok(RenameCopyOutcome::Taken) => return exists(to),
            // The endpoint went meanwhile (the branch is being deleted or switched).
            Ok(RenameCopyOutcome::NotFound) => return RoleOutcome::NotReady,
            Err(e) => return failed(branch, "moving the role's row", &e),
        };
        let sql = format!(
            "SET search_path = pg_catalog, pg_temp;\nALTER ROLE {} RENAME TO {};",
            quote_ident(from),
            quote_ident(to)
        );
        if let Err(e) = self.computes.admin_sql(&ep, &database, &sql).await {
            // The RENAME may have committed with its answer lost: the catalog decides.
            let names = self
                .computes
                .admin_sql(&ep, &database, &role_names_sql(&[from, to]))
                .await
                .map_err(|e| format!("{e:#}"))
                .and_then(|out| parse_role_names(&out));
            match names {
                Ok(names) if names.contains(to) && !names.contains(from) => {
                    info!(branch_id = %branch, endpoint_id = %ep, from, to, "the rename's answer was lost, but it committed: {e:#}");
                }
                Ok(names) if !names.contains(from) => {
                    // The role went meanwhile: a move back would have the resync make `from`
                    // again, so the row stays under `to`.
                    warn!(branch_id = %branch, endpoint_id = %ep, role = from, "renaming the role: {e:#}; neither name exists now, the row stays under {to}");
                    return RoleOutcome::Failed(format!("renaming the role {from}: {e:#}"));
                }
                Ok(_) => {
                    warn!(branch_id = %branch, endpoint_id = %ep, role = from, "renaming the role; moving its row back: {e:#}");
                    if let Err(back) = self
                        .metadata
                        .rename_role_copy(&ep, branch, to, from, sealed.as_deref(), Some(was_unreadable))
                        .await
                    {
                        warn!(branch_id = %branch, endpoint_id = %ep, role = from, "moving the row back; the resync finds the role under its old name: {back}");
                    }
                    return RoleOutcome::Failed(format!("renaming the role {from}: {e:#}"));
                }
                Err(read) => {
                    // Can't tell: the row stays under the new name, where a retried rename (or
                    // the resync, once the compute answers) finds it.
                    warn!(branch_id = %branch, endpoint_id = %ep, role = from, "renaming the role: {e:#}; reading the outcome: {read}");
                    return RoleOutcome::NotReady;
                }
            }
        }
        self.publish_twice(&ep, &[from.to_string(), to.to_string()])
            .await;
        info!(branch_id = %branch, endpoint_id = %ep, from, to, "role renamed");
        // A membership follows the role's OID, a grant its name: the members' roles follow.
        self.member_role_requests.request(branch);
        RoleOutcome::Ok
    }

    /// After a row's write: reads the version, runs `sql` (forwarding is off), confirms the
    /// row and tells the proxies. A failed `sql` is `NotApplied`: the row stays `pending` and
    /// the next resync applies it. The confirm clears `attributes_pending` by the write's
    /// `attributes_change` (an API create or PATCH) whatever else happened meanwhile; one that
    /// finds the version moved leaves `pending` to a resync it asks for at once (F2 fix round
    /// I-1). The role itself is done.
    async fn apply_and_confirm(
        &self,
        branch: BranchId,
        ep: &str,
        database: &str,
        name: &str,
        sql: &str,
        attributes_change: Option<uuid::Uuid>,
    ) -> Result<(), RoleOutcome> {
        let version = match self.metadata.role_copies(ep).await {
            Ok((_, version)) => version,
            Err(e) => {
                warn!(branch_id = %branch, endpoint_id = %ep, role = name, "stored but not applied; the resync will: reading the version: {e}");
                return Err(RoleOutcome::NotApplied);
            }
        };
        // Empty: nothing to run, the catalog has it already.
        if !sql.is_empty()
            && let Err(e) = self.computes.admin_sql(ep, database, sql).await
        {
            warn!(branch_id = %branch, endpoint_id = %ep, role = name, "stored but not applied; the resync will: {e:#}");
            return Err(RoleOutcome::NotApplied);
        }
        match self
            .metadata
            .confirm_role(ep, name, version, attributes_change)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                info!(branch_id = %branch, endpoint_id = %ep, role = name, "roles changed meanwhile; a resync clears pending");
                self.resync_requests.request(branch);
            }
            Err(e) => {
                warn!(branch_id = %branch, endpoint_id = %ep, role = name, "confirming the role; a resync clears pending: {e}");
                self.resync_requests.request(branch);
            }
        }
        self.publish_twice(ep, &[name.to_string()]).await;
        Ok(())
    }

    /// The role gone from SQL: no new logins, its sessions ended, what it owns moved to the
    /// owner (and its grants dropped) in every connectable database, then the role itself.
    /// A role already missing is done. Every step is safe to run again.
    async fn drop_role_sql(&self, ep: &str, database: &str, name: &str) -> anyhow::Result<()> {
        let exists = self
            .computes
            .admin_sql(ep, database, &exists_sql(name))
            .await
            .context("checking whether the role exists")?;
        if exists.trim().is_empty() {
            return Ok(());
        }
        // Listed and checked first: a database psql can't be given must not leave the role
        // locked out below.
        let databases = self
            .computes
            .admin_sql(ep, database, DATABASES_SQL)
            .await
            .context("listing the databases")?;
        let databases = database_names(&databases)?;
        let (ident, literal) = (quote_ident(name), quote_literal(name));
        self.computes
            .admin_sql(
                ep,
                database,
                &format!(
                    "SET search_path = pg_catalog, pg_temp; ALTER ROLE {ident} NOLOGIN; \
                     SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = {literal};"
                ),
            )
            .await
            .context("ending the role's sessions")?;
        for db in &databases {
            self.computes
                .admin_sql(
                    ep,
                    db,
                    &format!("REASSIGN OWNED BY {ident} TO {OWNER_ROLE}; DROP OWNED BY {ident};"),
                )
                .await
                .with_context(|| format!("moving the role's objects in {db}"))?;
        }
        self.computes
            .admin_sql(ep, database, &format!("DROP ROLE IF EXISTS {ident};"))
            .await
            .context("dropping the role")?;
        Ok(())
    }

    /// One role RPC: `payload` decoded for `subject`, handled within `deadline`, as the reply.
    /// A reset past its deadline may have stored its row (the resync applies it), so it answers
    /// `not_applied`, as does a create that stored its row; a create cut short before that,
    /// and a delete, answer `failed` (a retry is safe). `not_applied` says what was stored.
    pub async fn role_rpc_reply(
        &self,
        subject: &str,
        payload: &[u8],
        deadline: Duration,
    ) -> RoleReply {
        let bad = |e: &dyn std::fmt::Display| {
            role_reply(RoleOutcome::Failed(format!("bad request: {e}")))
        };
        let (branch, outcome) = match subject {
            SUBJECT_ROLE_CREATE => match serde_json::from_slice::<RoleCreateRequest>(payload) {
                Ok(r) => {
                    let stored = AtomicBool::new(false);
                    let outcome = timeout(
                        deadline,
                        self.create_role_tracked(r.branch_id, &RoleSpec::from(&r), &stored),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        if stored.load(Ordering::SeqCst) {
                            RoleOutcome::NotApplied
                        } else {
                            RoleOutcome::Failed(
                                "timed out before the role was stored; a retry is safe".into(),
                            )
                        }
                    });
                    (r.branch_id, outcome)
                }
                Err(e) => return bad(&e),
            },
            SUBJECT_ROLE_UPDATE => match serde_json::from_slice::<RoleUpdateRequest>(payload) {
                Ok(r) => {
                    let stored = AtomicBool::new(false);
                    let outcome = timeout(
                        deadline,
                        self.update_role_tracked(r.branch_id, &r.name, &RolePatch::from(&r), &stored),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        if stored.load(Ordering::SeqCst) {
                            RoleOutcome::NotApplied
                        } else {
                            RoleOutcome::Failed(
                                "timed out before the change was stored; a retry is safe".into(),
                            )
                        }
                    });
                    (r.branch_id, outcome)
                }
                Err(e) => return bad(&e),
            },
            SUBJECT_ROLE_RESET | SUBJECT_ROLE_DELETE => {
                let r = match serde_json::from_slice::<RoleRequest>(payload) {
                    Ok(r) => r,
                    Err(e) => return bad(&e),
                };
                let outcome = if subject == SUBJECT_ROLE_RESET {
                    timeout(deadline, self.reset_role(r.branch_id, &r.name))
                        .await
                        .unwrap_or(RoleOutcome::NotApplied)
                } else {
                    timeout(deadline, self.delete_role(r.branch_id, &r.name))
                        .await
                        .unwrap_or_else(|_| {
                            RoleOutcome::Failed("timed out; a retry is safe".into())
                        })
                };
                (r.branch_id, outcome)
            }
            other => return bad(&format!("unknown subject {other}")),
        };
        if !matches!(outcome, RoleOutcome::Ok) {
            info!(branch_id = %branch, subject, ?outcome, "role request not done");
        }
        let not_applied = matches!(outcome, RoleOutcome::NotApplied);
        let mut reply = role_reply(outcome);
        if not_applied {
            let message = match subject {
                SUBJECT_ROLE_CREATE => ROLE_CREATE_NOT_APPLIED,
                SUBJECT_ROLE_DELETE => ROLE_DELETE_NOT_APPLIED,
                SUBJECT_ROLE_UPDATE => ROLE_UPDATE_NOT_APPLIED,
                _ => RESET_NOT_APPLIED,
            };
            reply.error = Some(message.into());
        }
        reply
    }

    /// The endpoint of a ready `normal` branch and its recorded main database; otherwise the
    /// outcome to answer.
    async fn role_target(&self, branch: BranchId) -> Result<(String, String), RoleOutcome> {
        let ep = self.role_endpoint(branch).await?;
        match self.metadata.neon_databases(&ep).await {
            Ok(dbs) => match dbs.into_iter().next() {
                Some(db) => Ok((ep, db.name)),
                None => Err(RoleOutcome::NotReady),
            },
            Err(e) => Err(failed(branch, "reading the endpoint's database", &e)),
        }
    }

    /// The endpoint of a ready `normal` branch; otherwise the outcome to answer.
    async fn role_endpoint(&self, branch: BranchId) -> Result<String, RoleOutcome> {
        let record = self
            .metadata
            .get_branch(branch)
            .await
            .map_err(|e| failed(branch, "reading the branch", &e))?;
        record
            .filter(|b| b.status == BranchStatus::Ready && b.kind == BranchKind::Normal)
            .and_then(|b| b.endpoint_id)
            .ok_or(RoleOutcome::NotReady)
    }
}

fn exists(name: &str) -> RoleOutcome {
    RoleOutcome::Refused("role_exists", format!("a role named {name} already exists"))
}

/// Logs `err` and answers `Failed`.
fn failed(branch: BranchId, what: &str, err: &dyn std::fmt::Display) -> RoleOutcome {
    warn!(branch_id = %branch, "{what}: {err}");
    RoleOutcome::Failed(format!("{what}: {err}"))
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::RoleLevel;
    use chelabase_nats::RESET_NOT_APPLIED;
    use chelabase_secrets::open_role_password;
    use uuid::Uuid;

    use super::*;
    use crate::OWNER_ROLE;
    use crate::fakes::{FakeLogin, FakeWorker, Journal, entries, position, ready_database, worker};

    /// The existence check, as the fake backend journals it.
    const EXISTS_SQL: &str =
        "SET search_path = pg_catalog, pg_temp; SELECT 1 FROM pg_catalog.pg_roles WHERE rolname";

    // Fix round 1 (I3): the check runs on the superuser session; an owner who set the
    // database's search_path and made `public.=` must not have it run there.
    #[test]
    fn exists_sql_pins_the_search_path() {
        assert_eq!(
            exists_sql("app"),
            "SET search_path = pg_catalog, pg_temp; SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'app'"
        );
    }
    /// The first line of the create script.
    const SCRIPT: &str = "SET search_path = pg_catalog, pg_temp;\nSET ROLE \"postgres\";\n";

    fn pos(journal: &Journal, prefix: &str) -> usize {
        position(journal, prefix)
            .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
    }

    fn count(journal: &Journal, entry: &str) -> usize {
        entries(journal).iter().filter(|e| *e == entry).count()
    }

    /// Main of a ready database: `(branch, endpoint, database name)`; the journal starts empty.
    async fn ready(w: &FakeWorker, journal: &Journal) -> (BranchId, String, String) {
        let id = ready_database(w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.unwrap();
        let database = w.metadata.state().endpoints[&ep].databases[0].name.clone();
        journal.lock().unwrap().clear();
        w.computes.sql.lock().unwrap().clear();
        (main.id, ep, database)
    }

    /// Every SQL text that reached the compute.
    fn sql(w: &FakeWorker) -> Vec<String> {
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, s)| s.clone())
            .collect()
    }

    /// `(verifier, sealed password, pending)` of a non-owner row.
    fn login(
        w: &FakeWorker,
        ep: &str,
        name: &str,
    ) -> Option<(Option<String>, Option<String>, bool)> {
        let state = w.metadata.state();
        let endpoint = &state.endpoints[ep];
        let row = endpoint.logins.get(name)?;
        Some((
            endpoint.verifier(name),
            row.password_enc.clone(),
            row.pending,
        ))
    }

    /// A row made in SQL, as the receiver or the resync records it.
    fn sql_row(w: &FakeWorker, ep: &str, name: &str, verifier: &str) {
        let mut state = w.metadata.state();
        let endpoint = state.endpoints.get_mut(ep).unwrap();
        endpoint.set_verifier(name, Some(verifier));
        endpoint.logins.insert(
            name.into(),
            FakeLogin {
                origin: RoleOrigin::Sql,
                password_enc: None,
                pending: false,
                login: true,
                level: RoleLevel::Plain,
                api_owner_level: false,
                attributes: chelabase_metadata::RoleAttributes::default(),
                attributes_pending: false,
                attributes_change_id: None,
            },
        );
    }

    fn owner_secret(w: &FakeWorker, branch: BranchId, ep: &str) -> (String, String) {
        let sealed = w.metadata.branch(branch).owner_password_enc.unwrap();
        let state = w.metadata.state();
        (state.endpoints[ep].verifier(OWNER_ROLE).unwrap(), sealed)
    }

    fn refused(code: &'static str) -> impl Fn(&RoleOutcome) -> bool {
        move |o| matches!(o, RoleOutcome::Refused(c, _) if *c == code)
    }

    // --- Create --------------------------------------------------------------------------

    #[tokio::test]
    async fn create_runs_as_the_owner_in_one_script() {
        let (w, journal) = worker();
        let (branch, ep, db) = ready(&w, &journal).await;
        let asked = w.member_role_requests.sent().len();

        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);
        assert_eq!(w.member_role_requests.sent().len(), asked + 1, "a member-role reconcile is asked for");

        let (verifier, sealed, pending) = login(&w, &ep, "app").unwrap();
        let verifier = verifier.unwrap();
        assert!(!pending, "confirmed");
        let password =
            open_role_password(&w.key, &branch.to_string(), "app", &sealed.unwrap()).unwrap();
        assert!(chelabase_neon::scram::verifier(&password).starts_with("SCRAM-SHA-256$"));
        assert_eq!(w.member_role_requests.sent().last(), Some(&branch));
        let sent = sql(&w);
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(
            sent[0],
            "SET search_path = pg_catalog, pg_temp; SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'app'"
        );
        assert_eq!(
            sent[1],
            format!("{SCRIPT}CREATE ROLE \"app\" LOGIN PASSWORD '{verifier}';\nRESET ROLE;")
        );
        let order = [
            pos(&journal, "metadata.lock_endpoint"),
            pos(
                &journal,
                &format!("compute.sql {ep} {db} {EXISTS_SQL} = 'app'"),
            ),
            pos(&journal, "metadata.insert_api_role"),
            // The create script (its first line is the pin alone; the check's goes on).
            entries(&journal)
                .iter()
                .position(|e| *e == format!("compute.sql {ep} {db} SET search_path = pg_catalog, pg_temp;"))
                .expect("the create script ran"),
            pos(&journal, "metadata.confirm_role"),
            pos(&journal, &format!("cache.password_updated {ep} app")),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        // No spec push: the compute has the role already.
        assert!(w.pusher.pushed.lock().unwrap().is_empty());
        let state = w.metadata.state();
        let row = &state.endpoints[&ep].logins["app"];
        assert_eq!(
            (row.origin, row.level, row.api_owner_level),
            (RoleOrigin::Api, RoleLevel::Plain, false)
        );
    }

    #[tokio::test]
    async fn create_owner_level_grants_neon_superuser() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;

        assert_eq!(w.create_role(branch, &RoleSpec::login("admin", true)).await, RoleOutcome::Ok);

        let script = sql(&w).pop().unwrap();
        let verifier = login(&w, &ep, "admin").unwrap().0.unwrap();
        // The owner's attributes (N1), and the membership as cloud_admin.
        assert_eq!(
            script,
            format!(
                "{SCRIPT}CREATE ROLE \"admin\" LOGIN CREATEROLE CREATEDB BYPASSRLS REPLICATION \
                 PASSWORD '{verifier}';\nRESET ROLE;\nGRANT neon_superuser TO \"admin\";"
            )
        );
        let state = w.metadata.state();
        let row = &state.endpoints[&ep].logins["admin"];
        assert_eq!(
            (row.level, row.api_owner_level),
            (RoleLevel::OwnerLevel, true)
        );
    }

    #[tokio::test]
    async fn create_refuses_bad_and_reserved_names_without_sql() {
        let (w, journal) = worker();
        let (branch, _ep, _db) = ready(&w, &journal).await;

        for (name, code) in [
            ("Bad-Name", "invalid_role_name"),
            ("My Role", "invalid_role_name"),
            ("", "invalid_role_name"),
            ("postgres", "reserved_role_name"),
            ("authenticator", "reserved_role_name"),
            ("pg_x", "reserved_role_name"),
        ] {
            let outcome = w.create_role(branch, &RoleSpec::login(name, false)).await;
            assert!(refused(code)(&outcome), "{name:?}: {outcome:?}");
        }
        assert!(position(&journal, "compute.sql").is_none());
        assert!(position(&journal, "metadata.lock_endpoint").is_none());
    }

    #[tokio::test]
    async fn create_conflicts_on_an_existing_sql_role() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        w.computes.answer(EXISTS_SQL, "1\n");

        let outcome = w.create_role(branch, &RoleSpec::login("app", false)).await;

        assert!(refused("role_exists")(&outcome), "{outcome:?}");
        assert!(position(&journal, "metadata.insert_api_role").is_none());
        assert!(login(&w, &ep, "app").is_none(), "nothing adopted");
        assert_eq!(sql(&w).len(), 1);

        // A row already there (made in SQL, recorded by the receiver) is a conflict too.
        w.computes.answers.lock().unwrap().clear();
        sql_row(&w, &ep, "reporter", "SCRAM-SHA-256$x");
        let outcome = w.create_role(branch, &RoleSpec::login("reporter", false)).await;
        assert!(refused("role_exists")(&outcome), "{outcome:?}");
        assert_eq!(login(&w, &ep, "reporter").unwrap().1, None);
    }

    #[tokio::test]
    async fn create_at_the_cap_is_refused() {
        let (mut w, journal) = worker();
        w.roles.role_cap = 2;
        let (branch, ep, _db) = ready(&w, &journal).await;
        // A role made in SQL counts.
        sql_row(&w, &ep, "reporter", "SCRAM-SHA-256$x");
        assert_eq!(w.create_role(branch, &RoleSpec::login("a", true)).await, RoleOutcome::Ok);

        let outcome = w.create_role(branch, &RoleSpec::login("b", true)).await;

        assert!(
            matches!(&outcome, RoleOutcome::Refused("role_cap_reached", m) if m.contains('2')),
            "{outcome:?}"
        );
        assert!(login(&w, &ep, "b").is_none());
        assert!(!sql(&w).iter().any(|s| s.contains("CREATE ROLE \"b\"")));
    }

    #[tokio::test]
    async fn create_on_a_not_ready_branch_is_not_ready() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;

        assert_eq!(
            w.create_role(BranchId::new(), &RoleSpec::login("app", false)).await,
            RoleOutcome::NotReady
        );
        let pending = w
            .metadata
            .insert_database("p", chelabase_metadata::DatabaseStatus::Pending);
        assert_eq!(
            w.create_role(BranchId::from(pending), &RoleSpec::login("app", false)).await,
            RoleOutcome::NotReady
        );
        assert!(position(&journal, "metadata.lock_endpoint").is_none());

        // The compute doesn't answer the existence check.
        *w.computes.sql_failures_left.lock().unwrap() = 1;
        assert_eq!(
            w.create_role(branch, &RoleSpec::login("app", false)).await,
            RoleOutcome::NotReady
        );
        assert!(login(&w, &ep, "app").is_none());

        w.metadata.state().fail = Some("get_branch");
        assert!(matches!(
            w.create_role(branch, &RoleSpec::login("app", false)).await,
            RoleOutcome::Failed(_)
        ));
    }

    #[tokio::test]
    async fn create_sql_failure_is_not_applied_and_pending() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("CREATE ROLE".into(), "connection reset".into()));

        assert_eq!(
            w.create_role(branch, &RoleSpec::login("app", false)).await,
            RoleOutcome::NotApplied
        );

        let (verifier, sealed, pending) = login(&w, &ep, "app").unwrap();
        assert!(pending && verifier.is_some() && sealed.is_some());
        assert!(position(&journal, "metadata.confirm_role").is_none());
        assert!(position(&journal, "cache.password_updated").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn create_publishes_twice() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;

        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);

        let entry = format!("cache.password_updated {ep} app");
        assert_eq!(count(&journal, &entry), 1);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(count(&journal, &entry), 2, "{:?}", entries(&journal));
    }

    #[tokio::test]
    async fn a_publish_failure_doesnt_fail_the_create() {
        let (w, journal) = worker();
        let (branch, _ep, _db) = ready(&w, &journal).await;
        *w.cache.fail.lock().unwrap() = Some("valkey down".into());
        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);
        *w.cache.fail.lock().unwrap() = None;
        *w.cache.receivers.lock().unwrap() = 0;
        assert_eq!(w.create_role(branch, &RoleSpec::login("other", false)).await, RoleOutcome::Ok);
    }

    // --- Reset ---------------------------------------------------------------------------

    #[tokio::test]
    async fn reset_rotates_only_that_role() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);
        let owner_before = owner_secret(&w, branch, &ep);
        let (old_verifier, old_sealed, _) = login(&w, &ep, "app").unwrap();
        journal.lock().unwrap().clear();
        w.computes.sql.lock().unwrap().clear();

        assert_eq!(w.reset_role(branch, "app").await, RoleOutcome::Ok);

        assert_eq!(owner_secret(&w, branch, &ep), owner_before);
        let (verifier, sealed, pending) = login(&w, &ep, "app").unwrap();
        let verifier = verifier.unwrap();
        assert_ne!(Some(&verifier), old_verifier.as_ref());
        assert_ne!(sealed, old_sealed);
        assert!(!pending);
        assert_eq!(
            sql(&w),
            [format!("ALTER ROLE \"app\" PASSWORD '{verifier}';")]
        );
        let order = [
            pos(&journal, "metadata.lock_endpoint"),
            pos(&journal, "metadata.replace_role_secret"),
            pos(&journal, &format!("compute.sql {ep} app ALTER ROLE")),
            pos(&journal, "metadata.confirm_role"),
            pos(&journal, &format!("cache.password_updated {ep} app")),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        assert!(w.pusher.pushed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reset_of_an_sql_role_makes_it_revealable() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        sql_row(&w, &ep, "My \"Role\"", "SCRAM-SHA-256$old");

        assert_eq!(w.reset_role(branch, "My \"Role\"").await, RoleOutcome::Ok);

        let (verifier, sealed, pending) = login(&w, &ep, "My \"Role\"").unwrap();
        assert!(!pending);
        let password =
            open_role_password(&w.key, &branch.to_string(), "My \"Role\"", &sealed.unwrap())
                .unwrap();
        let verifier = verifier.unwrap();
        assert_ne!(verifier, "SCRAM-SHA-256$old");
        let params = verifier.split('$').nth(1).unwrap();
        let (iterations, salt) = params.split_once(':').unwrap();
        use base64::Engine;
        let salt = base64::engine::general_purpose::STANDARD
            .decode(salt)
            .unwrap();
        assert_eq!(
            chelabase_neon::scram::verifier_with_salt(&password, &salt, iterations.parse().unwrap()),
            verifier
        );
        assert_eq!(
            sql(&w),
            [format!(
                "ALTER ROLE \"My \"\"Role\"\"\" PASSWORD '{verifier}';"
            )]
        );
        assert_eq!(
            w.metadata.state().endpoints[&ep].logins["My \"Role\""].origin,
            RoleOrigin::Sql
        );
    }

    #[tokio::test]
    async fn reset_refusals() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;

        assert_eq!(w.reset_role(branch, "ghost").await, RoleOutcome::NotFound);
        assert_eq!(
            w.reset_role(branch, "authenticator").await,
            RoleOutcome::NotFound
        );
        assert!(refused("invalid_role_name")(
            &w.reset_role(branch, "").await
        ));
        assert!(refused("invalid_role_name")(
            &w.reset_role(branch, &"a".repeat(64)).await
        ));
        assert_eq!(
            w.reset_role(BranchId::new(), "app").await,
            RoleOutcome::NotReady
        );
        assert!(sql(&w).is_empty(), "{:?}", sql(&w));

        // The role is gone from SQL: not found, and the stored password stays for the resync.
        sql_row(&w, &ep, "dropped", "SCRAM-SHA-256$x");
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "ALTER ROLE".into(),
            "role \"dropped\" does not exist".into(),
        ));
        assert_eq!(w.reset_role(branch, "dropped").await, RoleOutcome::NotFound);

        // Still there, but the compute failed: not applied, pending.
        w.computes.answer(EXISTS_SQL, "1\n");
        assert_eq!(
            w.reset_role(branch, "dropped").await,
            RoleOutcome::NotApplied
        );
        assert!(login(&w, &ep, "dropped").unwrap().2);
    }

    #[tokio::test]
    async fn reset_of_the_owner_name_uses_the_owner_reset() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        let before = owner_secret(&w, branch, &ep);

        assert_eq!(w.reset_role(branch, OWNER_ROLE).await, RoleOutcome::Ok);

        assert!(position(&journal, "metadata.replace_owner_secret").is_some());
        assert!(position(&journal, "metadata.replace_role_secret").is_none());
        let after = owner_secret(&w, branch, &ep);
        assert_ne!(after.0, before.0);
        assert_ne!(after.1, before.1);

        *w.pusher.fail.lock().unwrap() = Some("compute_ctl said 500".into());
        assert_eq!(
            w.reset_role(branch, OWNER_ROLE).await,
            RoleOutcome::NotApplied
        );
        assert_eq!(
            w.reset_role(BranchId::new(), OWNER_ROLE).await,
            RoleOutcome::NotReady
        );
    }

    #[test]
    fn role_reply_codes() {
        let key = KeyRef {
            id: Uuid::nil(),
            name: "k".into(),
        };
        let ok = role_reply(RoleOutcome::Ok);
        assert!(ok.ok);
        assert_eq!((ok.code, ok.error), (None, None));
        let cases = [
            (
                RoleOutcome::Refused("invalid_role_name", "bad".into()),
                "invalid_role_name",
            ),
            (
                RoleOutcome::Refused("role_cap_reached", "full".into()),
                "role_cap_reached",
            ),
            (RoleOutcome::InUse(vec![key.clone()]), "role_in_use"),
            (RoleOutcome::NotReady, "branch_not_ready"),
            (RoleOutcome::NotFound, "not_found"),
            (RoleOutcome::NotApplied, "not_applied"),
            (RoleOutcome::Failed("boom".into()), "failed"),
        ];
        for (outcome, code) in cases {
            let in_use = matches!(outcome, RoleOutcome::InUse(_));
            let reply = role_reply(outcome);
            assert!(!reply.ok, "{code}");
            assert_eq!(reply.code.as_deref(), Some(code));
            assert!(reply.error.is_some(), "{code}");
            assert_eq!(
                reply.keys,
                if in_use { vec![key.clone()] } else { vec![] },
                "{code}"
            );
        }
        assert_eq!(
            role_reply(RoleOutcome::NotApplied).error.as_deref(),
            Some(RESET_NOT_APPLIED)
        );
    }

    // --- Delete --------------------------------------------------------------------------

    const EXISTS_APP: &str = "SET search_path = pg_catalog, pg_temp; SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'app'";
    const TERMINATE: &str = "SET search_path = pg_catalog, pg_temp; ALTER ROLE \"app\" NOLOGIN; \
        SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = 'app';";
    const DATABASES: &str = "SET search_path = pg_catalog, pg_temp; SELECT \
        encode(convert_to(datname, 'UTF8'), 'hex') FROM pg_database WHERE datallowconn AND NOT \
        datistemplate ORDER BY datname;";
    const REASSIGN: &str = "REASSIGN OWNED BY \"app\" TO postgres; DROP OWNED BY \"app\";";
    const DROP: &str = "DROP ROLE IF EXISTS \"app\";";

    /// The databases as the listing prints them: one hex-encoded name per line.
    fn databases(names: &[&str]) -> String {
        names
            .iter()
            .map(|n| format!("{}\n", hex::encode(n)))
            .collect()
    }

    /// `(database, sql)` of every SQL text that reached the compute.
    fn sql_on(w: &FakeWorker) -> Vec<(String, String)> {
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .map(|(_, db, s)| (db.clone(), s.clone()))
            .collect()
    }

    /// `app` created (plain) on a ready branch, and the journal and SQL log emptied.
    async fn with_app(w: &FakeWorker, journal: &Journal) -> (BranchId, String, String) {
        let (branch, ep, db) = ready(w, journal).await;
        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);
        journal.lock().unwrap().clear();
        w.computes.sql.lock().unwrap().clear();
        (branch, ep, db)
    }

    fn secret(branch: BranchId, role: &str, name: &str) -> chelabase_metadata::NewDataKey {
        chelabase_metadata::NewDataKey {
            id: Uuid::new_v4(),
            branch_id: branch,
            kind: chelabase_types::data_key::DataKeyKind::Secret,
            role: role.to_string(),
            name: name.to_string(),
            prefix: "sk_test".to_string(),
            sha256: [0; 32],
            sealed: None,
            databases: None,
            role_oid: None,
            role_timeline: None,
            role_bypass_rls: false,
        }
    }

    #[tokio::test]
    async fn delete_runs_in_order_across_every_database() {
        let (w, journal) = worker();
        let (branch, ep, db) = with_app(&w, &journal).await;
        w.computes.answer(EXISTS_APP, "1\n");
        w.computes
            .answer("FROM pg_database", &databases(&["app", "other"]));
        let asked = w.member_role_requests.sent().len();

        assert_eq!(w.delete_role(branch, "app").await, RoleOutcome::Ok);
        assert_eq!(w.member_role_requests.sent().len(), asked + 1, "a member-role reconcile is asked for");
        assert_eq!(w.member_role_requests.sent().last(), Some(&branch));

        let on = |d: &str, s: &str| (d.to_string(), s.to_string());
        assert_eq!(
            sql_on(&w),
            [
                on(&db, EXISTS_APP),
                on(&db, DATABASES),
                on(&db, TERMINATE),
                on("app", REASSIGN),
                on("other", REASSIGN),
                on(&db, DROP),
            ]
        );
        let order = [
            pos(&journal, "metadata.lock_endpoint"),
            pos(&journal, &format!("compute.sql {ep} {db} DROP ROLE")),
            pos(&journal, "metadata.delete_role_copy"),
            pos(&journal, &format!("cache.password_updated {ep} app")),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        assert!(login(&w, &ep, "app").is_none());
        assert!(w.pusher.pushed.lock().unwrap().is_empty());
        assert_eq!(
            count(&journal, &format!("cache.password_updated_later {ep} app")),
            1
        );
    }

    #[tokio::test]
    async fn delete_accepts_quoted_names() {
        let (w, journal) = worker();
        let (branch, ep, db) = ready(&w, &journal).await;
        let name = "My \"Role\"";
        sql_row(&w, &ep, name, "SCRAM-SHA-256$x");
        w.computes.answer(
            "SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'My \"Role\"'",
            "1\n",
        );
        w.computes
            .answer("FROM pg_database", &databases(&["app", "my-app"]));

        assert_eq!(w.delete_role(branch, name).await, RoleOutcome::Ok);

        let ident = "\"My \"\"Role\"\"\"";
        let on = |d: &str, s: String| (d.to_string(), s);
        let reassign = format!("REASSIGN OWNED BY {ident} TO postgres; DROP OWNED BY {ident};");
        assert_eq!(
            sql_on(&w)[2..],
            [
                on(
                    &db,
                    format!(
                        "SET search_path = pg_catalog, pg_temp; ALTER ROLE {ident} NOLOGIN; \
                         SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = \
                         'My \"Role\"';"
                    )
                ),
                on("app", reassign.clone()),
                on("my-app", reassign),
                on(&db, format!("DROP ROLE IF EXISTS {ident};")),
            ]
        );
        assert!(login(&w, &ep, name).is_none());
        // The hyphenated database reached psql as a quoted conninfo.
        assert_eq!(
            chelabase_neon::backend::admin_psql_command("my-app")
                .unwrap()
                .iter()
                .filter(|a| a.as_str() == "dbname='my-app'")
                .count(),
            1
        );
    }

    // T4: the row gone between the check and the delete (a forwarded DROP ROLE removed it):
    // the role is gone either way, so the delete succeeded.
    #[tokio::test]
    async fn delete_whose_row_vanished_meanwhile_succeeds() {
        let (w, journal) = worker();
        let (branch, ep, _db) = with_app(&w, &journal).await;
        w.metadata.state().role_row_vanishes = true;

        assert_eq!(w.delete_role(branch, "app").await, RoleOutcome::Ok);
        assert!(!w.metadata.state().endpoints[&ep].logins.contains_key("app"));
    }

    #[tokio::test]
    async fn delete_refuses_the_owner() {
        let (w, journal) = worker();
        let (branch, _ep, _db) = with_app(&w, &journal).await;

        for (name, code) in [
            (OWNER_ROLE, "reserved_role_name"),
            ("authenticator", "reserved_role_name"),
            ("pg_x", "reserved_role_name"),
            ("chelabase_x", "reserved_role_name"),
            ("cb_m_x", "reserved_role_name"),
            ("CB_M_X", "reserved_role_name"),
            ("", "invalid_role_name"),
        ] {
            let outcome = w.delete_role(branch, name).await;
            assert!(refused(code)(&outcome), "{name:?}: {outcome:?}");
        }
        assert!(position(&journal, "metadata.lock_endpoint").is_none());
        assert_eq!(w.delete_role(branch, "ghost").await, RoleOutcome::NotFound);
        assert_eq!(
            w.delete_role(BranchId::new(), "app").await,
            RoleOutcome::NotReady
        );
        assert!(sql(&w).is_empty(), "{:?}", sql(&w));
    }

    #[tokio::test]
    async fn delete_refused_while_a_live_secret_key_uses_the_role() {
        let (w, journal) = worker();
        let (branch, ep, _db) = with_app(&w, &journal).await;
        let live = secret(branch, "app", "backend");
        let revoked = secret(branch, "app", "old");
        let other = secret(branch, "reporting", "reports");
        {
            let mut state = w.metadata.state();
            state
                .data_keys
                .extend([live.clone(), revoked.clone(), other]);
            state.revoked_keys.insert(revoked.id);
        }

        assert_eq!(
            w.delete_role(branch, "app").await,
            RoleOutcome::InUse(vec![KeyRef {
                id: live.id,
                name: "backend".into()
            }])
        );
        assert!(sql(&w).is_empty(), "{:?}", sql(&w));
        assert!(login(&w, &ep, "app").is_some());

        w.metadata.state().revoked_keys.insert(live.id);
        assert_eq!(w.delete_role(branch, "app").await, RoleOutcome::Ok);
        assert!(login(&w, &ep, "app").is_none());
    }

    #[tokio::test]
    async fn delete_failure_keeps_the_row() {
        let (w, journal) = worker();
        let (branch, ep, _db) = with_app(&w, &journal).await;
        w.computes.answer(EXISTS_APP, "1\n");
        w.computes.answer("FROM pg_database", &databases(&["app"]));
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("REASSIGN".into(), "objects depend on it".into()));

        let outcome = w.delete_role(branch, "app").await;

        assert!(matches!(outcome, RoleOutcome::Failed(_)), "{outcome:?}");
        assert!(login(&w, &ep, "app").is_some());
        assert!(position(&journal, "metadata.delete_role_copy").is_none());
        assert!(!sql(&w).iter().any(|s| s == DROP));

        *w.computes.fail_sql_containing.lock().unwrap() = None;
        assert_eq!(w.delete_role(branch, "app").await, RoleOutcome::Ok);
        assert!(login(&w, &ep, "app").is_none());
    }

    #[tokio::test]
    async fn delete_refuses_an_unpassable_database_before_touching_the_role() {
        let (w, journal) = worker();
        let (branch, ep, _db) = with_app(&w, &journal).await;
        let long = "d".repeat(64);
        w.computes.answer(EXISTS_APP, "1\n");
        w.computes
            .answer("FROM pg_database", &databases(&["app", long.as_str()]));

        let outcome = w.delete_role(branch, "app").await;

        assert!(
            matches!(outcome, RoleOutcome::Failed(ref m) if m.contains(&long)),
            "{outcome:?}"
        );
        assert!(login(&w, &ep, "app").is_some());
        assert_eq!(sql(&w), [EXISTS_APP, DATABASES]);
    }

    #[tokio::test]
    async fn delete_of_a_role_missing_in_sql_still_removes_the_row() {
        let (w, journal) = worker();
        let (branch, ep, _db) = with_app(&w, &journal).await;

        assert_eq!(w.delete_role(branch, "app").await, RoleOutcome::Ok);

        assert_eq!(sql(&w), [EXISTS_APP]);
        assert!(login(&w, &ep, "app").is_none());
        assert!(position(&journal, &format!("cache.password_updated {ep} app")).is_some());
    }

    // --- RPCs ----------------------------------------------------------------------------

    #[tokio::test]
    async fn role_rpcs_reply() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        let deadline = Duration::from_secs(5);
        let request = |name: &str| {
            serde_json::to_vec(&RoleRequest {
                branch_id: branch,
                name: name.into(),
            })
            .unwrap()
        };
        let create = serde_json::to_vec(&RoleCreateRequest::login(branch, "app", true))
        .unwrap();

        let reply = w
            .role_rpc_reply(SUBJECT_ROLE_CREATE, &create, deadline)
            .await;
        assert!(reply.ok, "{reply:?}");
        assert!(sql(&w).last().unwrap().contains("GRANT neon_superuser"));
        let (_, sealed, _) = login(&w, &ep, "app").unwrap();
        let reply = w
            .role_rpc_reply(SUBJECT_ROLE_RESET, &request("app"), deadline)
            .await;
        assert!(reply.ok, "{reply:?}");
        assert_ne!(login(&w, &ep, "app").unwrap().1, sealed);
        let reply = w
            .role_rpc_reply(SUBJECT_ROLE_DELETE, &request("app"), deadline)
            .await;
        assert!(reply.ok, "{reply:?}");
        assert!(login(&w, &ep, "app").is_none());

        let reply = w
            .role_rpc_reply(SUBJECT_ROLE_DELETE, &request("app"), deadline)
            .await;
        assert_eq!(reply.code.as_deref(), Some("not_found"));
        let reply = w
            .role_rpc_reply(SUBJECT_ROLE_CREATE, &create, deadline)
            .await;
        assert!(reply.ok, "{reply:?}");
        let reply = w
            .role_rpc_reply(SUBJECT_ROLE_CREATE, &create, deadline)
            .await;
        assert_eq!(reply.code.as_deref(), Some("role_exists"), "{reply:?}");

        for (subject, payload) in [
            (SUBJECT_ROLE_CREATE, b"{".as_slice()),
            (SUBJECT_ROLE_RESET, b"{}".as_slice()),
            (SUBJECT_ROLE_DELETE, b"not json".as_slice()),
        ] {
            let reply = w.role_rpc_reply(subject, payload, deadline).await;
            assert!(!reply.ok);
            assert_eq!(reply.code.as_deref(), Some("failed"), "{subject}");
        }
    }

    #[tokio::test]
    async fn role_rpcs_reply_with_owner_level() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        let deadline = Duration::from_secs(5);
        for (name, owner_level) in [("reader", false), ("boss", true)] {
            let create = serde_json::to_vec(&RoleCreateRequest::login(branch, name, owner_level))
            .unwrap();
            w.computes.sql.lock().unwrap().clear();

            let reply = w
                .role_rpc_reply(SUBJECT_ROLE_CREATE, &create, deadline)
                .await;

            assert!(reply.ok, "{name}: {reply:?}");
            assert_eq!(reply.code, None, "{name}");
            let script = sql(&w).concat();
            assert_eq!(
                script.contains(&format!("GRANT neon_superuser TO \"{name}\"")),
                owner_level,
                "{name}: {script}"
            );
            let state = w.metadata.state();
            let row = &state.endpoints[&ep].logins[name];
            assert_eq!(row.api_owner_level, owner_level, "{name}");
            let level = if owner_level {
                RoleLevel::OwnerLevel
            } else {
                RoleLevel::Plain
            };
            assert_eq!(row.level, level, "{name}");
        }
    }

    // A stuck operation answers within its deadline: create (before its row) and delete say
    // failed (a retry is safe), reset says not applied.
    #[tokio::test(start_paused = true)]
    async fn role_rpcs_past_their_deadline_reply() {
        let (w, journal) = worker();
        let (branch, ep, _db) = with_app(&w, &journal).await;
        let _held = w.metadata.lock_endpoint(&ep).await.unwrap();
        let request = serde_json::to_vec(&RoleRequest {
            branch_id: branch,
            name: "app".into(),
        })
        .unwrap();
        let create = serde_json::to_vec(&RoleCreateRequest::login(branch, "other", true))
        .unwrap();
        let deadline = Duration::from_secs(1);

        for (subject, payload, code) in [
            (SUBJECT_ROLE_CREATE, &create, "failed"),
            (SUBJECT_ROLE_RESET, &request, "not_applied"),
            (SUBJECT_ROLE_DELETE, &request, "failed"),
        ] {
            let reply = w.role_rpc_reply(subject, payload, deadline).await;
            assert_eq!(reply.code.as_deref(), Some(code), "{subject}: {reply:?}");
        }
        assert!(login(&w, &ep, "other").is_none());
    }

    #[tokio::test]
    async fn not_applied_wording_fits_the_operation() {
        let (w, journal) = worker();
        let (branch, _ep, _db) = with_app(&w, &journal).await;
        w.computes.answer(EXISTS_APP, "1\n");
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("ROLE \"".into(), "connection reset".into()));
        let deadline = Duration::from_secs(5);
        let request = serde_json::to_vec(&RoleRequest {
            branch_id: branch,
            name: "app".into(),
        })
        .unwrap();
        let create = serde_json::to_vec(&RoleCreateRequest::login(branch, "other", false))
        .unwrap();

        for (subject, payload, message) in [
            (SUBJECT_ROLE_CREATE, &create, ROLE_CREATE_NOT_APPLIED),
            (SUBJECT_ROLE_RESET, &request, RESET_NOT_APPLIED),
        ] {
            let reply = w.role_rpc_reply(subject, payload, deadline).await;
            assert_eq!(reply.code.as_deref(), Some("not_applied"), "{subject}");
            assert_eq!(reply.error.as_deref(), Some(message), "{subject}");
        }
    }

    // --- U1: NOLOGIN, attributes, memberships, update ------------------------------------

    /// The membership check's SQL, as the fake backend answers it.
    const MEMBERSHIP_CHECK: &str = "MEMBER WITH ADMIN OPTION";
    /// One role's catalog read.
    const ROLE_READ: &str = "FROM pg_authid";

    fn until() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_900_000_000, 0).unwrap()
    }

    /// The membership check's answer: each name and whether the owner holds ADMIN on it.
    fn groups(names: &[(&str, bool)]) -> String {
        let plain: Vec<(&str, bool, bool)> = names.iter().map(|(n, a)| (*n, *a, false)).collect();
        reaching(&plain)
    }

    /// The membership check's answer: each name, whether the owner holds ADMIN on it, and
    /// whether it's a member (at any depth) of a reserved role.
    fn reaching(names: &[(&str, bool, bool)]) -> String {
        let tf = |b: bool| if b { "t" } else { "f" };
        names
            .iter()
            .map(|(n, admin, reaches)| format!("{}|{}|{}\n", hex::encode(n), tf(*admin), tf(*reaches)))
            .collect()
    }

    /// The stored attributes of a non-owner row.
    fn attributes(w: &FakeWorker, ep: &str, name: &str) -> chelabase_metadata::RoleAttributes {
        w.metadata.state().endpoints[ep].logins[name].attributes.clone()
    }

    /// The catalog read of one role answers `role`.
    fn in_catalog(w: &FakeWorker, role: &crate::resync::ObservedRole) {
        w.computes.answers.lock().unwrap().retain(|(n, _)| n != ROLE_READ);
        w.computes.answer(ROLE_READ, &crate::resync::role_line(role));
    }

    fn observed(name: &str, login: bool, password: Option<&str>) -> crate::resync::ObservedRole {
        crate::resync::ObservedRole {
            name: name.into(),
            login,
            password: password.map(str::to_string),
            ..crate::resync::ObservedRole::plain(name)
        }
    }

    #[tokio::test]
    async fn create_nologin_role_has_no_password() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        let spec = RoleSpec {
            login: false,
            ..RoleSpec::login("readers", false)
        };

        assert_eq!(w.create_role(branch, &spec).await, RoleOutcome::Ok);

        assert_eq!(login(&w, &ep, "readers"), Some((None, None, false)));
        let row = w.metadata.state().endpoints[&ep].logins["readers"].clone();
        assert_eq!((row.origin, row.login), (RoleOrigin::Api, false));
        let script = sql(&w).pop().unwrap();
        assert_eq!(
            script,
            format!("{SCRIPT}CREATE ROLE \"readers\" NOLOGIN;\nRESET ROLE;")
        );
        assert!(!script.contains("PASSWORD"), "{script}");
    }

    #[tokio::test]
    async fn create_role_applies_attributes_and_memberships() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        w.computes.answer(MEMBERSHIP_CHECK, &groups(&[("g", true)]));
        let spec = RoleSpec {
            login: false,
            inherit: false,
            connection_limit: 3,
            valid_until: Some(until()),
            bypass_rls: true,
            member_of: vec!["g".into()],
            ..RoleSpec::login("r", false)
        };

        assert_eq!(w.create_role(branch, &spec).await, RoleOutcome::Ok);

        let sent = sql(&w);
        assert!(
            sent.iter().any(|s| s.contains(MEMBERSHIP_CHECK) && s.contains("'g'")),
            "{sent:?}"
        );
        assert_eq!(
            sent.last().unwrap(),
            &format!(
                "{SCRIPT}CREATE ROLE \"r\" NOLOGIN NOINHERIT CONNECTION LIMIT 3 VALID UNTIL \
                 '2030-03-17T17:46:40.000000Z' BYPASSRLS;\nGRANT \"g\" TO \"r\";\nRESET ROLE;"
            )
        );
        assert_eq!(attributes(&w, &ep, "r"), spec.attributes());
        assert!(!login(&w, &ep, "r").unwrap().2, "confirmed");

        // A LOGIN role keeps its password, after the attributes.
        w.computes.sql.lock().unwrap().clear();
        let spec = RoleSpec {
            connection_limit: 0,
            member_of: vec!["g".into()],
            ..RoleSpec::login("app", false)
        };
        assert_eq!(w.create_role(branch, &spec).await, RoleOutcome::Ok);
        let verifier = login(&w, &ep, "app").unwrap().0.unwrap();
        assert_eq!(
            sql(&w).last().unwrap(),
            &format!(
                "{SCRIPT}CREATE ROLE \"app\" LOGIN CONNECTION LIMIT 0 PASSWORD '{verifier}';\n\
                 GRANT \"g\" TO \"app\";\nRESET ROLE;"
            )
        );
    }

    #[tokio::test]
    async fn membership_in_reserved_role_is_refused() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        let not_allowed = refused("role_membership_not_allowed");

        for target in [
            "authenticator",
            "anon",
            "authenticated",
            "chelabase_admin",
            "chelabase_auth_sync",
            "neon_superuser",
            "cloud_admin",
            "postgres",
            "pg_read_all_data",
            "r",
        ] {
            let spec = RoleSpec {
                member_of: vec![target.into()],
                ..RoleSpec::login("r", false)
            };
            let outcome = w.create_role(branch, &spec).await;
            assert!(not_allowed(&outcome), "{target}: {outcome:?}");
        }
        assert!(position(&journal, "metadata.lock_endpoint").is_none());
        assert!(sql(&w).is_empty(), "{:?}", sql(&w));

        // A role that doesn't exist, or one the owner can't administer.
        w.computes
            .answer(MEMBERSHIP_CHECK, &groups(&[("theirs", false)]));
        for target in ["ghost", "theirs"] {
            let spec = RoleSpec {
                member_of: vec![target.into()],
                ..RoleSpec::login("r", false)
            };
            let outcome = w.create_role(branch, &spec).await;
            assert!(not_allowed(&outcome), "{target}: {outcome:?}");
        }
        assert!(position(&journal, "metadata.insert_api_role").is_none());
        assert!(login(&w, &ep, "r").is_none());

        // An update is held to the same rules.
        assert_eq!(
            w.create_role(branch, &RoleSpec::login("app", false)).await,
            RoleOutcome::Ok
        );
        for target in ["anon", "neon_superuser", "app"] {
            let patch = RolePatch {
                member_of: Some(vec![target.into()]),
                ..RolePatch::default()
            };
            let outcome = w.update_role(branch, "app", &patch).await;
            assert!(not_allowed(&outcome), "{target}: {outcome:?}");
        }
        // And an attribute outside the allowed values.
        let patch = RolePatch {
            connection_limit: Some(-2),
            ..RolePatch::default()
        };
        assert!(refused("role_attribute_not_allowed")(
            &w.update_role(branch, "app", &patch).await
        ));
    }

    #[tokio::test]
    async fn update_role_toggles_login_and_reseals() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        assert_eq!(
            w.create_role(branch, &RoleSpec::login("app", false)).await,
            RoleOutcome::Ok
        );
        let (verifier, _, _) = login(&w, &ep, "app").unwrap();
        in_catalog(&w, &observed("app", true, verifier.as_deref()));
        w.computes.sql.lock().unwrap().clear();
        let off = RolePatch {
            login: Some(false),
            ..RolePatch::default()
        };
        let asked = w.member_role_requests.sent().len();

        assert_eq!(w.update_role(branch, "app", &off).await, RoleOutcome::Ok);
        // T8 I3: a successful update asks for a member-role reconcile (an owner-level or
        // membership change can make the role unsafe for member roles, R16).
        assert_eq!(w.member_role_requests.sent().len(), asked + 1, "a member-role reconcile is asked for");
        assert_eq!(w.member_role_requests.sent().last(), Some(&branch));

        assert_eq!(login(&w, &ep, "app"), Some((None, None, false)));
        assert!(!w.metadata.state().endpoints[&ep].logins["app"].login);
        assert_eq!(
            sql(&w).last().unwrap(),
            "SET search_path = pg_catalog, pg_temp;\nALTER ROLE \"app\" PASSWORD NULL;\n\
             ALTER ROLE \"app\" NOLOGIN;"
        );

        // Back on: a new password, sealed for the branch, revealable again.
        in_catalog(&w, &observed("app", false, None));
        journal.lock().unwrap().clear();
        let on = RolePatch {
            login: Some(true),
            ..RolePatch::default()
        };
        assert_eq!(w.update_role(branch, "app", &on).await, RoleOutcome::Ok);

        let (verifier, sealed, pending) = login(&w, &ep, "app").unwrap();
        let verifier = verifier.unwrap();
        assert!(!pending);
        let password =
            open_role_password(&w.key, &branch.to_string(), "app", &sealed.unwrap()).unwrap();
        let params = verifier.split('$').nth(1).unwrap();
        let (iterations, salt) = params.split_once(':').unwrap();
        use base64::Engine;
        let salt = base64::engine::general_purpose::STANDARD
            .decode(salt)
            .unwrap();
        assert_eq!(
            chelabase_neon::scram::verifier_with_salt(&password, &salt, iterations.parse().unwrap()),
            verifier
        );
        assert_eq!(
            sql(&w).last().unwrap(),
            &format!(
                "SET search_path = pg_catalog, pg_temp;\nALTER ROLE \"app\" PASSWORD '{verifier}';\n\
                 ALTER ROLE \"app\" LOGIN;"
            )
        );
        // The catalog read comes first; the change runs after the row is stored.
        let applied = entries(&journal)
            .iter()
            .rposition(|e| e.starts_with(&format!("compute.sql {ep} ")))
            .unwrap();
        let order = [
            pos(&journal, "metadata.lock_endpoint"),
            pos(&journal, "metadata.update_api_role"),
            applied,
            pos(&journal, "metadata.confirm_role"),
            pos(&journal, &format!("cache.password_updated {ep} app")),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
    }

    #[tokio::test]
    async fn update_role_bypass_rls() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        assert_eq!(
            w.create_role(branch, &RoleSpec::login("app", false)).await,
            RoleOutcome::Ok
        );
        let (verifier, sealed, _) = login(&w, &ep, "app").unwrap();
        in_catalog(&w, &observed("app", true, verifier.as_deref()));
        w.computes.sql.lock().unwrap().clear();
        let on = RolePatch {
            bypass_rls: Some(true),
            ..RolePatch::default()
        };

        assert_eq!(w.update_role(branch, "app", &on).await, RoleOutcome::Ok);

        assert_eq!(
            sql(&w).last().unwrap(),
            "SET search_path = pg_catalog, pg_temp;\nALTER ROLE \"app\" BYPASSRLS;"
        );
        assert!(attributes(&w, &ep, "app").bypass_rls);
        // The password is untouched.
        assert_eq!(login(&w, &ep, "app"), Some((verifier, sealed, false)));

        // A role made in SQL (NOLOGIN, so without a row) changes in SQL only.
        in_catalog(&w, &observed("reporting", false, None));
        w.computes.sql.lock().unwrap().clear();
        assert_eq!(w.update_role(branch, "reporting", &on).await, RoleOutcome::Ok);
        assert_eq!(
            sql(&w).last().unwrap(),
            "SET search_path = pg_catalog, pg_temp;\nALTER ROLE \"reporting\" BYPASSRLS;"
        );
        assert!(login(&w, &ep, "reporting").is_none());

        // Unknown everywhere: not found, nothing run.
        w.computes.answers.lock().unwrap().clear();
        w.computes.sql.lock().unwrap().clear();
        assert_eq!(w.update_role(branch, "ghost", &on).await, RoleOutcome::NotFound);
        assert!(!sql(&w).iter().any(|s| s.contains("ALTER")), "{:?}", sql(&w));
        // The owner and reserved names are refused.
        for name in [OWNER_ROLE, "authenticator"] {
            assert!(refused("reserved_role_name")(
                &w.update_role(branch, name, &on).await
            ));
        }
    }

    #[tokio::test]
    async fn update_role_refuses_roles_the_owner_does_not_administer() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        let on = RolePatch {
            bypass_rls: Some(true),
            ..RolePatch::default()
        };

        // A role made in SQL by someone else (the owner has no ADMIN on it): not ours.
        let mut theirs = observed("theirs", false, None);
        theirs.owner_admin = false;
        in_catalog(&w, &theirs);
        w.computes.sql.lock().unwrap().clear();
        assert_eq!(w.update_role(branch, "theirs", &on).await, RoleOutcome::NotFound);
        assert!(!sql(&w).iter().any(|s| s.contains("ALTER")), "{:?}", sql(&w));
        assert!(position(&journal, "metadata.store_role_secret").is_none());

        // Turning LOGIN on doesn't store a password for it either.
        let login_on = RolePatch {
            login: Some(true),
            ..RolePatch::default()
        };
        assert_eq!(w.update_role(branch, "theirs", &login_on).await, RoleOutcome::NotFound);
        assert!(login(&w, &ep, "theirs").is_none());
        assert!(!sql(&w).iter().any(|s| s.contains("ALTER")), "{:?}", sql(&w));

        // Any `chelabase_` role is Chelabase's own, whatever its catalog says.
        journal.lock().unwrap().clear();
        for name in ["chelabase_custom", "Chelabase_x"] {
            assert!(
                refused("reserved_role_name")(&w.update_role(branch, name, &on).await),
                "{name}"
            );
        }
        assert!(position(&journal, "metadata.lock_endpoint").is_none());
    }

    #[tokio::test]
    async fn membership_reaching_reserved_roles_is_refused() {
        let (w, journal) = worker();
        let (branch, _ep, _db) = ready(&w, &journal).await;
        // `g` is the owner's to grant but is itself (at some depth) a member of a reserved role.
        w.computes
            .answer(MEMBERSHIP_CHECK, &reaching(&[("g", true, true)]));
        let spec = RoleSpec {
            member_of: vec!["g".into()],
            ..RoleSpec::login("r", false)
        };

        let outcome = w.create_role(branch, &spec).await;

        assert!(refused("role_membership_not_allowed")(&outcome), "{outcome:?}");
        assert!(
            matches!(&outcome, RoleOutcome::Refused(_, m) if m.contains("reserved")),
            "{outcome:?}"
        );
        let check = sql(&w)
            .into_iter()
            .find(|s| s.contains(MEMBERSHIP_CHECK))
            .unwrap();
        for reserved in [
            "'neon_superuser'",
            "'authenticator'",
            "'anon'",
            "'authenticated'",
            "'chelabase\\_%'",
            "'pg\\_%'",
            "'MEMBER')",
        ] {
            assert!(check.contains(reserved), "{reserved}: {check}");
        }
        assert!(position(&journal, "metadata.insert_api_role").is_none());

        // The same group without the reserved reach is accepted.
        w.computes.answers.lock().unwrap().clear();
        w.computes
            .answer(MEMBERSHIP_CHECK, &reaching(&[("g", true, false)]));
        assert_eq!(w.create_role(branch, &spec).await, RoleOutcome::Ok);

        // An update is held to the same rule.
        w.computes.answers.lock().unwrap().clear();
        w.computes
            .answer(MEMBERSHIP_CHECK, &reaching(&[("g", true, true)]));
        in_catalog(&w, &observed("r", true, None));
        let patch = RolePatch {
            member_of: Some(vec!["g".into()]),
            ..RolePatch::default()
        };
        let outcome = w.update_role(branch, "r", &patch).await;
        assert!(refused("role_membership_not_allowed")(&outcome), "{outcome:?}");
    }

    #[tokio::test]
    async fn role_update_rpc_replies() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        assert_eq!(
            w.create_role(branch, &RoleSpec::login("app", false)).await,
            RoleOutcome::Ok
        );
        let verifier = login(&w, &ep, "app").unwrap().0;
        in_catalog(&w, &observed("app", true, verifier.as_deref()));
        let request = chelabase_nats::RoleUpdateRequest {
            inherit: Some(false),
            ..chelabase_nats::RoleUpdateRequest::new(branch, "app")
        };
        let payload = serde_json::to_vec(&request).unwrap();

        let reply = w
            .role_rpc_reply(chelabase_nats::SUBJECT_ROLE_UPDATE, &payload, Duration::from_secs(5))
            .await;

        assert!(reply.ok, "{reply:?}");
        assert!(!attributes(&w, &ep, "app").inherit);
        let mut changed = observed("app", true, verifier.as_deref());
        changed.inherit = false;
        in_catalog(&w, &changed);
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("ALTER ROLE".into(), "connection reset".into()));
        let request = chelabase_nats::RoleUpdateRequest {
            inherit: Some(true),
            ..chelabase_nats::RoleUpdateRequest::new(branch, "app")
        };
        let reply = w
            .role_rpc_reply(
                chelabase_nats::SUBJECT_ROLE_UPDATE,
                &serde_json::to_vec(&request).unwrap(),
                Duration::from_secs(5),
            )
            .await;
        assert_eq!(reply.code.as_deref(), Some("not_applied"));
        assert_eq!(
            reply.error.as_deref(),
            Some(chelabase_nats::ROLE_UPDATE_NOT_APPLIED)
        );
    }

    // --- T14 fix batch F2 -------------------------------------------------------------------

    /// I2: turning LOGIN on gives the role a copy, so it counts against the cap: refused at
    /// the cap for an API row and for a role made in SQL, with nothing stored or run.
    #[tokio::test]
    async fn turning_login_on_at_the_cap_is_refused() {
        let (mut w, journal) = worker();
        w.roles.role_cap = 2;
        let (branch, ep, _db) = ready(&w, &journal).await;
        sql_row(&w, &ep, "reporter", "SCRAM-SHA-256$x");
        let group = RoleSpec {
            login: false,
            ..RoleSpec::login("grp", false)
        };
        assert_eq!(w.create_role(branch, &group).await, RoleOutcome::Ok);
        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);
        let on = RolePatch {
            login: Some(true),
            ..RolePatch::default()
        };

        in_catalog(&w, &observed("grp", false, None));
        w.computes.sql.lock().unwrap().clear();
        let outcome = w.update_role(branch, "grp", &on).await;
        assert!(
            matches!(&outcome, RoleOutcome::Refused("role_cap_reached", m) if m.contains('2')),
            "{outcome:?}"
        );
        assert_eq!(login(&w, &ep, "grp"), Some((None, None, false)), "nothing stored");
        assert!(!w.metadata.state().endpoints[&ep].logins["grp"].login);
        assert!(!sql(&w).iter().any(|s| s.contains("ALTER")), "{:?}", sql(&w));

        // A role made in SQL without a password: the same.
        in_catalog(&w, &observed("made_in_sql", false, None));
        let outcome = w.update_role(branch, "made_in_sql", &on).await;
        assert!(refused("role_cap_reached")(&outcome), "{outcome:?}");
        assert!(login(&w, &ep, "made_in_sql").is_none());
        assert!(!sql(&w).iter().any(|s| s.contains("ALTER")), "{:?}", sql(&w));

        // Under the cap it goes through.
        w.roles.role_cap = 3;
        in_catalog(&w, &observed("grp", false, None));
        assert_eq!(w.update_role(branch, "grp", &on).await, RoleOutcome::Ok);
        assert!(login(&w, &ep, "grp").unwrap().0.is_some());
        assert_eq!(
            role_reply(RoleOutcome::Refused("role_cap_reached", "x".into())).code.as_deref(),
            Some("role_cap_reached")
        );
    }

    /// E32: a PATCH starts from the catalog, not from the stored row: SQL changes made since
    /// stick (no INHERIT revert, no REVOKE), and the row records the result.
    #[tokio::test]
    async fn a_patch_starts_from_what_sql_made_of_an_api_role() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);
        let verifier = login(&w, &ep, "app").unwrap().0;
        // Since then, in SQL: NOINHERIT, a membership and BYPASSRLS.
        let mut now = observed("app", true, verifier.as_deref());
        now.inherit = false;
        now.bypass_rls = true;
        now.member_of = vec!["g".into()];
        now.owner_granted = vec!["g".into()];
        in_catalog(&w, &now);
        w.computes.sql.lock().unwrap().clear();
        let patch = RolePatch {
            connection_limit: Some(5),
            ..RolePatch::default()
        };

        assert_eq!(w.update_role(branch, "app", &patch).await, RoleOutcome::Ok);

        assert_eq!(
            sql(&w).last().unwrap(),
            "SET search_path = pg_catalog, pg_temp;\nALTER ROLE \"app\" CONNECTION LIMIT 5;"
        );
        let stored = attributes(&w, &ep, "app");
        assert_eq!(
            stored,
            chelabase_metadata::RoleAttributes {
                inherit: false,
                connection_limit: 5,
                valid_until: None,
                bypass_rls: true,
                member_of: vec!["g".into()],
                member_no_set: vec![],
                member_admin: vec![],
            }
        );
        assert!(!w.metadata.state().endpoints[&ep].logins["app"].attributes_pending);
    }

    /// F2 fix round I-1: a confirm that loses to a version bump (a receiver write meanwhile)
    /// still clears the change's `attributes_pending` by its change id, and asks for a resync
    /// for the rest. A later SQL `GRANT` is then recorded by the resync, never revoked.
    #[tokio::test]
    async fn a_lost_confirm_doesnt_leave_a_stale_change_to_drive() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        w.metadata.state().confirm_version_moves = true;

        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);

        let row = w.metadata.state().endpoints[&ep].logins["app"].clone();
        assert!(!row.attributes_pending, "cleared by its change id");
        assert!(row.pending, "the version guard still holds for pending");
        assert_eq!(w.resync_requests.sent(), [branch], "a resync is asked for");

        // `GRANT readers TO app` in SQL, then the resync it triggers.
        let verifier = login(&w, &ep, "app").unwrap().0;
        let mut granted = observed("app", true, verifier.as_deref());
        granted.member_of = vec!["readers".into()];
        granted.owner_granted = vec!["readers".into()];
        w.computes.answer(
            "FROM pg_authid",
            &format!(
                "{}{}",
                crate::resync::role_line(&granted),
                crate::resync::role_line(&observed("readers", false, None))
            ),
        );
        w.computes.sql.lock().unwrap().clear();
        w.resync(branch).await.unwrap();
        assert!(!sql(&w).iter().any(|s| s.contains("REVOKE")), "{:?}", sql(&w));
        assert_eq!(attributes(&w, &ep, "app").member_of, ["readers"]);
        assert!(!w.metadata.state().endpoints[&ep].logins["app"].pending);
    }

    /// E32: a role SQL made NOLOGIN keeps its password through an unrelated PATCH (only a
    /// PATCH turning LOGIN off drops it), so `ALTER ROLE … LOGIN` in SQL brings it back.
    #[tokio::test]
    async fn an_unrelated_patch_keeps_the_password_of_a_role_sql_made_nologin() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);
        let (verifier, sealed, _) = login(&w, &ep, "app").unwrap();
        // `ALTER ROLE app NOLOGIN` in SQL: forwarded, the row's login follows.
        w.metadata.state().endpoints.get_mut(&ep).unwrap().logins.get_mut("app").unwrap().login = false;
        in_catalog(&w, &observed("app", false, verifier.as_deref()));
        w.computes.sql.lock().unwrap().clear();
        let patch = RolePatch {
            connection_limit: Some(5),
            ..RolePatch::default()
        };

        assert_eq!(w.update_role(branch, "app", &patch).await, RoleOutcome::Ok);

        assert_eq!(
            sql(&w).last().unwrap(),
            "SET search_path = pg_catalog, pg_temp;\nALTER ROLE \"app\" CONNECTION LIMIT 5;"
        );
        assert_eq!(login(&w, &ep, "app"), Some((verifier, sealed, false)), "the password stays");
    }

    /// T14 M1: a change the compute hasn't confirmed yet is the starting point instead, and a
    /// stored group dropped in SQL since isn't granted again by an unrelated PATCH.
    #[tokio::test]
    async fn an_unrelated_patch_doesnt_grant_a_dropped_group() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        w.computes.answer(MEMBERSHIP_CHECK, &groups(&[("g", true)]));
        let spec = RoleSpec {
            member_of: vec!["g".into()],
            inherit: false,
            ..RoleSpec::login("app", false)
        };
        assert_eq!(w.create_role(branch, &spec).await, RoleOutcome::Ok);
        let verifier = login(&w, &ep, "app").unwrap().0;
        // The create's change wasn't confirmed (still pending), and `g` was dropped since.
        w.metadata
            .state()
            .endpoints
            .get_mut(&ep)
            .unwrap()
            .logins
            .get_mut("app")
            .unwrap()
            .attributes_pending = true;
        w.computes.answers.lock().unwrap().clear();
        in_catalog(&w, &observed("app", true, verifier.as_deref()));
        w.computes.sql.lock().unwrap().clear();
        let patch = RolePatch {
            connection_limit: Some(5),
            ..RolePatch::default()
        };

        assert_eq!(w.update_role(branch, "app", &patch).await, RoleOutcome::Ok);

        let script = sql(&w).last().unwrap().clone();
        assert!(!script.contains("GRANT"), "{script}");
        // The pending NOINHERIT is still driven; the limit too.
        assert!(script.contains("NOINHERIT") && script.contains("CONNECTION LIMIT 5"), "{script}");
        assert_eq!(attributes(&w, &ep, "app").member_of, Vec::<String>::new());
    }

    /// I3/I4 helpers for the merge (F3): the stored-or-catalog starting point, exhaustively.
    #[test]
    fn a_roles_current_state_follows_the_catalog_unless_a_change_is_pending() {
        let row = |origin: RoleOrigin, pending: bool| chelabase_metadata::RoleCopy {
            name: "r".into(),
            origin,
            scram_verifier: None,
            password_enc: None,
            login: true,
            level: RoleLevel::Plain,
            api_owner_level: true,
            pending,
            attributes_pending: pending,
            created_at: chrono::Utc::now(),
            attributes: chelabase_metadata::RoleAttributes {
                connection_limit: 7,
                ..Default::default()
            },
        };
        let catalog = |owner_admin: bool| crate::resync::ObservedRole {
            owner_admin,
            login: false,
            connection_limit: 3,
            ..crate::resync::ObservedRole::plain("r")
        };
        let stored = (true, true, 7);
        let seen = (false, false, 3);
        for origin in [None, Some(RoleOrigin::Sql), Some(RoleOrigin::Api)] {
            for pending in [false, true] {
                for observed in [None, Some(false), Some(true)] {
                    let r = origin.map(|o| row(o, pending));
                    let o = observed.map(catalog);
                    let got = current_role_state(r.as_ref(), o.as_ref())
                        .map(|s| (s.owner_level, s.login, s.attributes.connection_limit));
                    let api = origin == Some(RoleOrigin::Api);
                    let want = match (api, pending, observed) {
                        (true, true, _) | (true, _, None) => Ok(stored),
                        (true, false, Some(_)) => Ok(seen),
                        (false, _, Some(true)) => Ok(seen),
                        (false, _, _) => Err(RoleOutcome::NotFound),
                    };
                    assert_eq!(got, want, "{origin:?} pending={pending} observed={observed:?}");
                }
            }
        }
    }

    /// I4 helper for the merge (F3): the membership check runs as SQL alone on any compute
    /// (a rehearsal's), with the roles path's verdicts.
    #[tokio::test]
    async fn the_member_check_runs_on_any_compute() {
        let (w, journal) = worker();
        let (_branch, _ep, _db) = ready(&w, &journal).await;
        w.computes.answer_at(
            "ep-rehearsal",
            MEMBERSHIP_CHECK,
            &reaching(&[("ok", true, false), ("theirs", false, false), ("deep", true, true)]),
        );
        let check = |targets: &[&str]| {
            let targets: Vec<String> = targets.iter().map(|t| t.to_string()).collect();
            let w = &w;
            async move { w.check_member_targets("ep-rehearsal", "app", &targets).await }
        };
        assert_eq!(check(&[]).await, Ok(()));
        assert_eq!(check(&["ok"]).await, Ok(()));
        for bad in ["theirs", "deep", "ghost"] {
            let outcome = check(&["ok", bad]).await.unwrap_err();
            assert!(refused("role_membership_not_allowed")(&outcome), "{bad}: {outcome:?}");
        }
        let ran = entries(&journal);
        assert!(ran.iter().any(|e| e.starts_with("compute.sql ep-rehearsal app")), "{ran:?}");
    }

    /// E29: a rename through the control plane: the role in SQL (forwarding off), its row
    /// (resealed under the new name), its keys and the proxies' copies move together.
    #[tokio::test]
    async fn rename_role_moves_the_role_its_row_and_its_keys() {
        let (w, journal) = worker();
        let (branch, ep, db) = ready(&w, &journal).await;
        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);
        let (verifier, sealed, _) = login(&w, &ep, "app").unwrap();
        let password =
            open_role_password(&w.key, &branch.to_string(), "app", &sealed.unwrap()).unwrap();
        w.metadata.state().data_keys.push(secret(branch, "app", "app-key"));
        in_catalog(&w, &observed("app", true, verifier.as_deref()));
        journal.lock().unwrap().clear();
        w.computes.sql.lock().unwrap().clear();
        let asked = w.member_role_requests.sent().len();

        assert_eq!(w.rename_role(branch, "app", "app_v2").await, RoleOutcome::Ok);
        assert_eq!(w.member_role_requests.sent().len(), asked + 1, "a member-role reconcile is asked for");

        assert!(
            sql(&w).iter().any(|s| s.ends_with("ALTER ROLE \"app\" RENAME TO \"app_v2\";")),
            "{:?}",
            sql(&w)
        );
        assert!(login(&w, &ep, "app").is_none());
        let (moved_verifier, moved_sealed, _) = login(&w, &ep, "app_v2").unwrap();
        assert_eq!(moved_verifier, verifier);
        assert_eq!(
            open_role_password(&w.key, &branch.to_string(), "app_v2", &moved_sealed.unwrap())
                .unwrap(),
            password,
            "resealed under the new name"
        );
        let key = w.metadata.state().data_keys.iter().find(|k| k.name == "app-key").cloned().unwrap();
        assert_eq!(key.role, "app_v2");
        let order = [
            pos(&journal, "metadata.lock_endpoint"),
            pos(&journal, &format!("compute.sql {ep} {db} {EXISTS_SQL} = 'app_v2'")),
            pos(&journal, "metadata.rename_role_copy"),
            pos(&journal, &format!("cache.password_updated {ep} app")),
            pos(&journal, &format!("cache.password_updated {ep} app_v2")),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
    }

    /// E29: the rename's refusals, none of which changes anything.
    #[tokio::test]
    async fn rename_role_refusals() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);
        let verifier = login(&w, &ep, "app").unwrap().0;
        in_catalog(&w, &observed("app", true, verifier.as_deref()));
        w.computes.sql.lock().unwrap().clear();

        for (from, to) in [
            ("app", OWNER_ROLE),
            (OWNER_ROLE, "boss"),
            ("app", "anon"),
            ("app", "pg_x"),
            ("app", "chelabase_x"),
            ("authenticator", "x"),
        ] {
            let outcome = w.rename_role(branch, from, to).await;
            assert!(refused("reserved_role_name")(&outcome), "{from} -> {to}: {outcome:?}");
        }
        for (from, to) in [("app", ""), ("", "x"), ("app", "a\0b")] {
            let outcome = w.rename_role(branch, from, to).await;
            assert!(refused("invalid_role_name")(&outcome), "{from:?} -> {to:?}: {outcome:?}");
        }
        assert!(refused("invalid_role_name")(&w.rename_role(branch, "app", "app").await));
        // The new name is taken in SQL.
        w.computes.answer_once_at(&ep, "rolname = 'taken'", "1\n");
        let outcome = w.rename_role(branch, "app", "taken").await;
        assert!(refused("role_exists")(&outcome), "{outcome:?}");
        // A row already holds it.
        sql_row(&w, &ep, "held", "SCRAM-SHA-256$h");
        let outcome = w.rename_role(branch, "app", "held").await;
        assert!(refused("role_exists")(&outcome), "{outcome:?}");
        // A role made in SQL by someone else, or one that doesn't exist.
        let mut theirs = observed("theirs", false, None);
        theirs.owner_admin = false;
        in_catalog(&w, &theirs);
        assert_eq!(w.rename_role(branch, "theirs", "mine").await, RoleOutcome::NotFound);
        w.computes.answers.lock().unwrap().retain(|(n, _)| n != ROLE_READ);
        assert_eq!(w.rename_role(branch, "ghost", "mine").await, RoleOutcome::NotFound);
        assert!(!sql(&w).iter().any(|s| s.contains("RENAME")), "{:?}", sql(&w));
        assert!(login(&w, &ep, "app").is_some());
        // Not ready.
        assert_eq!(w.rename_role(BranchId::new(), "app", "x").await, RoleOutcome::NotReady);
    }

    /// E29: when the SQL rename fails, the row and keys move back to the old name.
    #[tokio::test]
    async fn a_failed_sql_rename_moves_the_row_back() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);
        let (verifier, sealed, _) = login(&w, &ep, "app").unwrap();
        w.metadata.state().data_keys.push(secret(branch, "app", "app-key"));
        in_catalog(&w, &observed("app", true, verifier.as_deref()));
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("RENAME TO".into(), "connection reset".into()));
        // The catalog afterwards: still under the old name.
        w.computes.answer(ROLE_NAMES, &format!("{}\n", hex::encode("app")));

        let outcome = w.rename_role(branch, "app", "app_v2").await;

        assert!(matches!(&outcome, RoleOutcome::Failed(m) if m.contains("connection reset")), "{outcome:?}");
        assert_eq!(login(&w, &ep, "app").map(|l| (l.0, l.1)), Some((verifier, sealed)));
        assert!(login(&w, &ep, "app_v2").is_none());
        let key = w.metadata.state().data_keys.iter().find(|k| k.name == "app-key").cloned().unwrap();
        assert_eq!(key.role, "app");
    }

    /// The role names the post-rename check answers.
    const ROLE_NAMES: &str = "chelabase: role names";

    /// F2 fix round I-2: the RENAME committed but its answer was lost: the catalog shows the
    /// new name and not the old, so the rename stands (row and keys stay moved).
    #[tokio::test]
    async fn a_rename_whose_answer_was_lost_stands() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);
        let verifier = login(&w, &ep, "app").unwrap().0;
        w.metadata.state().data_keys.push(secret(branch, "app", "app-key"));
        in_catalog(&w, &observed("app", true, verifier.as_deref()));
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("RENAME TO".into(), "server closed the connection".into()));
        w.computes.answer(ROLE_NAMES, &format!("{}\n", hex::encode("app_v2")));

        assert_eq!(w.rename_role(branch, "app", "app_v2").await, RoleOutcome::Ok);

        assert!(login(&w, &ep, "app").is_none());
        assert_eq!(login(&w, &ep, "app_v2").unwrap().0, verifier);
        let key = w.metadata.state().data_keys.iter().find(|k| k.name == "app-key").cloned().unwrap();
        assert_eq!(key.role, "app_v2");
        assert!(position(&journal, &format!("cache.password_updated {ep} app_v2")).is_some());

        // The check itself can't be read: not ready, the move left as it is.
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);
        let verifier = login(&w, &ep, "app").unwrap().0;
        in_catalog(&w, &observed("app", true, verifier.as_deref()));
        w.computes.fail_when("RENAME TO");
        w.computes.fail_when(ROLE_NAMES);
        assert_eq!(w.rename_role(branch, "app", "app_v2").await, RoleOutcome::NotReady);
        assert!(login(&w, &ep, "app_v2").is_some());
    }

    /// F2 fix round I-2: the RENAME answered an error and the catalog has neither name (the
    /// role went meanwhile): the row isn't moved back, since a move back would have the resync
    /// make `from` again; the outcome is a failure.
    #[tokio::test]
    async fn a_failed_rename_of_a_vanished_role_isnt_moved_back() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);
        let verifier = login(&w, &ep, "app").unwrap().0;
        w.metadata.state().data_keys.push(secret(branch, "app", "app-key"));
        in_catalog(&w, &observed("app", true, verifier.as_deref()));
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("RENAME TO".into(), "connection reset".into()));
        w.computes.answer(ROLE_NAMES, "");

        let outcome = w.rename_role(branch, "app", "app_v2").await;

        assert!(matches!(&outcome, RoleOutcome::Failed(m) if m.contains("connection reset")), "{outcome:?}");
        assert!(login(&w, &ep, "app").is_none());
        assert!(login(&w, &ep, "app_v2").is_some());
        assert!(position(&journal, &format!("metadata.rename_role_copy {ep} app_v2 -> app")).is_none());
        let key = w.metadata.state().data_keys.iter().find(|k| k.name == "app-key").cloned().unwrap();
        assert_eq!(key.role, "app_v2");
    }

    /// F2 fix round M5: an endpoint gone between the checks and the row's move is not ready,
    /// never `role_exists`.
    #[tokio::test]
    async fn a_rename_whose_endpoint_went_meanwhile_is_not_ready() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);
        let verifier = login(&w, &ep, "app").unwrap().0;
        in_catalog(&w, &observed("app", true, verifier.as_deref()));
        w.metadata.state().rename_endpoint_vanishes = true;

        assert_eq!(w.rename_role(branch, "app", "app_v2").await, RoleOutcome::NotReady);
        assert!(!sql(&w).iter().any(|s| s.contains("RENAME TO")), "{:?}", sql(&w));
    }

    /// F2 fix round M4: a rename that couldn't reseal marks the row unreadable; moving it back
    /// restores the flag it had.
    #[tokio::test]
    async fn moving_a_rename_back_restores_the_unreadable_flag() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        assert_eq!(w.create_role(branch, &RoleSpec::login("app", false)).await, RoleOutcome::Ok);
        let verifier = login(&w, &ep, "app").unwrap().0;
        // A sealed password that won't open.
        w.metadata.state().endpoints.get_mut(&ep).unwrap().logins.get_mut("app").unwrap().password_enc =
            Some("v1:garbage".into());
        in_catalog(&w, &observed("app", true, verifier.as_deref()));
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("RENAME TO".into(), "connection reset".into()));
        w.computes.answer(ROLE_NAMES, &format!("{}\n", hex::encode("app")));

        assert!(matches!(w.rename_role(branch, "app", "app_v2").await, RoleOutcome::Failed(_)));

        let state = w.metadata.state();
        let endpoint = &state.endpoints[&ep];
        assert_eq!(endpoint.logins["app"].password_enc.as_deref(), Some("v1:garbage"));
        assert!(!endpoint.unreadable.contains("app"), "the flag it had");
    }

    // --- Personal logins (c3, RT4-4, K6 H2/H5) -----------------------------------------------

    /// F2: a whole verifier: `create_personal_login` takes no other.
    const PERSONAL_VERIFIER: &str = crate::f2_tests::PENCIL;

    /// The one script that made `name`, with its endpoint and database.
    fn creating_scripts(w: &FakeWorker, name: &str) -> Vec<(String, String, String)> {
        let needle = format!("CREATE ROLE {}", chelabase_types::role_name::quote_ident(name));
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, s)| s.contains(&needle))
            .cloned()
            .collect()
    }

    // The role and its marker are one script (one simple query, so one transaction): no other
    // session sees the role unmarked.
    #[tokio::test]
    async fn a_personal_login_is_made_and_marked_in_one_script() {
        use chelabase_types::role_name::mark_personal_login_sql;
        let (w, journal) = worker();
        let (_branch, ep, _db) = ready(&w, &journal).await;

        w.create_personal_login(&ep, "alice", PERSONAL_VERIFIER).await.unwrap();

        let made = creating_scripts(&w, "alice");
        assert_eq!(made.len(), 1, "{:?}", sql(&w));
        let (at, database, script) = &made[0];
        assert_eq!(at, &ep);
        assert_eq!(database, crate::resync::SYSTEM_DATABASE);
        let create = script.find("CREATE ROLE \"alice\"").unwrap();
        let mark = script
            .find(&mark_personal_login_sql("alice"))
            .unwrap_or_else(|| panic!("no marker in {script}"));
        assert!(create < mark, "the marker follows the CREATE: {script}");
        assert!(script.contains(" LOGIN") && !script.contains("NOLOGIN"), "{script}");
        assert!(script.contains(&format!("PASSWORD '{PERSONAL_VERIFIER}'")), "{script}");
        // Made by a superuser (cloud_admin): no `SET ROLE`, so the owner never holds ADMIN on it
        // and no membership is ever granted by the owner (K6 H2).
        assert!(!script.contains("SET ROLE"), "{script}");
        // Plain: none of the owner's attributes.
        for attribute in ["SUPERUSER", "CREATEROLE", "CREATEDB", "REPLICATION", "BYPASSRLS", "neon_superuser"] {
            assert!(!script.contains(attribute), "{attribute} in {script}");
        }
        // No script made the role without the marker, and none marks it without making it.
        assert!(
            !sql(&w).iter().any(|s| s.contains(&mark_personal_login_sql("alice")) && !s.contains("CREATE ROLE")),
            "{:?}",
            sql(&w)
        );
    }

    #[tokio::test]
    async fn a_personal_login_name_is_quoted_exactly() {
        use chelabase_types::role_name::mark_personal_login_sql;
        let (w, journal) = worker();
        let (_branch, ep, _db) = ready(&w, &journal).await;

        for name in ["Al\"ice", "Alice", "a b"] {
            w.create_personal_login(&ep, name, PERSONAL_VERIFIER).await.unwrap();
            let made = creating_scripts(&w, name);
            assert_eq!(made.len(), 1, "{name}: {:?}", sql(&w));
            assert!(made[0].2.contains(&mark_personal_login_sql(name)), "{name}: {}", made[0].2);
        }
    }

    // `CREATE ROLE` on a name that exists fails and nothing else runs: the worker never adopts
    // a customer's role (no IF NOT EXISTS, no ALTER of the existing one, no marker alone).
    #[tokio::test]
    async fn an_existing_name_is_never_adopted() {
        let (w, journal) = worker();
        let (_branch, ep, _db) = ready(&w, &journal).await;
        w.computes.fail_when("CREATE ROLE \"bob\"");

        let err = w.create_personal_login(&ep, "bob", PERSONAL_VERIFIER).await;

        assert!(err.is_err());
        let ran = sql(&w);
        assert!(
            !ran.iter().any(|s| s.contains("\"bob\"")),
            "nothing about bob ran after the refusal: {ran:?}"
        );
        let first = journal.lock().unwrap().clone();
        assert!(!first.iter().any(|e| e.contains("metadata.insert")), "{first:?}");
    }

    #[tokio::test]
    async fn the_create_script_never_adopts_or_skips() {
        let (w, journal) = worker();
        let (_branch, ep, _db) = ready(&w, &journal).await;

        w.create_personal_login(&ep, "carol", PERSONAL_VERIFIER).await.unwrap();

        let script = creating_scripts(&w, "carol").remove(0).2.to_ascii_uppercase();
        for forbidden in ["IF NOT EXISTS", "DO $", "DO '", "ON CONFLICT", "EXCEPTION"] {
            assert!(!script.contains(forbidden), "{forbidden} in {script}");
        }
        // The existence check of `create_role` is not used either: the CREATE itself decides.
        assert!(
            !sql(&w).iter().any(|s| s.contains("FROM pg_catalog.pg_roles WHERE rolname = 'carol'")),
            "{:?}",
            sql(&w)
        );
    }

    #[tokio::test]
    async fn a_personal_login_needs_a_scram_verifier_and_a_name_it_may_take() {
        let (w, journal) = worker();
        let (_branch, ep, _db) = ready(&w, &journal).await;

        for bad in ["", "md5000102030405060708090a0b0c0d0e0f", "plaintext"] {
            assert!(w.create_personal_login(&ep, "dave", bad).await.is_err(), "{bad:?}");
        }
        // Names the platform keeps for itself are refused before any SQL.
        for name in ["postgres", "cloud_admin", "chelabase_realtime", "cb_m_0123", "pg_monitor", "neon_superuser", "owner"] {
            assert!(w.create_personal_login(&ep, name, PERSONAL_VERIFIER).await.is_err(), "{name}");
        }
        assert!(creating_scripts(&w, "dave").is_empty());
        assert!(
            !sql(&w).iter().any(|s| s.contains("CREATE ROLE")),
            "a refusal runs no SQL: {:?}",
            sql(&w)
        );
    }

    // K6 H5: a personal login is dropped like any non-owner row, objects first. A `GRANTED BY`
    // dependency blocks DROP ROLE, so REASSIGN OWNED and DROP OWNED (in every database) come
    // before it, in that order.
    #[tokio::test]
    async fn deleting_a_personal_login_reassigns_and_drops_owned_first() {
        let (w, journal) = worker();
        let (branch, ep, db) = ready(&w, &journal).await;
        {
            let mut state = w.metadata.state();
            let endpoint = state.endpoints.get_mut(&ep).unwrap();
            endpoint.set_verifier("alice", Some(PERSONAL_VERIFIER));
            endpoint.logins.insert(
                "alice".into(),
                FakeLogin {
                    origin: RoleOrigin::Personal,
                    password_enc: None,
                    pending: false,
                    login: true,
                    level: RoleLevel::Plain,
                    api_owner_level: false,
                    attributes: chelabase_metadata::RoleAttributes::default(),
                    attributes_pending: false,
                    attributes_change_id: None,
                },
            );
        }
        w.computes.answer(
            "SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'alice'",
            "1\n",
        );
        w.computes.answer("FROM pg_database", &databases(&["app", "other"]));

        assert_eq!(w.delete_role(branch, "alice").await, RoleOutcome::Ok);

        let ran = sql_on(&w);
        let at = |needle: &str, d: &str| {
            ran.iter()
                .position(|(database, s)| s.contains(needle) && database == d)
                .unwrap_or_else(|| panic!("{needle:?} in {d} missing from {ran:?}"))
        };
        let reassign = "REASSIGN OWNED BY \"alice\" TO postgres; DROP OWNED BY \"alice\";";
        let order = [at(reassign, "app"), at(reassign, "other"), at("DROP ROLE IF EXISTS \"alice\";", &db)];
        assert!(order.is_sorted(), "{ran:?}");
        assert!(login(&w, &ep, "alice").is_none(), "the row goes");
    }
}
