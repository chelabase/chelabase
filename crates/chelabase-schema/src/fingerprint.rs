//! A model's fingerprint: SHA-256 (hex) of its canonical JSON (the normalized model, serialized
//! with serde's fixed field order; the model holds no maps).

use sha2::{Digest, Sha256};

use crate::model::SchemaModel;
use crate::plan::{OWNER_ROLE, Phase, RoleOp, Step};

/// The fingerprint of `m`, independent of the order its unordered lists came in.
pub fn fingerprint(m: &SchemaModel) -> String {
    let mut canonical = m.clone();
    canonical.normalize();
    // Serializing plain structs, vectors and strings can't fail.
    let json = serde_json::to_vec(&canonical).expect("a model serializes");
    hex::encode(Sha256::digest(&json))
}

/// The fingerprint the apply's pre-check compares (I1): `m`'s, without exactly what the plan's
/// phases 0 and 1a may already have done on an earlier attempt (a retry after a failure in
/// `roles`, `enum_add` or `tx`, or a re-apply after an interrupted one), so those don't read as
/// a change of the target, while every other change does (T14 minor 9):
/// - a role a `RoleOp::Create` makes, and the owner's memberships in it (E22: Postgres's
///   automatic grants to the creator);
/// - the fields a `RoleOp::Update` patches, on that role only (not the whole role: its other
///   attributes, and the owner's memberships in it, still count);
/// - a `RoleOp::Rename` (E29): the role (by its OID) and every reference to it are read under
///   the new name;
/// - the values each phase 1a step adds, on the enum it names (the enum's other values, its
///   name and owner still count).
///
/// A phase 1a statement that doesn't parse can't say what it added: the result then never
/// matches (it reads as stale), rather than hiding anything.
///
/// Compute stores it for the target's model and the plan; the apply takes it again of the live
/// target.
pub fn precheck_fingerprint(m: &SchemaModel, steps: &[Step]) -> String {
    let ops: Vec<&RoleOp> = steps.iter().filter_map(|s| s.role_op.as_ref()).collect();
    // Renames first (the other ops name the role by its new name), all at once and by the
    // target's OID, so a chain (b → c, then a → b) reads the same before, halfway and after.
    let renamed: std::collections::HashMap<crate::model::Oid, &str> = ops
        .iter()
        .filter_map(|op| match op {
            RoleOp::Rename { oid, to, .. } => Some((*oid, to.as_str())),
            _ => None,
        })
        .collect();
    let mut projected = if renamed.is_empty() {
        m.clone()
    } else {
        m.rename_roles(|oid, _| renamed.get(&oid).map(|to| to.to_string()))
    };
    let created: Vec<&str> = ops
        .iter()
        .filter_map(|op| match op {
            RoleOp::Create(spec) => Some(spec.name.as_str()),
            _ => None,
        })
        .collect();
    projected.roles.retain(|r| !created.contains(&r.name.as_str()));
    // E22: creating those roles also gave the owner its automatic memberships in them.
    for owner in projected.roles.iter_mut().filter(|r| r.name == OWNER_ROLE) {
        owner
            .member_of
            .retain(|ms| !created.contains(&ms.role.name.as_str()));
    }
    for op in &ops {
        let RoleOp::Update { name, patch } = op else {
            continue;
        };
        for r in projected.roles.iter_mut().filter(|r| &r.name == name) {
            project_patch(r, patch);
        }
    }
    for step in steps.iter().filter(|s| s.phase == Phase::EnumAdd) {
        let Some(added) = values_added(&step.sql) else {
            return format!("unparseable-phase-1a-{}", uuid::Uuid::new_v4());
        };
        for (schema, name, label) in added {
            for e in projected.enums.iter_mut().filter(|e| {
                e.name == name && schema.as_ref().is_none_or(|s| *s == e.schema)
            }) {
                e.labels.retain(|l| l.label != label);
            }
        }
    }
    fingerprint(&projected)
}

/// `r` with the fields `patch` sets at fixed values (whatever they were).
fn project_patch(r: &mut crate::model::Role, patch: &crate::plan::RolePatchModel) {
    use crate::model::{OWNER_LEVEL_ROLE, membership_merges};
    if patch.login.is_some() {
        r.login = false;
    }
    if patch.inherit.is_some() {
        r.inherit = false;
    }
    if patch.connection_limit.is_some() {
        r.connection_limit = 0;
    }
    if patch.valid_until.is_some() {
        r.valid_until = None;
    }
    if patch.bypass_rls.is_some() {
        r.bypass_rls = false;
    }
    if patch.owner_level.is_some() {
        r.member_of.retain(|m| m.role.name != OWNER_LEVEL_ROLE);
    }
    if patch.member_of.is_some() {
        r.member_of
            .retain(|m| m.role.name == OWNER_LEVEL_ROLE || !membership_merges(&m.role.name));
    }
}

/// The values `sql`'s `ALTER TYPE … ADD VALUE` statements add, as `(schema, enum, value)` (no
/// schema when unqualified); `None` when it doesn't parse.
fn values_added(sql: &str) -> Option<Vec<(Option<String>, String, String)>> {
    use pg_query::NodeEnum as N;
    let parsed = pg_query::parse(sql).ok()?;
    let mut out = Vec::new();
    for raw in &parsed.protobuf.stmts {
        let Some(N::AlterEnumStmt(s)) = raw.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
            continue;
        };
        if s.new_val.is_empty() || !s.old_val.is_empty() {
            continue;
        }
        let parts: Vec<String> = s
            .type_name
            .iter()
            .filter_map(|n| match &n.node {
                Some(N::String(v)) => Some(v.sval.clone()),
                _ => None,
            })
            .collect();
        match parts.as_slice() {
            [name] => out.push((None, name.clone(), s.new_val.clone())),
            [.., schema, name] => out.push((Some(schema.clone()), name.clone(), s.new_val.clone())),
            [] => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::model::Oid;
    use crate::model::sample::full_model;
    use crate::plan::{RolePatchModel, RoleSpecModel, StepId, StepKind};

    /// Every leaf of `v`, as a JSON pointer.
    fn leaves(v: &Value, path: String, out: &mut Vec<String>) {
        match v {
            Value::Object(map) => {
                for (k, child) in map {
                    leaves(child, format!("{path}/{k}"), out);
                }
            }
            Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    leaves(child, format!("{path}/{i}"), out);
                }
            }
            // Every optional field is set on at least one object of the sample.
            Value::Null => {}
            _ => out.push(path),
        }
    }

    fn mutate(v: &mut Value) {
        *v = match v {
            Value::Bool(b) => Value::Bool(!*b),
            Value::Number(n) => match n.as_i64() {
                Some(i) if i == i64::MAX => Value::from(i - 1),
                Some(i) => Value::from(i + 1),
                None => Value::from(n.as_u64().unwrap() - 1),
            },
            Value::String(s) => Value::String(format!("{s}x")),
            _ => unreachable!("leaves are scalars"),
        };
    }

    #[test]
    fn fingerprint_changes_with_any_field() {
        let model = full_model();
        let base = fingerprint(&model);
        assert_eq!(base.len(), 64);
        assert_eq!(fingerprint(&model), base, "stable");

        let json = serde_json::to_value(&model).unwrap();
        let mut paths = Vec::new();
        leaves(&json, String::new(), &mut paths);
        let mut changed = 0;
        for path in &paths {
            let mut v = json.clone();
            mutate(v.pointer_mut(path).unwrap());
            // An enum's tag with a suffix isn't a model any more; every other leaf is.
            let Ok(mutated) = serde_json::from_value::<SchemaModel>(v) else {
                continue;
            };
            assert_ne!(
                fingerprint(&mutated),
                base,
                "{path} doesn't change the fingerprint"
            );
            changed += 1;
        }
        assert!(
            changed > 150,
            "only {changed} of {} leaves were mutated",
            paths.len()
        );
    }

    fn step(id: &str, phase: Phase, sql: &str, role_op: Option<RoleOp>) -> Step {
        Step {
            id: StepId(id.to_string()),
            kind: StepKind::Generated,
            phase,
            sql: sql.to_string(),
            items: Vec::new(),
            data_loss: false,
            rewrite: false,
            security: false,
            locks: Vec::new(),
            role_op,
            online: None,
            breaking: None,
        }
    }

    /// A plan touching role `roles[0]` (an update) and `ghost` (a create) in phase 0, and adding
    /// a value to `"Billing".status` in phase 1a.
    fn plan_steps(model: &SchemaModel) -> Vec<Step> {
        let touched = model.roles[0].name.clone();
        vec![
            step(
                "r1",
                Phase::Roles,
                "-- role",
                Some(RoleOp::Update {
                    name: touched,
                    patch: RolePatchModel {
                        connection_limit: Some(7),
                        ..RolePatchModel::default()
                    },
                }),
            ),
            step(
                "r2",
                Phase::Roles,
                "-- role",
                Some(RoleOp::Create(RoleSpecModel {
                    name: "ghost".into(),
                    owner_level: false,
                    login: true,
                    inherit: true,
                    connection_limit: -1,
                    valid_until: None,
                    bypass_rls: false,
                    member_of: Vec::new(),
                })),
            ),
            step(
                "e1",
                Phase::EnumAdd,
                "SET lock_timeout = '5s'; ALTER TYPE \"Billing\".status ADD VALUE IF NOT EXISTS 'zz'",
                None,
            ),
            step("t1", Phase::Tx, "CREATE TABLE public.x (id int)", None),
        ]
    }

    // I1 (fix round 2): what the plan's phases 0 and 1a may have done on an earlier attempt
    // doesn't count as a change; anything else does.
    #[test]
    fn precheck_fingerprint_ignores_the_plans_own_roles_and_enum_values() {
        let before = full_model();
        assert_eq!(before.enums[0].schema, "Billing");
        let steps = plan_steps(&before);
        let base = precheck_fingerprint(&before, &steps);

        // Phase 0 created `ghost` and changed roles[0]; phase 1a added a value.
        let mut after = before.clone();
        let mut ghost = after.roles[0].clone();
        ghost.name = "ghost".into();
        ghost.oid = Oid(99_999);
        after.roles.push(ghost);
        after.roles[0].connection_limit = 7;
        let mut label = after.enums[0].labels[0].clone();
        label.label = "zz".into();
        label.oid = Oid(99_998);
        after.enums[0].labels.push(label.clone());
        assert_eq!(precheck_fingerprint(&after, &steps), base);
        assert_ne!(fingerprint(&after), fingerprint(&before), "the full one sees it");

        // An unrelated role, an unrelated enum and a column are changes.
        let mut other_role = before.clone();
        let mut intruder = other_role.roles[0].clone();
        intruder.name = "intruder".into();
        other_role.roles.push(intruder);
        assert_ne!(precheck_fingerprint(&other_role, &steps), base);
        if before.enums.len() > 1 {
            let mut other_enum = before.clone();
            other_enum.enums[1].labels.push(label.clone());
            assert_ne!(precheck_fingerprint(&other_enum, &steps), base);
        }
        let mut renamed_enum = before.clone();
        renamed_enum.enums[0].schema = "public".into();
        renamed_enum.enums[0].labels.push(label);
        assert_ne!(
            precheck_fingerprint(&renamed_enum, &steps),
            base,
            "a same-named enum in another schema isn't the plan's"
        );
        let mut column = before.clone();
        column.tables[0].columns[0].comment = Some("changed".into());
        assert_ne!(precheck_fingerprint(&column, &steps), base);
        // A plan without phases 0 and 1a projects nothing out.
        assert_eq!(precheck_fingerprint(&before, &[]), fingerprint(&before));
    }

    // E20/E22: phase 0 of an earlier attempt also gave the owner its automatic memberships in
    // the roles it created; those aren't a change either, but an owner grant in another role is.
    #[test]
    fn precheck_fingerprint_ignores_the_owners_memberships_in_the_plans_new_roles() {
        let mut before = full_model();
        let mut owner = before.roles[0].clone();
        owner.name = crate::plan::OWNER_ROLE.into();
        owner.oid = Oid(10);
        owner.member_of.clear();
        before.roles.push(owner);
        let steps = plan_steps(&before);
        let base = precheck_fingerprint(&before, &steps);

        fn owner_of(m: &mut SchemaModel) -> &mut crate::model::Role {
            m.roles
                .iter_mut()
                .find(|r| r.name == crate::plan::OWNER_ROLE)
                .unwrap()
        }
        let ghost = crate::model::NamedRole {
            oid: Oid(99_999),
            name: "ghost".into(),
        };
        let owner_name = crate::model::NamedRole {
            oid: Oid(10),
            name: crate::plan::OWNER_ROLE.into(),
        };
        let mut after = before.clone();
        owner_of(&mut after)
            .member_of
            .extend(crate::model::owner_grants(&ghost, &owner_name));
        assert_eq!(precheck_fingerprint(&after, &steps), base);

        let mut other = before.clone();
        let intruder = crate::model::NamedRole {
            oid: Oid(99_000),
            name: "intruder".into(),
        };
        owner_of(&mut other)
            .member_of
            .extend(crate::model::owner_grants(&intruder, &owner_name));
        assert_ne!(precheck_fingerprint(&other, &steps), base);
    }

    /// T14 minor 9 (and fix round M3), as an invariant: for each field a `RoleOp::Update` can
    /// patch, the projection hides exactly that field of that role (for owner-level and
    /// memberships, the rows they own), and every other change of the role and of the enum the
    /// plan adds to is still seen. Checked over every scalar leaf of the role and the enum;
    /// only the role's own name and the values' texts, which identify them, are skipped.
    #[test]
    fn precheck_hides_only_the_plans_own_changes() {
        use crate::model::{OWNER_LEVEL_ROLE, membership_merges};
        let before = full_model();
        let touched = before.roles[0].name.clone();
        let rows: Vec<String> = before.roles[0]
            .member_of
            .iter()
            .map(|m| m.role.name.clone())
            .collect();
        let json = serde_json::to_value(&before).unwrap();
        let mut leaves_of = Vec::new();
        leaves(&json, String::new(), &mut leaves_of);
        let fields: [(&str, RolePatchModel); 7] = [
            ("login", RolePatchModel { login: Some(true), ..Default::default() }),
            ("inherit", RolePatchModel { inherit: Some(true), ..Default::default() }),
            (
                "connection_limit",
                RolePatchModel { connection_limit: Some(7), ..Default::default() },
            ),
            ("valid_until", RolePatchModel { valid_until: Some(None), ..Default::default() }),
            ("bypass_rls", RolePatchModel { bypass_rls: Some(true), ..Default::default() }),
            ("owner_level", RolePatchModel { owner_level: Some(false), ..Default::default() }),
            ("member_of", RolePatchModel { member_of: Some(Vec::new()), ..Default::default() }),
        ];
        for (field, patch) in fields {
            let mut steps = plan_steps(&before);
            steps[0].role_op = Some(RoleOp::Update {
                name: touched.clone(),
                patch,
            });
            let base = precheck_fingerprint(&before, &steps);
            let mut seen = 0;
            for path in &leaves_of {
                if !path.starts_with("/roles/0/") && !path.starts_with("/enums/0/") {
                    continue;
                }
                if path == "/roles/0/name" || (path.starts_with("/enums/0/labels/") && path.ends_with("/label")) {
                    continue;
                }
                let mut v = json.clone();
                mutate(v.pointer_mut(path).unwrap());
                let Ok(mutated) = serde_json::from_value::<SchemaModel>(v) else {
                    continue;
                };
                let row = path
                    .strip_prefix("/roles/0/member_of/")
                    .and_then(|r| r.split('/').next())
                    .and_then(|k| k.parse::<usize>().ok())
                    .map(|k| rows[k].as_str());
                let hidden = match (field, row) {
                    ("owner_level", Some(role)) => {
                        role == OWNER_LEVEL_ROLE && !path.ends_with("/role/name")
                    }
                    ("member_of", Some(role)) => {
                        role != OWNER_LEVEL_ROLE && membership_merges(role)
                    }
                    ("owner_level" | "member_of", None) => false,
                    (f, _) => *path == format!("/roles/0/{f}"),
                };
                let same = precheck_fingerprint(&mutated, &steps) == base;
                assert_eq!(same, hidden, "{field}: {path}");
                seen += 1;
            }
            assert!(seen > 20, "{field}: only {seen}");
        }

        // The owner's memberships in an updated role are seen.
        let steps = plan_steps(&before);
        let base = precheck_fingerprint(&before, &steps);
        let mut owner = before.clone();
        let mut o = owner.roles[0].clone();
        o.name = crate::plan::OWNER_ROLE.into();
        o.member_of = crate::model::owner_grants(
            &crate::model::NamedRole {
                oid: before.roles[0].oid,
                name: touched.clone(),
            },
            &crate::model::NamedRole {
                oid: Oid(10),
                name: crate::plan::OWNER_ROLE.into(),
            },
        );
        owner.roles.push(o);
        assert_ne!(precheck_fingerprint(&owner, &steps), base);

        // Another value added to the enum the plan adds to is seen.
        let mut other_label = before.clone();
        other_label.enums[0].labels.push(crate::model::EnumLabel {
            oid: Oid(99_997),
            label: "yy".into(),
        });
        assert_ne!(precheck_fingerprint(&other_label, &steps), base);
    }

    // T14 minor 9: a phase 1a statement that doesn't parse can't vouch for anything: the
    // fingerprint never matches (stale), rather than hiding every enum.
    #[test]
    fn an_unparsable_phase1a_step_reads_as_stale() {
        let m = full_model();
        let steps = vec![step("e1", Phase::EnumAdd, "ALTER TYPE (((", None)];
        assert_ne!(precheck_fingerprint(&m, &steps), precheck_fingerprint(&m, &steps));
    }

    // E29, fix round M2: renames are projected by the target's OID, so a chain (Readers → zz,
    // then app → Readers) reads the same before it, halfway and after it.
    #[test]
    fn precheck_follows_the_plans_role_renames() {
        let before = full_model();
        let (app, readers) = (&before.roles[0], &before.roles[1]);
        let rename = |oid: Oid, from: &str, to: &str| {
            step(
                "r",
                Phase::Roles,
                "-- rename",
                Some(RoleOp::Rename {
                    oid,
                    from: from.into(),
                    to: to.into(),
                }),
            )
        };
        let steps = vec![
            rename(readers.oid, &readers.name, "zz"),
            rename(app.oid, &app.name, &readers.name),
        ];
        let (app_oid, readers_oid, readers_name) = (app.oid, readers.oid, readers.name.clone());
        let halfway = before.rename_roles(|oid, _| (oid == readers_oid).then(|| "zz".to_string()));
        let after = halfway.rename_roles(|oid, _| (oid == app_oid).then(|| readers_name.clone()));
        assert_ne!(fingerprint(&after), fingerprint(&before));
        let base = precheck_fingerprint(&before, &steps);
        assert_eq!(precheck_fingerprint(&halfway, &steps), base);
        assert_eq!(precheck_fingerprint(&after, &steps), base);
        // Another change still shows.
        let mut other = after.clone();
        other.tables[0].comment = Some("x".into());
        assert_ne!(precheck_fingerprint(&other, &steps), base);
    }

    #[test]
    fn fingerprint_ignores_the_order_of_unordered_lists() {
        let a = full_model();
        let mut b = full_model();
        b.tables.reverse();
        b.roles.reverse();
        b.tables[0].grants.reverse();
        assert_eq!(fingerprint(&a), fingerprint(&b));

        // Enum labels are ordered: swapping them is a change.
        let mut c = full_model();
        c.enums[0].labels.reverse();
        assert_ne!(fingerprint(&a), fingerprint(&c));
    }
}
