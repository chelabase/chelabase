//! Members hub step 1, T2: `GrantSources::effective` (spec Part 5 "Effective access", D1,
//! MH-1–3) and `qualifies` (MH-17).
//!
//! The reference model below is written from the spec, not from the implementation:
//! - **Union.** No source is `None`. The platform level is the highest source level. Each role
//!   keeps its own level: the highest level among the sources naming it. An Admin source names
//!   every role except the data API's `anon`/`authenticated` (R17, R18), which only a source
//!   listing them names (MH-3: `All { plus }`).
//! - **Narrowing (D1).** The override caps the platform level and every role's level; its role
//!   list intersects. While the union holds every role (an Admin source), the override's list
//!   is kept as it is, data API names included (today's R17 rule: "its roles alone are kept",
//!   `RoleSet::intersect(All, x) == x`). Below Admin without a role list, "every role" is gone
//!   and only the roles a source named explicitly stay.
//! - **Shapes.** Admin with no narrowing below Admin and no role list is `All { plus }`;
//!   everything else is `Only`, holding the owner exactly when the level is Admin (R7).
//! - Names no grant may hold (`owner`, reserved names and prefixes) are never held.
//!
//! One modelling choice beyond the spec text: a role a source names explicitly survives a
//! narrowing below Admin even when another source is Admin, so adding a source never takes
//! access away (`adding_a_source_never_takes_access_away`).

use std::collections::{BTreeMap, BTreeSet};

use chelabase_types::access::{
    act_as_reserved, data_api_role, qualifies, tenant_admin_effective, Effective, Grant, GrantSources, Narrowing, PgRole,
    PlatformRole, RoleSet,
};
use proptest::prelude::*;

use PlatformRole::{Admin, Developer, Viewer};

const LEVELS: [PlatformRole; 3] = [Viewer, Developer, Admin];

/// The names every comparison probes: held or not, data API, never held, reserved.
const PROBES: [&str; 8] = ["a", "b", "c", "anon", "authenticated", "cb_m_x", "postgres", "owner"];

fn named(names: &[&str]) -> BTreeSet<PgRole> {
    names.iter().map(|n| PgRole::parse(n)).collect()
}

fn only(level: PlatformRole, names: &[&str]) -> Grant {
    Grant { platform_role: level, roles: RoleSet::Only(named(names)) }
}

fn all(plus: &[&str]) -> RoleSet {
    RoleSet::All { plus: plus.iter().map(|s| s.to_string()).collect() }
}

fn admin_all(plus: &[&str]) -> Grant {
    Grant { platform_role: Admin, roles: all(plus) }
}

fn narrowing(level: Option<PlatformRole>, roles: Option<&[&str]>) -> Narrowing {
    Narrowing { platform_role: level, roles: roles.map(named) }
}

fn subsets(universe: &[&'static str]) -> Vec<Vec<&'static str>> {
    (0..1u32 << universe.len())
        .map(|bits| universe.iter().enumerate().filter(|(i, _)| bits & (1 << i) != 0).map(|(_, n)| *n).collect())
        .collect()
}

/// A role a grant may hold: a named role no act-as refuses (data API names included).
fn grantable(r: &PgRole) -> Option<&str> {
    match r {
        PgRole::Owner => None,
        PgRole::Named(n) if act_as_reserved(n) => None,
        PgRole::Named(n) => Some(n),
    }
}

// ---- The reference model ------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum ModelRoles {
    /// Every role but the data API's, plus these (with their own levels in `levels`).
    All { plus: BTreeSet<String> },
    /// Exactly these named roles (the owner is added when the level is Admin).
    Only,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Model {
    platform_role: PlatformRole,
    roles: ModelRoles,
    /// `Only`: every held named role and its level. `All`: the plus names' levels.
    levels: BTreeMap<String, PlatformRole>,
}

impl Model {
    fn role_set(&self) -> RoleSet {
        match &self.roles {
            ModelRoles::All { plus } => RoleSet::All { plus: plus.clone() },
            ModelRoles::Only => {
                let mut set: BTreeSet<PgRole> = self.levels.keys().map(|n| PgRole::Named(n.clone())).collect();
                if self.platform_role == Admin {
                    set.insert(PgRole::Owner);
                }
                RoleSet::Only(set)
            }
        }
    }

    fn level_of(&self, name: &str) -> Option<PlatformRole> {
        if act_as_reserved(name) {
            return None;
        }
        match &self.roles {
            ModelRoles::All { .. } if !data_api_role(name) => Some(self.platform_role),
            _ => self.levels.get(name).copied(),
        }
    }

    fn roles_at_least(&self, min: PlatformRole) -> RoleSet {
        match &self.roles {
            ModelRoles::All { plus } => RoleSet::All {
                plus: plus.iter().filter(|n| self.levels[*n] >= min).cloned().collect(),
            },
            ModelRoles::Only => {
                let mut set: BTreeSet<PgRole> =
                    self.levels.iter().filter(|(_, l)| **l >= min).map(|(n, _)| PgRole::Named(n.clone())).collect();
                if self.platform_role == Admin {
                    set.insert(PgRole::Owner);
                }
                RoleSet::Only(set)
            }
        }
    }
}

fn model(own: Option<&Grant>, teams: &[Grant], db: Option<&Narrowing>) -> Option<Model> {
    let sources: Vec<&Grant> = own.into_iter().chain(teams).collect();
    let platform = sources.iter().map(|g| g.platform_role).max()?;
    let every = sources.iter().any(|g| g.platform_role == Admin);
    // Explicitly named roles, each at the best level among the sources naming it.
    let mut explicit: BTreeMap<String, PlatformRole> = BTreeMap::new();
    for g in &sources {
        let names: Vec<String> = match &g.roles {
            RoleSet::Only(set) => set.iter().filter_map(grantable).map(str::to_string).collect(),
            RoleSet::All { plus } => plus.iter().filter(|n| data_api_role(n)).cloned().collect(),
        };
        for n in names {
            let l = explicit.entry(n).or_insert(g.platform_role);
            *l = (*l).max(g.platform_role);
        }
    }
    let cap = db.and_then(|n| n.platform_role);
    let list: Option<BTreeSet<String>> =
        db.and_then(|n| n.roles.as_ref()).map(|s| s.iter().filter_map(grantable).map(str::to_string).collect());
    let capped = |l: PlatformRole| cap.map_or(l, |c| l.min(c));
    let level = capped(platform);

    if every && list.is_none() && level == Admin {
        let plus: BTreeSet<String> = explicit.keys().filter(|n| data_api_role(n)).cloned().collect();
        let levels = explicit.into_iter().filter(|(n, _)| data_api_role(n)).collect();
        return Some(Model { platform_role: level, roles: ModelRoles::All { plus }, levels });
    }
    let levels: BTreeMap<String, PlatformRole> = match (&list, every) {
        // Every role ∩ the override's list: the list, each at the (capped) Admin level.
        (Some(list), true) => list.iter().map(|n| (n.clone(), level)).collect(),
        (Some(list), false) => explicit
            .into_iter()
            .filter(|(n, _)| list.contains(n))
            .map(|(n, l)| (n, capped(l)))
            .collect(),
        // Below Admin, "every role" is gone: what a source named explicitly stays. An Admin
        // source names every non-data-API role at Admin.
        (None, _) => explicit
            .into_iter()
            .map(|(n, l)| {
                let best = if every && !data_api_role(&n) { Admin } else { l };
                (n, capped(best))
            })
            .collect(),
    };
    Some(Model { platform_role: level, roles: ModelRoles::Only, levels })
}

/// Compares an `Effective` with the model on every observable: the level, the role set, each
/// probe's level, `roles_at_least` at every level, `may_act_as` agreeing with `level_of`, and
/// `qualifies`.
fn check(got: Option<&Effective>, want: Option<&Model>, case: &dyn std::fmt::Debug) {
    match (got, want) {
        (None, None) => {}
        (Some(_), None) => panic!("Some where the model has no source: {case:?}"),
        (None, Some(m)) => panic!("None where the model has {m:?}: {case:?}"),
        (Some(e), Some(m)) => {
            assert_eq!(e.platform_role, m.platform_role, "level: {case:?}");
            assert_eq!(e.roles, m.role_set(), "roles: {case:?}");
            for name in PROBES {
                assert_eq!(e.level_of(name), m.level_of(name), "level_of({name}): {case:?}");
                assert_eq!(
                    e.roles.may_act_as(name),
                    m.level_of(name).is_some(),
                    "may_act_as({name}) vs level_of: {case:?}"
                );
            }
            for min in LEVELS {
                assert_eq!(e.roles_at_least(min), m.roles_at_least(min), "roles_at_least({min:?}): {case:?}");
            }
        }
    }
    for active in [false, true] {
        let want_q = active && want.is_some_and(|m| m.platform_role >= Developer);
        assert_eq!(qualifies(active, got), want_q, "qualifies(active={active}): {case:?}");
    }
}

/// Every grant shape a source can have: V/D/A × subsets of {a, b, anon} as `Only`, and Admin
/// as `All { plus }` (the metadata's Admin row, R17) with and without `anon`.
fn grants() -> Vec<Grant> {
    let mut out = Vec::new();
    for level in LEVELS {
        for s in subsets(&["a", "b", "anon"]) {
            out.push(only(level, &s));
        }
    }
    out.push(admin_all(&[]));
    out.push(admin_all(&["anon"]));
    out
}

fn overrides() -> Vec<Option<Narrowing>> {
    let mut out = vec![None];
    for level in [None, Some(Viewer), Some(Developer), Some(Admin)] {
        out.push(Some(narrowing(level, None)));
        for s in subsets(&["a", "c", "anon"]) {
            out.push(Some(narrowing(level, Some(&s))));
        }
    }
    out
}

#[test]
fn effective_matches_a_model_exhaustively() {
    let grants = grants();
    let owns: Vec<Option<Grant>> = std::iter::once(None).chain(grants.iter().cloned().map(Some)).collect();
    let mut team_lists: Vec<Vec<Grant>> = vec![vec![]];
    for g in &grants {
        team_lists.push(vec![g.clone()]);
    }
    for g in &grants {
        for h in &grants {
            team_lists.push(vec![g.clone(), h.clone()]);
        }
    }
    let overrides = overrides();
    let mut cases = 0usize;
    for own in &owns {
        for teams in &team_lists {
            for db in &overrides {
                let e = GrantSources::new(own.clone(), teams.clone(), db.clone()).effective();
                let m = model(own.as_ref(), teams, db.as_ref());
                check(e.as_ref(), m.as_ref(), &(own, teams, db));
                cases += 1;
            }
        }
    }
    assert_eq!(cases, owns.len() * team_lists.len() * overrides.len());
    assert_eq!(cases, 27 * (1 + 26 + 26 * 26) * 37);
}

/// S20: in step 1 the metadata gives no team grants and an Admin row is `All { plus: {} }`, so
/// `plus` is always empty (an override intersects to `Only`) and `/me` keeps `"all"`.
#[test]
fn step_one_shapes_never_carry_plus() {
    let mut owns: Vec<Grant> = vec![admin_all(&[])];
    for level in [Viewer, Developer] {
        for s in subsets(&["a", "b", "anon"]) {
            owns.push(only(level, &s));
        }
    }
    for own in owns {
        for db in overrides() {
            let Some(e) = GrantSources::new(Some(own.clone()), Vec::new(), db.clone()).effective() else {
                panic!("an own grant gives access: {own:?} {db:?}");
            };
            if let RoleSet::All { plus } = &e.roles {
                assert!(plus.is_empty(), "{own:?} {db:?}: {plus:?}");
            }
        }
    }
}

#[test]
fn no_source_is_none() {
    for db in overrides() {
        assert!(GrantSources::new(None, Vec::new(), db.clone()).effective().is_none(), "{db:?}");
    }
    // An override alone (a member whose project grant was deleted) is no access, never a
    // default level.
    let admin_override = narrowing(Some(Admin), Some(&["a", "anon"]));
    assert!(GrantSources::new(None, vec![], Some(admin_override)).effective().is_none());
}

#[test]
fn admin_from_any_source_is_all_with_named_data_api_roles() {
    // MH-3: own Developer {anon}, a team Admin: every role, and anon kept.
    let e = GrantSources::new(Some(only(Developer, &["anon"])), vec![admin_all(&[])], None).effective().unwrap();
    assert_eq!(e.platform_role, Admin);
    assert_eq!(e.roles, all(&["anon"]));
    assert!(e.roles.may_act_as("anon"));
    assert!(!e.roles.may_act_as("authenticated"), "only a source naming it");
    assert!(e.roles.may_act_as("billing"));
    assert!(!e.roles.may_act_as("authenticator"));
    assert!(!e.roles.may_act_as("cb_m_x"));
    // anon keeps the level of the source that named it (MH-1); every other role is Admin.
    assert_eq!(e.level_of("anon"), Some(Developer));
    assert_eq!(e.level_of("billing"), Some(Admin));
    assert_eq!(e.level_of("authenticated"), None);
    // Personal logins (D1): roles held at Developer or above.
    assert_eq!(e.roles_at_least(Developer), all(&["anon"]));
    assert_eq!(e.roles_at_least(Admin), all(&[]));

    // The Admin source alone: every role, no data API role.
    let e = GrantSources::new(Some(admin_all(&[])), vec![], None).effective().unwrap();
    assert_eq!(e.roles, all(&[]));
    assert!(!e.roles.may_act_as("anon"));
    assert_eq!(e.level_of("anon"), None);

    // Admin from a team, `authenticated` named by the own Viewer grant: kept, at Viewer.
    let e = GrantSources::new(Some(only(Viewer, &["authenticated"])), vec![admin_all(&[])], None).effective().unwrap();
    assert_eq!(e.roles, all(&["authenticated"]));
    assert_eq!(e.level_of("authenticated"), Some(Viewer));
    assert_eq!(e.roles_at_least(Developer), all(&[]));

    // Two Admin sources naming different data API roles: both kept, at Admin.
    let e = GrantSources::new(Some(admin_all(&["anon"])), vec![admin_all(&["authenticated"])], None).effective().unwrap();
    assert_eq!(e.roles, all(&["anon", "authenticated"]));
    assert_eq!(e.level_of("authenticated"), Some(Admin));

    // An override's role list intersects to `Only` (with the owner while Admin).
    let db = narrowing(None, Some(&["billing", "anon"]));
    let e = GrantSources::new(Some(only(Developer, &["anon"])), vec![admin_all(&[])], Some(db)).effective().unwrap();
    assert_eq!(e.roles, only(Admin, &["owner", "billing", "anon"]).roles);
    // An override below Admin without roles: every role is gone, the named anon stays.
    let db = narrowing(Some(Developer), None);
    let e = GrantSources::new(Some(only(Developer, &["anon"])), vec![admin_all(&[])], Some(db)).effective().unwrap();
    assert_eq!((e.platform_role, e.roles.clone()), (Developer, only(Developer, &["anon"]).roles));
}

#[test]
fn qualifies_needs_active_and_developer() {
    let at = |level: PlatformRole| GrantSources::new(Some(only(level, &["a"])), vec![], None).effective().unwrap();
    let cases: Vec<(&str, Option<Effective>, bool)> = vec![
        ("no access", None, false),
        ("viewer", Some(at(Viewer)), false),
        ("developer", Some(at(Developer)), true),
        ("admin", Some(at(Admin)), true),
        ("tenant admin", Some(tenant_admin_effective()), true),
        // Developer through a team, Viewer own (step 2's shape).
        (
            "viewer own, developer team",
            GrantSources::new(Some(only(Viewer, &["a"])), vec![only(Developer, &[])], None).effective(),
            true,
        ),
        // An override down to Viewer takes it away.
        (
            "admin narrowed to viewer",
            GrantSources::new(Some(admin_all(&[])), vec![], Some(narrowing(Some(Viewer), None))).effective(),
            false,
        ),
    ];
    for (name, e, level_ok) in cases {
        for active in [false, true] {
            assert_eq!(qualifies(active, e.as_ref()), active && level_ok, "{name}, active={active}");
        }
    }
}

#[test]
fn the_tenant_admin_effective_is_admin_with_every_role() {
    let e = tenant_admin_effective();
    assert_eq!(e.platform_role, Admin);
    assert_eq!(e.roles, all(&[]));
    assert_eq!(e.level_of("billing"), Some(Admin));
    assert_eq!(e.level_of("anon"), None, "R18: never the data API roles");
    assert_eq!(e.level_of("postgres"), None);
    assert_eq!(e.roles_at_least(Developer), all(&[]));
    // The same as a project Admin's grant without an override.
    let project_admin = GrantSources::new(Some(admin_all(&[])), vec![], None).effective().unwrap();
    assert_eq!((project_admin.platform_role, project_admin.roles), (e.platform_role, e.roles));
}

// ---- Properties over a wider universe -----------------------------------------------------

/// Includes names no grant may hold (`owner` as `PgRole::Owner`, `Owner`, `postgres`, `cb_m_x`,
/// `chelabase_x`, `authenticator`).
const UNIVERSE: [&str; 11] =
    ["a", "b", "c", "anon", "authenticated", "owner", "Owner", "postgres", "cb_m_x", "chelabase_x", "authenticator"];

fn arb_level() -> impl Strategy<Value = PlatformRole> {
    prop_oneof![Just(Viewer), Just(Developer), Just(Admin)]
}

fn arb_names() -> impl Strategy<Value = BTreeSet<PgRole>> {
    proptest::collection::btree_set(proptest::sample::select(&UNIVERSE[..]), 0..=5)
        .prop_map(|names| names.into_iter().map(PgRole::parse).collect())
}

fn arb_grant() -> impl Strategy<Value = Grant> {
    prop_oneof![
        6 => (arb_level(), arb_names()).prop_map(|(platform_role, s)| Grant { platform_role, roles: RoleSet::Only(s) }),
        2 => proptest::collection::btree_set(prop_oneof![Just("anon".to_string()), Just("authenticated".to_string())], 0..=2)
            .prop_map(|plus| Grant { platform_role: Admin, roles: RoleSet::All { plus } }),
    ]
}

fn arb_narrowing() -> impl Strategy<Value = Option<Narrowing>> {
    proptest::option::of(
        (proptest::option::of(arb_level()), proptest::option::of(arb_names()))
            .prop_map(|(platform_role, roles)| Narrowing { platform_role, roles }),
    )
}

fn arb_sources() -> impl Strategy<Value = (Option<Grant>, Vec<Grant>, Option<Narrowing>)> {
    (proptest::option::of(arb_grant()), proptest::collection::vec(arb_grant(), 0..=4), arb_narrowing())
}

/// What an `Effective` shows, for comparing two of them.
#[allow(clippy::type_complexity)]
fn view(e: &Option<Effective>) -> Option<(PlatformRole, RoleSet, Vec<Option<PlatformRole>>, Vec<RoleSet>)> {
    e.as_ref().map(|e| {
        (
            e.platform_role,
            e.roles.clone(),
            UNIVERSE.iter().map(|n| e.level_of(n)).collect(),
            LEVELS.iter().map(|l| e.roles_at_least(*l)).collect(),
        )
    })
}

/// Whether `g` names `name` (D1): an Admin source names every role except a data API role it
/// doesn't list; any other source names what it lists. Reserved names are never named.
fn names(g: &Grant, name: &str) -> bool {
    if act_as_reserved(name) {
        return false;
    }
    let listed = match &g.roles {
        RoleSet::Only(s) => s.contains(&PgRole::Named(name.to_string())),
        RoleSet::All { plus } => plus.contains(name),
    };
    listed || (g.platform_role == Admin && !data_api_role(name))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    #[test]
    fn union_is_commutative_and_idempotent_over_teams(sources in arb_sources(), rotate in 0usize..5) {
        let (own, teams, db) = sources;
        let base = view(&GrantSources::new(own.clone(), teams.clone(), db.clone()).effective());
        // Any order of the team grants.
        let mut rotated = teams.clone();
        if !rotated.is_empty() {
            let k = rotate % rotated.len();
            rotated.rotate_left(k);
        }
        prop_assert_eq!(&view(&GrantSources::new(own.clone(), rotated, db.clone()).effective()), &base);
        let mut reversed = teams.clone();
        reversed.reverse();
        prop_assert_eq!(&view(&GrantSources::new(own.clone(), reversed, db.clone()).effective()), &base);
        // A team listed twice changes nothing.
        let doubled: Vec<Grant> = teams.iter().chain(teams.iter()).cloned().collect();
        prop_assert_eq!(&view(&GrantSources::new(own.clone(), doubled, db.clone()).effective()), &base);
        // The own grant is one source like the others.
        let mut as_team = teams.clone();
        as_team.extend(own.clone());
        prop_assert_eq!(&view(&GrantSources::new(None, as_team, db.clone()).effective()), &base);
        // And it matches the model.
        let e = GrantSources::new(own.clone(), teams.clone(), db.clone()).effective();
        check(e.as_ref(), model(own.as_ref(), &teams, db.as_ref()).as_ref(), &(&own, &teams, &db));
    }

    #[test]
    fn a_roles_level_never_exceeds_its_best_source_or_the_cap(sources in arb_sources()) {
        let (own, teams, db) = sources;
        let Some(e) = GrantSources::new(own.clone(), teams.clone(), db.clone()).effective() else {
            prop_assert!(own.is_none() && teams.is_empty(), "None with a source");
            return Ok(());
        };
        let sources: Vec<&Grant> = own.iter().chain(teams.iter()).collect();
        let best_level = sources.iter().map(|g| g.platform_role).max().unwrap();
        let cap = db.as_ref().and_then(|n| n.platform_role);
        let listed = db.as_ref().and_then(|n| n.roles.as_ref());
        prop_assert!(e.platform_role <= best_level);
        prop_assert!(cap.is_none_or(|c| e.platform_role <= c));
        for name in UNIVERSE {
            let Some(level) = e.level_of(name) else { continue };
            prop_assert!(!act_as_reserved(name), "a reserved name is held: {}", name);
            prop_assert!(e.roles.may_act_as(name), "{} has a level but can't be acted as", name);
            prop_assert!(level <= e.platform_role, "{} above the platform level", name);
            prop_assert!(cap.is_none_or(|c| level <= c), "{} above the override's cap", name);
            if let Some(list) = listed {
                prop_assert!(list.contains(&PgRole::Named(name.to_string())), "{} not in the override's list", name);
            }
            // The best source naming it (an override's list names a data API role for an Admin
            // source, R17/R18 as today).
            let best = sources
                .iter()
                .filter(|g| {
                    names(g, name)
                        || (g.platform_role == Admin
                            && listed.is_some_and(|l| l.contains(&PgRole::Named(name.to_string()))))
                })
                .map(|g| g.platform_role)
                .max();
            prop_assert!(best.is_some_and(|b| level <= b), "{} at {:?}, best source {:?}", name, level, best);
        }
        // The owner exactly with Admin; `All` only with Admin.
        prop_assert_eq!(e.roles.contains(&PgRole::Owner), e.platform_role == Admin);
        let every_role = matches!(e.roles, RoleSet::All { .. });
        prop_assert!(!every_role || e.platform_role == Admin, "All below Admin");
    }

    #[test]
    fn adding_a_source_never_takes_access_away(sources in arb_sources(), extra in arb_grant()) {
        let (own, teams, db) = sources;
        let before = GrantSources::new(own.clone(), teams.clone(), db.clone()).effective();
        let mut more = teams.clone();
        more.push(extra);
        let after = GrantSources::new(own, more, db).effective().expect("a source");
        if let Some(before) = before {
            prop_assert!(after.platform_role >= before.platform_role);
            for name in UNIVERSE {
                if let Some(l) = before.level_of(name) {
                    prop_assert!(after.level_of(name).is_some_and(|a| a >= l), "{} lost or lowered", name);
                }
            }
        }
    }
}
