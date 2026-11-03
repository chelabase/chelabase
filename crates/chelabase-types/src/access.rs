//! Access levels for members (see the members-and-roles design, parts 1 and 2).
//!
//! Pure: the effective-grant algebra and [`decide`]. Loading members and grants lives in the
//! orchestrator; nothing here touches I/O.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::mode::Mode;
use crate::mode_policy::ModePolicy;

/// A member's platform role on a project. Ordered: Viewer < Developer < Admin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum PlatformRole {
    Viewer,
    Developer,
    Admin,
}

impl PlatformRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Developer => "developer",
            Self::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "viewer" => Some(Self::Viewer),
            "developer" => Some(Self::Developer),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
}

/// A Postgres role in an effective grant: the database owner (implied by Admin, never stored
/// in grant rows), or a named role.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PgRole {
    Owner,
    Named(String),
}

impl PgRole {
    /// The word that stands for the database owner (`?role=owner`, `extra.required`).
    pub const OWNER: &'static str = "owner";

    /// Reads a `pg_roles` entry: `owner` is the owner, anything else a named role.
    pub fn parse(s: &str) -> Self {
        if s == Self::OWNER {
            Self::Owner
        } else {
            Self::Named(s.to_string())
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Owner => Self::OWNER,
            Self::Named(name) => name,
        }
    }
}

/// The Postgres roles a grant allows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoleSet {
    /// Every role of the branch, the owner included: an Admin's grant (a project Admin's,
    /// ruling R17, and a tenant admin's), never below Admin. It doesn't hold the data API's
    /// `anon` and `authenticated` for acting as them (R18) unless a source named them: `plus`
    /// holds only those names (MH-3), and [`RoleSet::may_act_as`] honours them.
    All { plus: BTreeSet<String> },
    Only(BTreeSet<PgRole>),
}

impl RoleSet {
    /// Every role, no data API role.
    pub fn all() -> Self {
        Self::All { plus: BTreeSet::new() }
    }

    /// Whether the set holds `r`. `All` holds every role except a data API role `plus` doesn't
    /// name (R18).
    pub fn contains(&self, r: &PgRole) -> bool {
        match (self, r) {
            (Self::All { plus }, PgRole::Named(name)) if data_api_role(name) => plus.contains(name),
            (Self::All { .. }, _) => true,
            (Self::Only(set), r) => set.contains(r),
        }
    }

    /// Whether a grant with these roles lets its holder act as the named role `name` (R18):
    /// never a name [`act_as_reserved`] refuses; `All` is every other role except the data
    /// API's `anon` and `authenticated`, which need a source naming them (`plus`).
    pub fn may_act_as(&self, name: &str) -> bool {
        if act_as_reserved(name) {
            return false;
        }
        match self {
            Self::All { plus } => !data_api_role(name) || plus.contains(name),
            Self::Only(set) => set.contains(&PgRole::Named(name.to_string())),
        }
    }

    /// `All ∩ X = X` (the first `All` when both are).
    pub fn intersect(&self, other: &RoleSet) -> RoleSet {
        match (self, other) {
            (Self::All { .. }, x) | (x, Self::All { .. }) => x.clone(),
            (Self::Only(a), Self::Only(b)) => Self::Only(a.intersection(b).cloned().collect()),
        }
    }
}

/// One source of access: a project grant (a team grant in step 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub platform_role: PlatformRole,
    pub roles: RoleSet,
}

/// An override on a Chelabase database. It only narrows; a `None`
/// field leaves that part as it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Narrowing {
    pub platform_role: Option<PlatformRole>,
    pub roles: Option<BTreeSet<PgRole>>,
}

/// Every source of a member's access on one project, and their override on one Chelabase
/// database (spec Part 5, MH-2). There is no public own-grant field: [`GrantSources::effective`]
/// is the only way to a grant, so no caller can read the own grant alone and miss a team's:
///
/// ```compile_fail
/// let sources = chelabase_types::access::GrantSources::default();
/// let _own = sources.own; // private: read access through `effective()` only
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrantSources {
    own: Option<Grant>,
    teams: Vec<Grant>,
    database: Option<Narrowing>,
}

impl GrantSources {
    pub fn new(own: Option<Grant>, teams: Vec<Grant>, database: Option<Narrowing>) -> Self {
        Self { own, teams, database }
    }

    /// The same sources with `database` as the override (T2-4: a project's sources narrowed
    /// for one of its databases).
    pub fn narrowed(&self, database: Option<Narrowing>) -> Self {
        Self { own: self.own.clone(), teams: self.teams.clone(), database }
    }

    /// Union then narrowing (D1). `None` with no source, whatever the override says (an
    /// override alone is no access, never a default level).
    ///
    /// - **Union.** The platform level is the highest source level. Each role keeps its own
    ///   level: the highest among the sources naming it. An Admin source names every role
    ///   except the data API's `anon`/`authenticated` (R17, R18), which only a source listing
    ///   them names (MH-3). Entries no grant may hold (`owner`, every name [`act_as_reserved`]
    ///   refuses) are dropped defensively.
    /// - **Narrowing.** The override caps the platform level and every role's level; its role
    ///   list intersects. While the union holds every role (an Admin source), the override's
    ///   list is kept as it is, each at the capped level (R17: "its roles alone are kept").
    ///   Narrowed below Admin without a list, "every role" is gone and only the roles a source
    ///   named explicitly stay (so adding a source never takes a role away).
    /// - **Shapes.** Admin from any source, no narrowing below Admin and no list: `All { plus }`.
    ///   Otherwise `Only`, holding the owner exactly when the level is Admin (R7).
    pub fn effective(&self) -> Option<Effective> {
        let sources: Vec<&Grant> = self.own.iter().chain(&self.teams).collect();
        let platform = sources.iter().map(|g| g.platform_role).max()?;
        let every = platform == PlatformRole::Admin;
        // The roles each source names explicitly, at the best level among them.
        let mut explicit: BTreeMap<String, PlatformRole> = BTreeMap::new();
        for g in &sources {
            let named: Vec<&str> = match &g.roles {
                RoleSet::Only(set) => set.iter().filter_map(grantable_name).collect(),
                RoleSet::All { plus } => plus.iter().map(String::as_str).filter(|n| data_api_role(n)).collect(),
            };
            for n in named {
                let level = explicit.entry(n.to_string()).or_insert(g.platform_role);
                *level = (*level).max(g.platform_role);
            }
        }
        let cap = self.database.as_ref().and_then(|n| n.platform_role);
        let capped = |l: PlatformRole| cap.map_or(l, |c| l.min(c));
        let list: Option<BTreeSet<&str>> = self
            .database
            .as_ref()
            .and_then(|n| n.roles.as_ref())
            .map(|set| set.iter().filter_map(grantable_name).collect());
        let level = capped(platform);

        if every && list.is_none() && level == PlatformRole::Admin {
            let role_levels: BTreeMap<String, PlatformRole> =
                explicit.into_iter().filter(|(n, _)| data_api_role(n)).collect();
            let plus = role_levels.keys().cloned().collect();
            return Some(Effective { platform_role: level, roles: RoleSet::All { plus }, role_levels });
        }
        let role_levels: BTreeMap<String, PlatformRole> = match list {
            // Every role ∩ the list: the list, each at the (capped) Admin level.
            Some(list) if every => list.into_iter().map(|n| (n.to_string(), level)).collect(),
            Some(list) => explicit
                .into_iter()
                .filter(|(n, _)| list.contains(n.as_str()))
                .map(|(n, l)| (n, capped(l)))
                .collect(),
            // No list: what a source named stays; an Admin source names every non-data-API
            // role at Admin.
            None => explicit
                .into_iter()
                .map(|(n, l)| {
                    let best = if every && !data_api_role(&n) { PlatformRole::Admin } else { l };
                    (n, capped(best))
                })
                .collect(),
        };
        Some(Effective::only(level, role_levels))
    }
}

/// The access that applies to a member on a project (or, once narrowed, on one database): the
/// platform level, the role set, and each held role's own level (D1, MH-1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effective {
    pub platform_role: PlatformRole,
    pub roles: RoleSet,
    /// `Only`: every held named role and its level. `All`: the `plus` names' levels (every
    /// other role is held at the platform level).
    role_levels: BTreeMap<String, PlatformRole>,
}

impl Effective {
    /// `Only` with these named roles, and the owner exactly when the level is Admin (R7).
    fn only(platform_role: PlatformRole, role_levels: BTreeMap<String, PlatformRole>) -> Self {
        let roles = role_set(platform_role, role_levels.keys());
        Self { platform_role, roles, role_levels }
    }

    /// Each named role and its level (D1): under `All` only the `plus` names (the data API
    /// roles a source names); every other role is held at the platform level.
    pub fn role_levels(&self) -> &BTreeMap<String, PlatformRole> {
        &self.role_levels
    }

    /// The level the member holds `name` at (`All`: the platform level for any name
    /// [`RoleSet::may_act_as`] accepts); `None` when not held. Never a reserved name.
    pub fn level_of(&self, name: &str) -> Option<PlatformRole> {
        if act_as_reserved(name) {
            return None;
        }
        match &self.roles {
            RoleSet::All { .. } if !data_api_role(name) => Some(self.platform_role),
            _ => self.role_levels.get(name).copied(),
        }
    }

    /// The roles held at `min` or above (personal logins: `Developer`, D1). The owner stays
    /// exactly when the level is Admin.
    pub fn roles_at_least(&self, min: PlatformRole) -> RoleSet {
        let held = self.role_levels.iter().filter(|(_, l)| **l >= min).map(|(n, _)| n);
        match &self.roles {
            RoleSet::All { .. } => RoleSet::All { plus: held.cloned().collect() },
            RoleSet::Only(_) => role_set(self.platform_role, held),
        }
    }
}

/// `Only(names)`, plus the owner when `level` is Admin.
fn role_set<'a>(level: PlatformRole, names: impl Iterator<Item = &'a String>) -> RoleSet {
    let mut set: BTreeSet<PgRole> = names.map(|n| PgRole::Named(n.clone())).collect();
    if level == PlatformRole::Admin {
        set.insert(PgRole::Owner);
    }
    RoleSet::Only(set)
}

/// A named role a grant may hold (data API names included), or `None`.
fn grantable_name(r: &PgRole) -> Option<&str> {
    match r {
        PgRole::Owner => None,
        PgRole::Named(name) if act_as_reserved(name) => None,
        PgRole::Named(name) => Some(name),
    }
}

/// MH-17: whether a member may hold a personal login: active (not suspended) and an effective
/// level of at least Developer (tenant admins, judged with [`tenant_admin_effective`],
/// included).
pub fn qualifies(active: bool, effective: Option<&Effective>) -> bool {
    active && effective.is_some_and(|e| e.platform_role >= PlatformRole::Developer)
}

/// Names no member may act as by name: `owner` (any case), every reserved role name and
/// prefix (`pg_`, `cb_m_`, `chelabase_`, `postgres`, `authenticator`, …; any case), except the
/// data API's own `anon` and `authenticated`, which the console and SQL editor run as.
/// The owner is reached only as [`Requested::Owner`].
pub fn act_as_reserved(name: &str) -> bool {
    if data_api_role(name) {
        return false;
    }
    name.eq_ignore_ascii_case(PgRole::OWNER) || crate::role_name::is_membership_reserved(name)
}

/// The data API's own roles, `anon` and `authenticated`: a member acts as them only with a
/// grant naming them, Admins included (R18).
pub fn data_api_role(name: &str) -> bool {
    name == "anon" || name == "authenticated"
}

/// A tenant admin's access on every project: Admin with every role, the owner included, no
/// data API role (R18). A project Admin's grant without an override (R17) has the same shape;
/// only a tenant admin's is never narrowed.
pub fn tenant_admin_effective() -> Effective {
    Effective { platform_role: PlatformRole::Admin, roles: RoleSet::all(), role_levels: BTreeMap::new() }
}

/// Who may call a route (the `access` field of every endpoint's policy).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// No identity.
    Public,
    /// A data API key, already checked by the gateway.
    DataKey,
    /// Any verified identity.
    SignedIn,
    /// Self-hosted: an active tenant admin. Platform: a member of the operator tenant.
    Instance,
    /// Any active member of the caller's tenant.
    TenantMember,
    /// An active tenant admin.
    TenantAdmin,
    /// Effective platform role on the project at least this.
    Project(PlatformRole),
    /// The minimum depends on the target branch's protection (unknown counts as protected).
    Branch {
        unprotected: PlatformRole,
        protected: PlatformRole,
    },
    /// Effective platform role at least this, and the requested Postgres role in the
    /// effective set. The owner needs Admin.
    ActAs(PlatformRole),
    /// [`Access::Branch`] and [`Access::ActAs`] together: the minimum depends on the target
    /// branch's protection, and the requested Postgres role must be in the effective set (the
    /// owner needs Admin).
    ActAsBranch {
        unprotected: PlatformRole,
        protected: PlatformRole,
    },
}

/// The caller's membership in the tenant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberFacts {
    NotMember,
    Invited,
    /// Signed up and waiting for an admin (R40).
    Pending,
    Removed,
    /// Suspended by a tenant admin (P14): refused everywhere but the public routes.
    Suspended,
    /// D7: the member was offboarded (the mark their removal sets): refused everywhere but the
    /// public routes, like [`MemberFacts::Suspended`].
    Offboarded,
    Active {
        tenant_admin: bool,
        /// Whether the member has a grant on any project of the tenant.
        has_any_grant: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallerFacts {
    pub member: MemberFacts,
    /// Platform mode: whether the caller is an active member of `CHELABASE_OPERATOR_TENANT_ID`.
    pub in_operator_org: bool,
}

/// The Postgres role an act-as request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requested<'a> {
    Owner,
    Role(&'a str),
}

impl<'a> Requested<'a> {
    /// Reads a requested role name (`?role=`, a console or SQL editor request): `owner` in any
    /// case, or the database's real owner role (`owner_role`, e.g. `postgres`), is the owner;
    /// anything else is a named role. An empty `owner_role` (unknown) matches nothing.
    pub fn from_name(name: &'a str, owner_role: &str) -> Requested<'a> {
        if name.eq_ignore_ascii_case(PgRole::OWNER) || (!owner_role.is_empty() && name == owner_role) {
            Requested::Owner
        } else {
            Requested::Role(name)
        }
    }
}

/// Why a request is refused (mapped to the API answers by the orchestrator).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deny {
    /// 404 `not_found`: existence isn't revealed.
    NotFound,
    /// 403 `forbidden`, with `extra.required`.
    Forbidden { required: String },
    /// 403 `no_access_yet`.
    NoAccessYet,
    /// 403 `not_available_in_platform_mode`.
    NotAvailableInPlatformMode,
    /// 403 `pending_approval`.
    PendingApproval,
    /// 403 `member_suspended` (P14).
    Suspended,
    /// 403 `member_offboarded` (D7).
    Offboarded,
}

/// Decides whether a caller may use a route.
///
/// - `grant`: the caller's effective access on the route's project
///   ([`GrantSources::effective`]); `None` when they have none there. An active tenant admin is
///   always judged with [`tenant_admin_effective`], whatever is passed.
/// - An act-as of a named role needs the role in the effective set **at the route's minimum
///   level**: its own level (D1, MH-1), not only the member's platform level.
/// - `branch_protected`: the target branch's protection, for `Branch` routes (for merges, the
///   branch merged into). `None` counts as protected.
/// - `requested`: the act-as role (`None` = the owner). Build it with
///   [`Requested::from_name`]; `decide` also maps `Role("owner")` and `Role(owner_role)` to the
///   owner itself.
/// - `owner_role`: the database's owner role name (`databases.owner_role`, e.g. `postgres`),
///   for `ActAs` routes; `None` elsewhere or when unknown (then only `owner` names the owner).
///   A `Role(n)` that [`act_as_reserved`] refuses is `Forbidden{"role:<n>"}` even if listed.
/// - `Instance` is checked first: a platform operator passes without tenant membership.
#[allow(clippy::too_many_arguments)]
pub fn decide(
    policy: &ModePolicy,
    caller: &CallerFacts,
    grant: Option<&Effective>,
    branch_protected: Option<bool>,
    requested: Option<Requested<'_>>,
    owner_role: Option<&str>,
    access: Access,
) -> Result<(), Deny> {
    let forbidden = |required: &str| Err(Deny::Forbidden { required: required.to_string() });

    if matches!(access, Access::Public | Access::DataKey) {
        return Ok(());
    }
    // P14 (T2-3), D7: a suspended or offboarded member is refused the signed-in routes too;
    // `/v1/me`, which tells them where they stand, doesn't come here.
    if access == Access::SignedIn {
        return match caller.member {
            MemberFacts::Suspended => Err(Deny::Suspended),
            MemberFacts::Offboarded => Err(Deny::Offboarded),
            _ => Ok(()),
        };
    }
    let active_tenant_admin = matches!(caller.member, MemberFacts::Active { tenant_admin: true, .. });
    if access == Access::Instance && policy.instance_admin(active_tenant_admin, caller.in_operator_org) {
        return Ok(());
    }
    let (tenant_admin, has_any_grant) = match caller.member {
        MemberFacts::Active { tenant_admin, has_any_grant } => (tenant_admin, has_any_grant),
        MemberFacts::Removed => return Err(Deny::NotFound),
        MemberFacts::Suspended => return Err(Deny::Suspended),
        // D7: refused like a suspended member, with its own answer.
        MemberFacts::Offboarded => return Err(Deny::Offboarded),
        MemberFacts::Pending => return Err(Deny::PendingApproval),
        MemberFacts::NotMember | MemberFacts::Invited => {
            return Err(match policy.mode() {
                Mode::SelfHosted => Deny::NoAccessYet,
                Mode::Platform => Deny::NotAvailableInPlatformMode,
            });
        }
    };
    let min = match access {
        Access::Public | Access::DataKey | Access::SignedIn | Access::TenantMember => return Ok(()),
        // Passing callers returned above.
        Access::Instance => return forbidden("instance admin"),
        Access::TenantAdmin => return if tenant_admin { Ok(()) } else { forbidden("tenant admin") },
        Access::Project(min) | Access::ActAs(min) => min,
        Access::Branch { unprotected, protected } | Access::ActAsBranch { unprotected, protected } => {
            if branch_protected.unwrap_or(true) {
                protected
            } else {
                unprotected
            }
        }
    };
    let admin_grant;
    let grant = if tenant_admin {
        admin_grant = tenant_admin_effective();
        &admin_grant
    } else {
        match grant {
            Some(grant) => grant,
            None => return Err(if has_any_grant { Deny::NotFound } else { Deny::NoAccessYet }),
        }
    };
    if grant.platform_role < min {
        return forbidden(min.as_str());
    }
    if !matches!(access, Access::ActAs(_) | Access::ActAsBranch { .. }) {
        return Ok(());
    }
    let requested = match requested {
        None | Some(Requested::Owner) => Requested::Owner,
        Some(Requested::Role(name)) => Requested::from_name(name, owner_role.unwrap_or("")),
    };
    match requested {
        // Admin implies the owner (R7).
        Requested::Owner => {
            if grant.platform_role == PlatformRole::Admin {
                Ok(())
            } else {
                forbidden(PgRole::OWNER)
            }
        }
        // R18: only granted roles; `All` doesn't hold the data API's roles, a source must name
        // them. D1: the role's own level must reach the route's minimum.
        Requested::Role(name) => {
            if grant.roles.may_act_as(name) && grant.level_of(name).is_some_and(|l| l >= min) {
                Ok(())
            } else {
                forbidden(&format!("role:{name}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_role_order_and_round_trip() {
        use PlatformRole::*;
        assert!(Viewer < Developer && Developer < Admin);
        for r in [Viewer, Developer, Admin] {
            assert_eq!(PlatformRole::parse(r.as_str()), Some(r));
            assert_eq!(
                serde_json::to_value(r).unwrap(),
                serde_json::json!(r.as_str())
            );
            let back: PlatformRole = serde_json::from_value(serde_json::json!(r.as_str())).unwrap();
            assert_eq!(back, r);
        }
        assert_eq!(PlatformRole::parse("Admin"), None);
        assert_eq!(PlatformRole::parse(""), None);
    }

    fn set(names: &[&str]) -> BTreeSet<PgRole> {
        names.iter().map(|n| PgRole::parse(n)).collect()
    }

    fn only(names: &[&str]) -> RoleSet {
        RoleSet::Only(set(names))
    }

    fn grant(platform_role: PlatformRole, names: &[&str]) -> Grant {
        Grant { platform_role, roles: only(names) }
    }

    /// A project Admin's row (R17): every role.
    fn admin_row() -> Grant {
        Grant { platform_role: PlatformRole::Admin, roles: RoleSet::all() }
    }

    /// The effective access of one own grant and an optional override.
    fn eff(own: &Grant, db: Option<&Narrowing>) -> Effective {
        GrantSources::new(Some(own.clone()), Vec::new(), db.cloned()).effective().expect("a source")
    }

    /// An effective access's shape, as a grant.
    fn shape(e: &Effective) -> Grant {
        Grant { platform_role: e.platform_role, roles: e.roles.clone() }
    }

    #[test]
    fn pg_role_parses_the_owner_literal() {
        assert_eq!(PgRole::parse("owner"), PgRole::Owner);
        assert_eq!(PgRole::parse("Owner"), PgRole::Named("Owner".into()));
        assert_eq!(PgRole::parse("billing"), PgRole::Named("billing".into()));
        assert_eq!(PgRole::Owner.as_str(), "owner");
        assert_eq!(PgRole::Named("app".into()).as_str(), "app");
    }

    #[test]
    fn requested_from_name_maps_the_owner() {
        assert_eq!(Requested::from_name("owner", "postgres"), Requested::Owner);
        assert_eq!(Requested::from_name("OWNER", "postgres"), Requested::Owner);
        assert_eq!(Requested::from_name("postgres", "postgres"), Requested::Owner);
        assert_eq!(Requested::from_name("app_owner", "app_owner"), Requested::Owner);
        assert_eq!(Requested::from_name("postgres", "app_owner"), Requested::Role("postgres"));
        assert_eq!(Requested::from_name("billing", "postgres"), Requested::Role("billing"));
        // An empty owner name (unknown) never matches.
        assert_eq!(Requested::from_name("", ""), Requested::Role(""));
    }

    #[test]
    fn role_set_contains_and_intersect() {
        let a = only(&["owner", "billing", "sales"]);
        let b = only(&["billing", "app"]);
        let all = RoleSet::all();
        assert!(all.contains(&PgRole::Owner));
        assert!(all.contains(&PgRole::Named("x".into())));
        // R18: every role but the data API's, unless named (MH-3).
        assert!(!all.contains(&PgRole::Named("anon".into())));
        let plus = RoleSet::All { plus: ["anon".to_string()].into() };
        assert!(plus.contains(&PgRole::Named("anon".into())));
        assert!(!plus.contains(&PgRole::Named("authenticated".into())));
        assert!(a.contains(&PgRole::Owner));
        assert!(!b.contains(&PgRole::Owner));
        assert_eq!(all.intersect(&a), a);
        assert_eq!(a.intersect(&all), a);
        assert_eq!(all.intersect(&all), all);
        assert_eq!(a.intersect(&b), only(&["billing"]));
    }

    #[test]
    fn tenant_admin_effective_is_admin_with_every_role() {
        let e = tenant_admin_effective();
        assert_eq!(shape(&e), admin_row());
        assert_eq!(e, eff(&admin_row(), None));
    }

    #[test]
    fn effective_examples() {
        use PlatformRole::*;
        // R17: an Admin source is every role (the owner included); a row never stores it.
        assert_eq!(shape(&eff(&admin_row(), None)), admin_row());
        assert_eq!(shape(&eff(&grant(Admin, &["billing", "sales"]), None)), admin_row());
        // A database override down to Developer removes the owner and every role there.
        let db = Narrowing { platform_role: Some(Developer), roles: None };
        assert_eq!(shape(&eff(&admin_row(), Some(&db))), grant(Developer, &[]));
        // A roles-only override keeps Admin, and so the owner.
        let roles_only = Narrowing { platform_role: None, roles: Some(set(&["sales"])) };
        assert_eq!(shape(&eff(&admin_row(), Some(&roles_only))), grant(Admin, &["owner", "sales"]));
        assert_eq!(shape(&eff(&grant(Developer, &["billing", "sales"]), Some(&roles_only))), grant(Developer, &["sales"]));
        // A role and a roles override together: the lower role, the intersected roles.
        let both = Narrowing { platform_role: Some(Viewer), roles: Some(set(&["sales", "app"])) };
        assert_eq!(shape(&eff(&grant(Developer, &["billing", "sales"]), Some(&both))), grant(Viewer, &["sales"]));
        // R17: every role ∩ a list is the list (only the callers keep a tenant admin's grant
        // unnarrowed).
        let both = Narrowing { platform_role: Some(Developer), roles: Some(set(&["sales", "app"])) };
        assert_eq!(shape(&eff(&admin_row(), Some(&both))), grant(Developer, &["sales", "app"]));
        let admin_level = Narrowing { platform_role: Some(Admin), roles: None };
        assert_eq!(shape(&eff(&admin_row(), Some(&admin_level))), admin_row());
        // `All` is only valid with Admin: anything else is read as no roles.
        let bad_all = Grant { platform_role: Developer, roles: RoleSet::all() };
        assert_eq!(shape(&eff(&bad_all, None)), grant(Developer, &[]));
        // Invalid row entries are dropped: `owner` (any case), the owner's name, reserved names
        // and prefixes. The data API roles stay.
        let bad = grant(
            Developer,
            &["owner", "Owner", "postgres", "cb_m_x", "chelabase_x", "pg_monitor", "authenticator", "billing", "anon", "authenticated"],
        );
        assert_eq!(shape(&eff(&bad, None)), grant(Developer, &["billing", "anon", "authenticated"]));
        let listed = Narrowing { platform_role: None, roles: Some(set(&["postgres", "cb_m_x", "owner", "billing"])) };
        assert_eq!(shape(&eff(&admin_row(), Some(&listed))), grant(Admin, &["owner", "billing"]));
    }

    // ---- decide_matrix_exhaustive -------------------------------------------------------
    //
    // The expectations are hand-written tables, read from the spec (parts 2 and 4 and rulings
    // R6/R7), not derived from `decide`'s steps.

    /// One caller shape and the grant passed to `decide` (already effective, or raw).
    struct Kind {
        name: &'static str,
        member: MemberFacts,
        operator: bool,
        grant: Option<Effective>,
        /// Outcomes for [Instance, TenantMember, TenantAdmin, Project(V), Project(D),
        /// Project(A)], self-hosted then platform. `.` ok, `N` not_found, `Y` no_access_yet,
        /// `P` not_available_in_platform_mode, `I` forbidden "instance admin", `T` forbidden
        /// "tenant admin", `v`/`d`/`a` forbidden "viewer"/"developer"/"admin".
        levels: [&'static str; 2],
        /// Outcomes per entry of `REQUESTS` once the level passes: `.` ok, `o` forbidden
        /// "owner", `r` forbidden "role:<name>". Empty when the kind has no grant.
        requests: &'static str,
    }

    /// `None` means no role asked (the owner). `owner_role` is `postgres` throughout.
    const REQUESTS: [Option<Requested<'static>>; 10] = [
        None,
        Some(Requested::Owner),
        Some(Requested::Role("billing")),
        Some(Requested::Role("sales")),
        Some(Requested::Role("owner")),
        Some(Requested::Role("postgres")),
        Some(Requested::Role("cb_m_x")),
        Some(Requested::Role("chelabase_x")),
        Some(Requested::Role("anon")),
        Some(Requested::Role("authenticator")),
    ];

    const fn active(tenant_admin: bool, has_any_grant: bool) -> MemberFacts {
        MemberFacts::Active { tenant_admin, has_any_grant }
    }

    fn kinds() -> Vec<Kind> {
        use PlatformRole::*;
        // R17: a project Admin's grant is every role.
        let row_admin = eff(&admin_row(), None);
        let admin_to_dev = eff(&admin_row(), Some(&Narrowing { platform_role: Some(Developer), roles: None }));
        // A source listing names no grant may hold.
        let raw_dev = eff(&grant(Developer, &["billing", "owner", "postgres", "cb_m_x", "chelabase_x", "authenticator"]), None);
        vec![
            Kind { name: "operator, active, no grants", member: active(false, false), operator: true, grant: None,
                levels: ["I.TYYY", "..TYYY"], requests: "" },
            Kind { name: "tenant admin, grant None", member: active(true, true), operator: false, grant: None,
                levels: ["......", "I....."], requests: "......rrrr" },
            Kind { name: "tenant admin, all-roles grant", member: active(true, true), operator: false, grant: Some(tenant_admin_effective()),
                levels: ["......", "I....."], requests: "......rrrr" },
            Kind { name: "tenant admin, narrowed grant", member: active(true, false), operator: false, grant: Some(eff(&grant(Viewer, &["billing"]), None)),
                levels: ["......", "I....."], requests: "......rrrr" },
            Kind { name: "admin", member: active(false, true), operator: false, grant: Some(row_admin),
                levels: ["I.T...", "I.T..."], requests: "......rrrr" },
            Kind { name: "admin overridden to developer", member: active(false, true), operator: false, grant: Some(admin_to_dev),
                levels: ["I.T..a", "I.T..a"], requests: "oorroorrrr" },
            Kind { name: "developer, raw grant", member: active(false, true), operator: false, grant: Some(raw_dev),
                levels: ["I.T..a", "I.T..a"], requests: "oo.roorrrr" },
            Kind { name: "developer", member: active(false, true), operator: false, grant: Some(eff(&grant(Developer, &["billing"]), None)),
                levels: ["I.T..a", "I.T..a"], requests: "oo.roorrrr" },
            Kind { name: "viewer", member: active(false, true), operator: false, grant: Some(eff(&grant(Viewer, &["billing"]), None)),
                levels: ["I.T.da", "I.T.da"], requests: "oo.roorrrr" },
            Kind { name: "active, no grant here", member: active(false, true), operator: false, grant: None,
                levels: ["I.TNNN", "I.TNNN"], requests: "" },
            Kind { name: "active, no grants anywhere", member: active(false, false), operator: false, grant: None,
                levels: ["I.TYYY", "I.TYYY"], requests: "" },
            Kind { name: "invited", member: MemberFacts::Invited, operator: false, grant: None,
                levels: ["YYYYYY", "PPPPPP"], requests: "" },
            Kind { name: "pending", member: MemberFacts::Pending, operator: false, grant: None,
                levels: ["WWWWWW", "WWWWWW"], requests: "" },
            Kind { name: "pending operator", member: MemberFacts::Pending, operator: true, grant: None,
                levels: ["WWWWWW", ".WWWWW"], requests: "" },
            Kind { name: "removed", member: MemberFacts::Removed, operator: false, grant: None,
                levels: ["NNNNNN", "NNNNNN"], requests: "" },
            Kind { name: "removed operator", member: MemberFacts::Removed, operator: true, grant: None,
                levels: ["NNNNNN", ".NNNNN"], requests: "" },
            Kind { name: "non-member", member: MemberFacts::NotMember, operator: false, grant: None,
                levels: ["YYYYYY", "PPPPPP"], requests: "" },
            Kind { name: "non-member operator", member: MemberFacts::NotMember, operator: true, grant: None,
                levels: ["YYYYYY", ".PPPPP"], requests: "" },
        ]
    }

    fn outcome(code: u8, requested: Option<Requested<'_>>) -> Result<(), Deny> {
        let forbidden = |s: &str| Err(Deny::Forbidden { required: s.to_string() });
        match code {
            b'.' => Ok(()),
            b'N' => Err(Deny::NotFound),
            b'Y' => Err(Deny::NoAccessYet),
            b'P' => Err(Deny::NotAvailableInPlatformMode),
            b'W' => Err(Deny::PendingApproval),
            b'I' => forbidden("instance admin"),
            b'T' => forbidden("tenant admin"),
            b'v' => forbidden("viewer"),
            b'd' => forbidden("developer"),
            b'a' => forbidden("admin"),
            b'o' => forbidden("owner"),
            b'r' => match requested {
                Some(Requested::Role(name)) => forbidden(&format!("role:{name}")),
                other => panic!("`r` needs a named request, got {other:?}"),
            },
            other => panic!("unknown code {}", other as char),
        }
    }

    fn level_column(r: PlatformRole) -> usize {
        match r {
            PlatformRole::Viewer => 3,
            PlatformRole::Developer => 4,
            PlatformRole::Admin => 5,
        }
    }

    fn expected(access: Access, mode: usize, k: &Kind, protected: Option<bool>, req_index: usize) -> Result<(), Deny> {
        let req = REQUESTS[req_index];
        let levels = k.levels[mode].as_bytes();
        let level = |col: usize| outcome(levels[col], req);
        match access {
            Access::Public | Access::DataKey | Access::SignedIn => Ok(()),
            Access::Instance => level(0),
            Access::TenantMember => level(1),
            Access::TenantAdmin => level(2),
            Access::Project(m) => level(level_column(m)),
            // Unknown protection counts as protected.
            Access::Branch { unprotected, protected: p } => {
                level(level_column(if protected == Some(false) { unprotected } else { p }))
            }
            Access::ActAs(m) => {
                level(level_column(m))?;
                outcome(k.requests.as_bytes()[req_index], req)
            }
            Access::ActAsBranch { unprotected, protected: p } => {
                level(level_column(if protected == Some(false) { unprotected } else { p }))?;
                outcome(k.requests.as_bytes()[req_index], req)
            }
        }
    }

    fn every_access() -> Vec<Access> {
        use PlatformRole::*;
        let roles = [Viewer, Developer, Admin];
        let mut all = vec![
            Access::Public,
            Access::DataKey,
            Access::SignedIn,
            Access::Instance,
            Access::TenantMember,
            Access::TenantAdmin,
        ];
        all.extend(roles.map(Access::Project));
        for unprotected in roles {
            for protected in roles {
                all.push(Access::Branch { unprotected, protected });
            }
        }
        for unprotected in roles {
            for protected in roles {
                all.push(Access::ActAsBranch { unprotected, protected });
            }
        }
        all.extend(roles.map(Access::ActAs));
        all
    }

    #[test]
    fn decide_matrix_exhaustive() {
        let kinds = kinds();
        for k in &kinds {
            assert!(k.requests.is_empty() || k.requests.len() == REQUESTS.len(), "{}", k.name);
        }
        let mut cases = 0;
        for access in every_access() {
            for (mode_index, mode) in [Mode::SelfHosted, Mode::Platform].into_iter().enumerate() {
                let policy = ModePolicy::new(mode);
                for k in &kinds {
                    let caller = CallerFacts { member: k.member, in_operator_org: k.operator };
                    for protected in [None, Some(false), Some(true)] {
                        for (i, requested) in REQUESTS.into_iter().enumerate() {
                            let got = decide(&policy, &caller, k.grant.as_ref(), protected, requested, Some("postgres"), access);
                            let want = expected(access, mode_index, k, protected, i);
                            assert_eq!(
                                got, want,
                                "{access:?} {mode:?} {} protected={protected:?} requested={requested:?}",
                                k.name
                            );
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 30 * 2 * kinds.len() * 3 * REQUESTS.len());
    }

    // D7 (final round A): an offboarded member is
    // refused exactly where a suspended one is, with its own answer: every access but `Public`
    // and `DataKey`, `SignedIn` included (`GET /v1/me` doesn't come here), whatever grant,
    // protection, request or mode. Exhaustive over the matrix's inputs.
    #[test]
    fn an_offboarded_member_is_refused_wherever_a_suspended_one_is() {
        use PlatformRole::*;
        let grants = [
            None,
            Some(tenant_admin_effective()),
            Some(eff(&admin_row(), None)),
            Some(eff(&grant(Developer, &["billing"]), None)),
            Some(eff(&grant(Viewer, &["billing"]), None)),
        ];
        let mut cases = 0;
        for access in every_access() {
            for mode in [Mode::SelfHosted, Mode::Platform] {
                let policy = ModePolicy::new(mode);
                for operator in [false, true] {
                    for g in &grants {
                        for protected in [None, Some(false), Some(true)] {
                            for requested in REQUESTS {
                                let run = |member| {
                                    let caller = CallerFacts { member, in_operator_org: operator };
                                    decide(&policy, &caller, g.as_ref(), protected, requested, Some("postgres"), access)
                                };
                                let suspended = run(MemberFacts::Suspended);
                                let want = match suspended {
                                    Err(Deny::Suspended) => Err(Deny::Offboarded),
                                    other => other,
                                };
                                let got = run(MemberFacts::Offboarded);
                                assert_eq!(
                                    got, want,
                                    "{access:?} {mode:?} operator={operator} grant={g:?} protected={protected:?} requested={requested:?}"
                                );
                                if !operator && !matches!(access, Access::Public | Access::DataKey) {
                                    assert_eq!(got, Err(Deny::Offboarded), "{access:?} {mode:?}");
                                }
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 30 * 2 * 2 * grants.len() * 3 * REQUESTS.len());
    }

    #[test]
    fn act_as_branch_needs_the_branch_level_and_the_role() {
        use PlatformRole::*;
        let policy = ModePolicy::new(Mode::SelfHosted);
        let caller = CallerFacts { member: active(false, true), in_operator_org: false };
        let dev = eff(&grant(Developer, &["billing"]), None);
        let access = Access::ActAsBranch { unprotected: Developer, protected: Admin };
        let billing = Some(Requested::Role("billing"));
        let admin = Err(Deny::Forbidden { required: "admin".into() });
        assert_eq!(decide(&policy, &caller, Some(&dev), Some(false), billing, Some("postgres"), access), Ok(()));
        assert_eq!(decide(&policy, &caller, Some(&dev), Some(true), billing, Some("postgres"), access), admin);
        assert_eq!(decide(&policy, &caller, Some(&dev), None, billing, Some("postgres"), access), admin);
        assert_eq!(
            decide(&policy, &caller, Some(&dev), Some(false), Some(Requested::Role("sales")), Some("postgres"), access),
            Err(Deny::Forbidden { required: "role:sales".into() })
        );
        let boss = eff(&grant(Admin, &["billing"]), None);
        assert_eq!(decide(&policy, &caller, Some(&boss), Some(true), billing, Some("postgres"), access), Ok(()));
    }

    #[test]
    fn a_tenant_admin_is_admin_whatever_grant_is_passed() {
        let policy = ModePolicy::new(Mode::SelfHosted);
        let caller = CallerFacts { member: active(true, false), in_operator_org: false };
        let viewer = eff(&grant(PlatformRole::Viewer, &[]), None);
        for g in [None, Some(&viewer)] {
            let d = |access| decide(&policy, &caller, g, Some(true), None, Some("postgres"), access);
            assert_eq!(d(Access::Project(PlatformRole::Admin)), Ok(()), "{g:?}");
            assert_eq!(d(Access::ActAs(PlatformRole::Admin)), Ok(()), "{g:?}");
        }
    }

    #[test]
    fn a_platform_operator_reaches_instance_routes_without_membership() {
        let policy = ModePolicy::new(Mode::Platform);
        for member in [MemberFacts::NotMember, MemberFacts::Invited, MemberFacts::Removed] {
            let caller = CallerFacts { member, in_operator_org: true };
            assert_eq!(decide(&policy, &caller, None, None, None, None, Access::Instance), Ok(()), "{member:?}");
            assert_ne!(decide(&policy, &caller, None, None, None, None, Access::TenantMember), Ok(()), "{member:?}");
        }
    }

    #[test]
    fn act_as_with_an_unknown_owner_name() {
        // Without the database's owner name, `postgres` is just a reserved name.
        let policy = ModePolicy::new(Mode::SelfHosted);
        let caller = CallerFacts { member: active(false, true), in_operator_org: false };
        let admin = eff(&grant(PlatformRole::Admin, &["billing"]), None);
        let act = |r| decide(&policy, &caller, Some(&admin), None, Some(r), None, Access::ActAs(PlatformRole::Viewer));
        assert_eq!(act(Requested::Role("postgres")), Err(Deny::Forbidden { required: "role:postgres".into() }));
        assert_eq!(act(Requested::Role("owner")), Ok(()));
        assert_eq!(act(Requested::Owner), Ok(()));
        assert_eq!(act(Requested::Role("billing")), Ok(()));
    }

    // R15 (table editor): an Admin may act as `anon` and `authenticated` without a grant of
    // them; below Admin only a grant listing them lets a member act as them.
    #[test]
    fn admins_act_as_the_data_api_roles() {
        // R17/R18 (user, 2026-10-14): only granted roles. A project Admin's grant is every role
        // (`All`) except the data API's anon and authenticated, which need a grant naming them;
        // an Admin narrowed to named roles acts only as those (and the owner).
        use PlatformRole::*;
        let policy = ModePolicy::new(Mode::SelfHosted);
        let caller = CallerFacts { member: active(false, true), in_operator_org: false };
        let act = |g: &Effective, r| decide(&policy, &caller, Some(g), None, Some(Requested::Role(r)), Some("postgres"), Access::ActAs(Viewer));
        let refused = |r: &str| Err(Deny::Forbidden { required: format!("role:{r}") });
        let admin_all = eff(&admin_row(), None);
        let admin_narrowed = eff(
            &admin_row(),
            Some(&Narrowing { platform_role: None, roles: Some([PgRole::Named("billing".into())].into_iter().collect()) }),
        );
        let dev = eff(&grant(Developer, &["billing"]), None);
        let dev_anon = eff(&grant(Developer, &["billing", "anon"]), None);
        // R18: every role means the branch's own roles; the data API's roles only when named.
        for r in ["sales", "billing"] {
            assert_eq!(act(&admin_all, r), Ok(()), "{r}");
        }
        for r in ["anon", "authenticated"] {
            assert_eq!(act(&admin_all, r), refused(r), "{r}");
        }
        for r in ["anon", "authenticated", "sales"] {
            assert_eq!(act(&admin_narrowed, r), refused(r), "{r}");
            assert_eq!(act(&dev, r), refused(r), "{r}");
        }
        assert_eq!(act(&admin_narrowed, "billing"), Ok(()));
        assert_eq!(act(&dev_anon, "anon"), Ok(()));
        assert_eq!(act(&dev_anon, "authenticated"), refused("authenticated"));
        assert_eq!(act(&admin_all, "authenticator"), refused("authenticator"));
    }

    // ---- grant_algebra ------------------------------------------------------------------

    use proptest::prelude::*;

    /// Includes names no grant may hold (`owner`, `postgres`, `cb_m_x`, `chelabase_x`).
    const UNIVERSE: [&str; 9] = ["owner", "billing", "sales", "app", "anon", "postgres", "cb_m_x", "chelabase_x", "Owner"];

    fn valid_named(r: &PgRole) -> bool {
        matches!(r, PgRole::Named(n) if ["billing", "sales", "app", "anon"].contains(&n.as_str()))
    }

    fn arb_role() -> impl Strategy<Value = PlatformRole> {
        prop_oneof![
            Just(PlatformRole::Viewer),
            Just(PlatformRole::Developer),
            Just(PlatformRole::Admin)
        ]
    }

    fn arb_set() -> impl Strategy<Value = BTreeSet<PgRole>> {
        proptest::collection::btree_set(proptest::sample::select(&UNIVERSE[..]), 0..=UNIVERSE.len())
            .prop_map(|names| names.into_iter().map(PgRole::parse).collect())
    }

    /// Any grant row (an Only set, possibly holding invalid names), a tenant admin's grant, or
    /// a malformed `All` without Admin.
    fn arb_grant() -> impl Strategy<Value = Grant> {
        prop_oneof![
            6 => (arb_role(), arb_set()).prop_map(|(platform_role, s)| Grant { platform_role, roles: RoleSet::Only(s) }),
            1 => Just(Grant { platform_role: PlatformRole::Admin, roles: RoleSet::all() }),
            1 => prop_oneof![Just(PlatformRole::Viewer), Just(PlatformRole::Developer)]
                .prop_map(|platform_role| Grant { platform_role, roles: RoleSet::all() }),
        ]
    }

    fn arb_narrowing() -> impl Strategy<Value = Option<Narrowing>> {
        proptest::option::of(
            (proptest::option::of(arb_role()), proptest::option::of(arb_set()))
                .prop_map(|(platform_role, roles)| Narrowing { platform_role, roles }),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn grant_algebra(project in arb_grant(), db in arb_narrowing()) {
            let e = shape(&eff(&project, db.as_ref()));
            // The owner exactly with Admin.
            prop_assert_eq!(e.roles.contains(&PgRole::Owner), e.platform_role == PlatformRole::Admin);
            // `All` only with Admin.
            let every_role = matches!(e.roles, RoleSet::All { .. });
            prop_assert!(!every_role || e.platform_role == PlatformRole::Admin, "All below Admin");
            let project_roles = match &project.roles {
                RoleSet::Only(s) => s.clone(),
                // `All` without Admin is read as no roles; with Admin, it lists none.
                RoleSet::All { .. } => BTreeSet::new(),
            };

            if project.platform_role == PlatformRole::Admin {
                // R17: an Admin source is every role, narrowed by an override: the override's
                // valid roles (with the owner while Admin); below Admin without roles, the
                // valid names the source listed; still every role (with the data API names it
                // listed) when the override changes neither.
                let want_role = db.as_ref().and_then(|n| n.platform_role).map_or(PlatformRole::Admin, |r| r.min(PlatformRole::Admin));
                let listed: BTreeSet<PgRole> = project_roles.iter().filter(|r| valid_named(r)).cloned().collect();
                let want = match db.as_ref().and_then(|n| n.roles.as_ref()) {
                    Some(s) => {
                        let mut kept: BTreeSet<PgRole> = s.iter().filter(|r| valid_named(r)).cloned().collect();
                        if want_role == PlatformRole::Admin {
                            kept.insert(PgRole::Owner);
                        }
                        RoleSet::Only(kept)
                    }
                    None if want_role == PlatformRole::Admin => RoleSet::All {
                        plus: listed.iter().map(|r| r.as_str().to_string()).filter(|n| data_api_role(n)).collect(),
                    },
                    None => RoleSet::Only(listed),
                };
                prop_assert_eq!(e, Grant { platform_role: want_role, roles: want });
                return Ok(());
            }
            let RoleSet::Only(roles) = &e.roles else {
                prop_assert!(false, "a row grant became All: {:?}", e);
                unreachable!()
            };
            let levels = [db.as_ref()];
            // Never above any present level.
            prop_assert!(e.platform_role <= project.platform_role);
            for n in levels.iter().flatten() {
                if let Some(r) = n.platform_role {
                    prop_assert!(e.platform_role <= r);
                }
            }
            // Named roles ⊆ each present level's roles, and never an invalid name.
            for r in roles.iter().filter(|r| **r != PgRole::Owner) {
                prop_assert!(valid_named(r), "invalid name kept: {:?}", r);
                prop_assert!(project_roles.contains(r));
                for n in levels.iter().flatten() {
                    if let Some(s) = &n.roles {
                        prop_assert!(s.contains(r));
                    }
                }
            }
            // Equals the intersection of the valid names, plus the owner with Admin.
            let mut want_role = project.platform_role;
            let mut want_roles: BTreeSet<PgRole> = project_roles.iter().filter(|r| valid_named(r)).cloned().collect();
            for n in levels.iter().flatten() {
                if let Some(r) = n.platform_role {
                    want_role = want_role.min(r);
                }
                if let Some(s) = &n.roles {
                    want_roles = want_roles.intersection(s).cloned().collect();
                }
            }
            if want_role == PlatformRole::Admin {
                want_roles.insert(PgRole::Owner);
            }
            prop_assert_eq!(e, Grant { platform_role: want_role, roles: RoleSet::Only(want_roles) });
        }
    }
}

#[cfg(test)]
mod effective_decide_tests;
