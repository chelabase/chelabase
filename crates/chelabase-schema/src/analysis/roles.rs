//! Role changes the merge can't make (E30), reported "apply by hand" with their SQL.
//!
//! The roles path (U2) makes a role's name (E29: a rename), LOGIN, INHERIT, CONNECTION LIMIT,
//! VALID UNTIL, BYPASSRLS, owner-level and its memberships by role. Anything else the source
//! changed on a role is listed here and never merged; the merged model keeps the target's values
//! for it ([`super::merge`]) and the drift compare ignores it ([`crate::compare`]):
//! - other attributes (`SUPERUSER`, `CREATEDB`, `CREATEROLE`, `REPLICATION`) and the comment;
//! - memberships the roles path refuses (`pg_*`, reserved, `chelabase_…` and `cb_m_…` roles, the owner);
//! - the options of a membership both sides have;
//! - every attribute of the owner ([`OWNER_ROLE`]); its memberships are never a change (E22).

use super::index::{Entry, Unmerged};
use super::{ObjSnapshot, ReportedChange, ReportedChangeKind, World};
use crate::model::{Membership, OWNER_LEVEL_ROLE, Reported, ReportedKind, Role, membership_merges};
use crate::plan::OWNER_ROLE;
use crate::sql::{ident, literal};

fn role_of(e: Option<&Entry>) -> Option<(&Role, &Unmerged)> {
    let e = e?;
    match (&e.snap, &e.unmerged) {
        (ObjSnapshot::Role(r), Some(u)) => Some((r, u)),
        _ => None,
    }
}

/// The owner's U2 attributes: LOGIN, INHERIT, CONNECTION LIMIT, VALID UNTIL, BYPASSRLS.
type OwnerAttrs = (bool, bool, i32, Option<String>, bool);

/// The options the roles path gives a membership it makes (`GRANT r TO x`): no ADMIN, INHERIT
/// following the member's INHERIT attribute, SET.
fn default_options(member: &Role) -> String {
    format!("{:?}", (false, member.inherit, true))
}

/// Every source role whose unmerged parts changed since the base, or that joins a role with
/// options other than the roles path's (fix round I2), unless the target already has the same:
/// reported with the SQL that makes the source's changes on the target. By name.
pub(crate) fn reports(w: &World) -> Vec<ReportedChange> {
    let mut out = Vec::new();
    for (id, e) in &w.si.entries {
        let Some((s, su)) = role_of(Some(e)) else {
            continue;
        };
        let base = role_of(w.bi.get(id));
        let target = role_of(w.ti.get(id));
        let default = default_options(s);
        // A membership new on the source whose options aren't the defaults (and that the
        // target doesn't already have so).
        let new_options = su.options.iter().any(|(role, rows)| {
            base.is_none_or(|(_, bu)| !bu.options.contains_key(role))
                && rows.iter().any(|o| *o != default)
                && target.is_none_or(|(_, tu)| tu.options.get(role) != Some(rows))
        });
        let changed = match base {
            None => {
                !su.other.is_empty()
                    || su.comment.is_some()
                    || !su.refused.is_empty()
                    || new_options
            }
            Some((_, bu)) => bu.differs(su) || new_options,
        };
        if !changed {
            continue;
        }
        let sql = by_hand(base.map(|(b, _)| b), target.map(|(t, _)| t), s);
        if sql.is_empty() {
            continue;
        }
        out.push(ReportedChange {
            object: Reported {
                kind: ReportedKind::Role,
                oid: s.oid,
                schema: None,
                name: s.name.clone(),
                parent: None,
                arguments: None,
                definition: Some(sql.join(";\n") + ";"),
            },
            change: if base.is_none() {
                ReportedChangeKind::Created
            } else {
                ReportedChangeKind::Altered
            },
            needed_by: Vec::new(),
        });
    }
    out.sort_by(|a, b| a.object.name.cmp(&b.object.name));
    out
}

/// The statements that make on the target what the source changed since the base (`b`; `None`:
/// a role the source created) in the parts the merge doesn't make, skipping what the target (`t`)
/// already has. Never a statement for a part only the target changed: those stay (fix round I2).
fn by_hand(b: Option<&Role>, t: Option<&Role>, s: &Role) -> Vec<String> {
    let name = ident(&s.name);
    let mut out = Vec::new();
    let mut attrs: Vec<String> = Vec::new();
    if s.name == OWNER_ROLE {
        let values = |r: Option<&Role>| {
            r.map(|r| {
                (
                    r.login,
                    r.inherit,
                    r.connection_limit,
                    r.valid_until.clone(),
                    r.bypass_rls,
                )
            })
        };
        let (bv, tv) = (values(b), values(t));
        let changed = |f: &dyn Fn(&OwnerAttrs) -> String| {
            let sv = f(&values(Some(s)).expect("the source's"));
            bv.as_ref().is_none_or(|v| f(v) != sv) && tv.as_ref().is_none_or(|v| f(v) != sv)
        };
        if changed(&|v| v.0.to_string()) {
            attrs.push(if s.login { "LOGIN" } else { "NOLOGIN" }.to_string());
        }
        if changed(&|v| v.1.to_string()) {
            attrs.push(if s.inherit { "INHERIT" } else { "NOINHERIT" }.to_string());
        }
        if changed(&|v| v.2.to_string()) {
            attrs.push(format!("CONNECTION LIMIT {}", s.connection_limit));
        }
        if changed(&|v| format!("{:?}", v.3)) {
            attrs.push(format!(
                "VALID UNTIL {}",
                literal(s.valid_until.as_deref().unwrap_or("infinity"))
            ));
        }
        if changed(&|v| v.4.to_string()) {
            attrs.push(if s.bypass_rls { "BYPASSRLS" } else { "NOBYPASSRLS" }.to_string());
        }
    }
    let none: &[String] = &[];
    let b_other = b.map_or(none, |b| &b.other_attributes);
    let t_other = t.map_or(none, |t| &t.other_attributes);
    for a in &s.other_attributes {
        if !b_other.contains(a) && !t_other.contains(a) {
            attrs.push(a.clone());
        }
    }
    for a in b_other {
        if !s.other_attributes.contains(a) && t_other.contains(a) {
            attrs.push(format!("NO{a}"));
        }
    }
    if !attrs.is_empty() {
        out.push(format!("ALTER ROLE {name} {}", attrs.join(" ")));
    }
    let b_comment = b.and_then(|b| b.comment.as_ref());
    let t_comment = t.and_then(|t| t.comment.as_ref());
    if b_comment != s.comment.as_ref() && t_comment != s.comment.as_ref() {
        out.push(format!(
            "COMMENT ON ROLE {name} IS {}",
            s.comment.as_deref().map_or("NULL".to_string(), literal)
        ));
    }
    if s.name == OWNER_ROLE {
        return out;
    }
    let no_rows: &[Membership] = &[];
    let b_rows = b.map_or(no_rows, |b| &b.member_of);
    let t_rows = t.map_or(no_rows, |t| &t.member_of);
    let first = |list: &[Membership], role: &str| -> Option<(bool, bool, bool)> {
        list.iter()
            .find(|m| m.role.name == role)
            .map(|m| (m.admin, m.inherit, m.set))
    };
    let default = (false, s.inherit, true);
    let with = |o: (bool, bool, bool)| format!("WITH ADMIN {}, INHERIT {}, SET {}", o.0, o.1, o.2);
    let mut done: Vec<&str> = Vec::new();
    for m in &s.member_of {
        let role = m.role.name.as_str();
        if role == OWNER_LEVEL_ROLE || done.contains(&role) {
            continue;
        }
        done.push(role);
        let o = first(&s.member_of, role).expect("its own row");
        let (in_b, in_t) = (first(b_rows, role), first(t_rows, role));
        let wanted = if !membership_merges(role) {
            // A refused membership the source added.
            in_b.is_none() && in_t.is_none()
        } else {
            match in_b {
                // Options the source changed on a membership the base had.
                Some(bo) => bo != o && in_t.is_some_and(|to| to != o),
                // A new membership with options the roles path doesn't give.
                None => o != default && in_t != Some(o),
            }
        };
        if wanted {
            out.push(format!("GRANT {} TO {name} {}", ident(role), with(o)));
        }
    }
    for m in b_rows {
        let role = m.role.name.as_str();
        if !membership_merges(role)
            && first(&s.member_of, role).is_none()
            && first(t_rows, role).is_some()
            && !done.contains(&role)
        {
            done.push(role);
            out.push(format!("REVOKE {} FROM {name}", ident(role)));
        }
    }
    out
}
