//! Schemas, grants, enums, sequences and default privileges.

use std::collections::BTreeMap;

use super::super::ddl::{self, OWNER_ROLE};
use super::{
    Bucket, EnumAdd, Gen, default_grants, defpriv_pkey, grant_pkey, grant_sort_key, owner_changed,
    pkey,
};
use crate::error::SchemaError;
use crate::model::*;
use crate::sql::{ident, literal, qualified};

impl<'a> Gen<'a> {
    // ---- schemas ----

    pub(super) fn schemas(&mut self) -> Result<(), SchemaError> {
        let (t, m) = (self.t, self.m);
        for &(ti, mi) in &self.schemas.pairs.clone() {
            let (a, b) = (&t.schemas[ti], &m.schemas[mi]);
            let k = pkey("schema", &[&b.name]);
            if owner_changed(&a.owner, &b.owner) {
                self.tx(
                    Bucket::Schema,
                    "alter-schema-owner",
                    &[&b.name],
                    format!(
                        "ALTER SCHEMA {} OWNER TO {}",
                        ident(&b.name),
                        ident(&b.owner.name)
                    ),
                )
                .key(k.clone());
            }
            if a.comment != b.comment {
                self.tx(
                    Bucket::Schema,
                    "comment-schema",
                    &[&b.name],
                    ddl::comment("SCHEMA", &ident(&b.name), &b.comment),
                )
                .key(k);
            }
            let on = format!("SCHEMA {}", ident(&b.name));
            self.grants(
                &on,
                &pkey("schema", &[&b.name]),
                None,
                Some((&a.grants, &a.owner.name)),
                &b.grants,
                &b.owner.name,
                &[],
                &[&b.name],
            )?;
        }
        for &mi in &self.schemas.m_only.clone() {
            let b = &m.schemas[mi];
            let on = format!("SCHEMA {}", ident(&b.name));
            self.grants(
                &on,
                &pkey("schema", &[&b.name]),
                None,
                None,
                &b.grants,
                &b.owner.name,
                &[],
                &[&b.name],
            )?;
        }
        for &ti in &self.schemas.t_only.clone() {
            let a = &t.schemas[ti];
            let name = self.cur_name((ObjClass::Schema, a.oid, 0), &a.name);
            self.tx(
                Bucket::DropSchema,
                "drop-schema",
                &[&a.name],
                format!("DROP SCHEMA {}", ident(&name)),
            )
            .key(pkey("schema", &[&a.name]));
        }
        Ok(())
    }

    /// Revokes then grants turning `t` (or, for a new object, `baseline`) into `m`. Privileges
    /// of the object's owner are implicit and never touched.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn grants(
        &mut self,
        on: &str,
        on_key: &str,
        column: Option<&str>,
        t: Option<(&[Grant], &str)>,
        m: &[Grant],
        m_owner: &str,
        baseline: &[Grant],
        path: &[&str],
    ) -> Result<(), SchemaError> {
        let gkey = |g: &Grant| {
            (
                grant_sort_key(g).0,
                grant_sort_key(g).1,
                grant_sort_key(g).2,
            )
        };
        let filter = |gs: &[Grant], owner: &str| -> BTreeMap<(u8, String, String), Grant> {
            gs.iter()
                .filter(|g| {
                    ddl::grantee_name(&g.grantee) != owner || matches!(g.grantee, RoleRef::Public)
                })
                .map(|g| (gkey(g), g.clone()))
                .collect()
        };
        let mm = filter(m, m_owner);
        let (tt, revoke) = match t {
            Some((gs, owner)) => (filter(gs, owner), true),
            None => (filter(baseline, m_owner), false),
        };
        let bad = |g: &Grant| SchemaError::Unplannable {
            object: path.join("."),
            reason: format!("unknown privilege {:?}", g.privilege),
        };
        let path_of = |g: &Grant| {
            let mut p: Vec<String> = path.iter().map(|s| s.to_string()).collect();
            if let Some(c) = column {
                p.push(c.to_string());
            }
            p.push(ddl::grantee_name(&g.grantee).to_string());
            p.push(g.privilege.to_lowercase());
            p
        };
        let mut todo: Vec<(&str, String, Grant)> = Vec::new();
        if revoke {
            for (k, g) in &tt {
                match mm.get(k) {
                    None => todo.push((
                        "revoke",
                        ddl::revoke_sql(g, on, column, false).ok_or_else(|| bad(g))?,
                        g.clone(),
                    )),
                    Some(n) if g.grantable && !n.grantable => todo.push((
                        "revoke-grant-option",
                        ddl::revoke_sql(g, on, column, true).ok_or_else(|| bad(g))?,
                        g.clone(),
                    )),
                    Some(_) => {}
                }
            }
        }
        for (k, g) in &mm {
            let needed = match tt.get(k) {
                None => true,
                Some(o) => g.grantable && !o.grantable,
            };
            if needed {
                todo.push((
                    "grant",
                    ddl::grant_sql(g, on, column).ok_or_else(|| bad(g))?,
                    g.clone(),
                ));
            }
        }
        for (verb, sql, g) in todo {
            let p = path_of(&g);
            let p: Vec<&str> = p.iter().map(String::as_str).collect();
            self.tx(Bucket::Grant, verb, &p, sql)
                .key(grant_pkey(on_key, &g));
        }
        Ok(())
    }

    // ---- enums ----

    pub(super) fn enum_types(&mut self) -> Result<(), SchemaError> {
        let (t, m) = (self.t, self.m);
        for &mi in &self.enums.m_only.clone() {
            let b = &m.enums[mi];
            let q = qualified(&b.schema, &b.name);
            let k = pkey("enum", &[&b.schema, &b.name]);
            let labels: Vec<String> = b.labels.iter().map(|l| literal(&l.label)).collect();
            self.tx(
                Bucket::Enum,
                "create-enum",
                &[&b.schema, &b.name],
                format!("CREATE TYPE {q} AS ENUM ({})", labels.join(", ")),
            )
            .key(k.clone());
            if b.owner.name != OWNER_ROLE {
                self.tx(
                    Bucket::Enum,
                    "alter-enum-owner",
                    &[&b.schema, &b.name],
                    format!("ALTER TYPE {q} OWNER TO {}", ident(&b.owner.name)),
                )
                .key(k.clone());
            }
            if b.comment.is_some() {
                self.tx(
                    Bucket::Enum,
                    "comment-enum",
                    &[&b.schema, &b.name],
                    ddl::comment("TYPE", &q, &b.comment),
                )
                .key(k);
            }
        }
        for k in 0..self.enums.pairs.len() {
            let (ti, mi) = self.enums.pairs[k];
            let (a, b) = (&t.enums[ti], &m.enums[mi]);
            let q = qualified(&b.schema, &b.name);
            let q_t = qualified(&a.schema, &a.name);
            let key = pkey("enum", &[&b.schema, &b.name]);
            let lp = self.labels[k].clone();
            // Values kept must keep their order.
            let mut kept: Vec<(usize, usize)> = lp.pairs.clone();
            kept.sort_by_key(|p| p.1);
            if kept.windows(2).any(|w| w[0].0 > w[1].0) {
                return Err(SchemaError::Unplannable {
                    object: format!("{}.{}", b.schema, b.name),
                    reason: "Postgres can't reorder enum values; merge this enum with own SQL"
                        .to_string(),
                });
            }
            let t_label = |mi: usize| -> String {
                match lp.pairs.iter().find(|p| p.1 == mi) {
                    Some(&(ti, _)) => a.labels[ti].label.clone(),
                    None => b.labels[mi].label.clone(),
                }
            };
            for &ni in &lp.m_only {
                let label = &b.labels[ni].label;
                let (position, neighbor) = if ni > 0 {
                    ("AFTER", Some(ni - 1))
                } else {
                    (
                        "BEFORE",
                        (1..b.labels.len()).find(|i| lp.pairs.iter().any(|p| p.1 == *i)),
                    )
                };
                let (sql, sql_1a) = match neighbor {
                    Some(n) => (
                        format!(
                            "ALTER TYPE {q} ADD VALUE IF NOT EXISTS {} {position} {}",
                            literal(label),
                            literal(&b.labels[n].label)
                        ),
                        format!(
                            "ALTER TYPE {q_t} ADD VALUE IF NOT EXISTS {} {position} {}",
                            literal(label),
                            literal(&t_label(n))
                        ),
                    ),
                    None => (
                        format!("ALTER TYPE {q} ADD VALUE IF NOT EXISTS {}", literal(label)),
                        format!(
                            "ALTER TYPE {q_t} ADD VALUE IF NOT EXISTS {}",
                            literal(label)
                        ),
                    ),
                };
                let o = self.tx(
                    Bucket::Enum,
                    "add-enum-value",
                    &[&b.schema, &b.name, label],
                    sql,
                );
                o.key(key.clone());
                o.enum_add = Some(EnumAdd {
                    group: key.clone(),
                    label: label.clone(),
                    sql_1a,
                    // A new value of a name the target has: that value is renamed away (or the
                    // plan couldn't keep the target's order).
                    renamed_away: a.labels.iter().any(|l| l.label == *label),
                });
            }
            if owner_changed(&a.owner, &b.owner) {
                self.tx(
                    Bucket::Enum,
                    "alter-enum-owner",
                    &[&b.schema, &b.name],
                    format!("ALTER TYPE {q} OWNER TO {}", ident(&b.owner.name)),
                )
                .key(key.clone());
            }
            if a.comment != b.comment {
                self.tx(
                    Bucket::Enum,
                    "comment-enum",
                    &[&b.schema, &b.name],
                    ddl::comment("TYPE", &q, &b.comment),
                )
                .key(key);
            }
        }
        for &ti in &self.enums.t_only.clone() {
            let a = &t.enums[ti];
            let name = self.cur_name((ObjClass::Enum, a.oid, 0), &a.name);
            let schema = self.cur_schema(&a.schema);
            self.tx(
                Bucket::DropEnum,
                "drop-enum",
                &[&a.schema, &a.name],
                format!("DROP TYPE {}", qualified(&schema, &name)),
            )
            .key(pkey("enum", &[&a.schema, &a.name]));
        }
        Ok(())
    }

    // ---- sequences ----

    /// A target `OWNED BY` in the merged model's names (`None` when that column goes).
    pub(super) fn owned_by_in_m(&self, ob: &OwnedBy) -> Option<(String, String, String)> {
        let (t, m) = (self.t, self.m);
        let tp = self.tps.iter().find(|tp| {
            let a = &t.tables[tp.ti];
            a.schema == ob.schema && a.name == ob.table
        })?;
        let a = &t.tables[tp.ti];
        let ci = a.columns.iter().position(|c| c.attnum == ob.attnum)?;
        let b = &m.tables[tp.mi];
        let mc = &b.columns[tp.cols.m_of(ci)?];
        Some((b.schema.clone(), b.name.clone(), mc.name.clone()))
    }

    pub(super) fn sequences(&mut self) -> Result<(), SchemaError> {
        let (t, m) = (self.t, self.m);
        let owned_sql = |q: &str, ob: &OwnedBy| {
            format!(
                "ALTER SEQUENCE {q} OWNED BY {}.{}",
                qualified(&ob.schema, &ob.table),
                ident(&ob.column)
            )
        };
        for &mi in &self.seqs.m_only.clone() {
            let b = &m.sequences[mi];
            let q = qualified(&b.schema, &b.name);
            let k = pkey("sequence", &[&b.schema, &b.name]);
            let path = [b.schema.as_str(), b.name.as_str()];
            self.tx(
                Bucket::Sequence,
                "create-sequence",
                &path,
                format!("CREATE SEQUENCE {q} {}", ddl::sequence_options(&b.params)),
            )
            .key(k.clone());
            if b.owner.name != OWNER_ROLE {
                self.tx(
                    Bucket::Sequence,
                    "alter-sequence-owner",
                    &path,
                    format!("ALTER SEQUENCE {q} OWNER TO {}", ident(&b.owner.name)),
                )
                .key(k.clone());
            }
            if b.comment.is_some() {
                self.tx(
                    Bucket::Sequence,
                    "comment-sequence",
                    &path,
                    ddl::comment("SEQUENCE", &q, &b.comment),
                )
                .key(k.clone());
            }
            if let Some(ob) = &b.owned_by {
                self.tx(
                    Bucket::OwnedBy,
                    "sequence-owned-by",
                    &path,
                    owned_sql(&q, ob),
                )
                .key(k.clone());
            }
            let base = default_grants(
                &t.default_privileges,
                &b.schema,
                DefaultPrivilegeObject::Sequences,
                &b.owner,
            );
            self.grants(
                &format!("SEQUENCE {q}"),
                &k,
                None,
                None,
                &b.grants,
                &b.owner.name,
                &base,
                &path,
            )?;
        }
        for &(ti, mi) in &self.seqs.pairs.clone() {
            let (a, b) = (&t.sequences[ti], &m.sequences[mi]);
            let q = qualified(&b.schema, &b.name);
            let k = pkey("sequence", &[&b.schema, &b.name]);
            let path = [b.schema.as_str(), b.name.as_str()];
            let changes = ddl::sequence_changes(&a.params, &b.params, true);
            if !changes.is_empty() {
                self.tx(
                    Bucket::Sequence,
                    "alter-sequence",
                    &path,
                    format!("ALTER SEQUENCE {q} {}", changes.join(" ")),
                )
                .key(k.clone());
            }
            let t_owned = a.owned_by.as_ref().and_then(|ob| self.owned_by_in_m(ob));
            let m_owned = b
                .owned_by
                .as_ref()
                .map(|ob| (ob.schema.clone(), ob.table.clone(), ob.column.clone()));
            if a.owned_by.is_some() && (t_owned.is_none() || t_owned != m_owned) {
                self.tx(
                    Bucket::DetachDefault,
                    "sequence-owned-by-none",
                    &path,
                    format!("ALTER SEQUENCE {q} OWNED BY NONE"),
                )
                .key(k.clone());
            }
            if let Some(ob) = &b.owned_by
                && (a.owned_by.is_none() || t_owned != m_owned)
            {
                self.tx(
                    Bucket::OwnedBy,
                    "sequence-owned-by",
                    &path,
                    owned_sql(&q, ob),
                )
                .key(k.clone());
            }
            if owner_changed(&a.owner, &b.owner) {
                self.tx(
                    Bucket::Sequence,
                    "alter-sequence-owner",
                    &path,
                    format!("ALTER SEQUENCE {q} OWNER TO {}", ident(&b.owner.name)),
                )
                .key(k.clone());
            }
            if a.comment != b.comment {
                self.tx(
                    Bucket::Sequence,
                    "comment-sequence",
                    &path,
                    ddl::comment("SEQUENCE", &q, &b.comment),
                )
                .key(k.clone());
            }
            self.grants(
                &format!("SEQUENCE {q}"),
                &k,
                None,
                Some((&a.grants, &a.owner.name)),
                &b.grants,
                &b.owner.name,
                &[],
                &path,
            )?;
        }
        for &ti in &self.seqs.t_only.clone() {
            let a = &t.sequences[ti];
            let name = self.cur_name((ObjClass::Sequence, a.oid, 0), &a.name);
            let schema = self.cur_schema(&a.schema);
            // IF EXISTS: a sequence owned by a dropped column is already gone.
            self.tx(
                Bucket::DropSequence,
                "drop-sequence",
                &[&a.schema, &a.name],
                format!("DROP SEQUENCE IF EXISTS {}", qualified(&schema, &name)),
            )
            .key(pkey("sequence", &[&a.schema, &a.name]));
        }
        Ok(())
    }

    // ---- default privileges ----

    pub(super) fn default_privileges(&mut self) -> Result<(), SchemaError> {
        let (t, m) = (self.t, self.m);
        type DKey = (String, Option<String>, DefaultPrivilegeObject);
        let dkey = |d: &DefaultPrivilege| -> DKey {
            (d.role.name.clone(), d.schema.clone(), d.object_type)
        };
        let mut keys: Vec<DKey> = m.default_privileges.iter().map(dkey).collect();
        for d in &t.default_privileges {
            let k = dkey(d);
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        for (role, schema, object) in keys {
            let find = |model: &'a SchemaModel| {
                model
                    .default_privileges
                    .iter()
                    .find(|d| d.role.name == role && d.schema == schema && d.object_type == object)
            };
            let hardwired = || -> Vec<Grant> {
                if schema.is_some() {
                    return Vec::new();
                }
                let (owner, public) = ddl::hardwired_defaults(object);
                let r = RoleRef::Role {
                    oid: Oid(0),
                    name: role.clone(),
                };
                let mk = |grantee: &RoleRef, p: &&str| Grant {
                    grantor: NamedRole {
                        oid: Oid(0),
                        name: role.clone(),
                    },
                    grantee: grantee.clone(),
                    privilege: p.to_string(),
                    grantable: false,
                };
                owner
                    .iter()
                    .map(|p| mk(&r, p))
                    .chain(public.iter().map(|p| mk(&RoleRef::Public, p)))
                    .collect()
            };
            let tg = find(t).map_or_else(hardwired, |d| d.grants.clone());
            let mg = find(m).map_or_else(hardwired, |d| d.grants.clone());
            let index = |gs: &[Grant]| -> BTreeMap<(u8, String, String), Grant> {
                gs.iter().map(|g| (grant_sort_key(g), g.clone())).collect()
            };
            let (ti, mi) = (index(&tg), index(&mg));
            let prefix = format!(
                "ALTER DEFAULT PRIVILEGES FOR ROLE {}{}",
                ident(&role),
                schema
                    .as_ref()
                    .map_or(String::new(), |s| format!(" IN SCHEMA {}", ident(s)))
            );
            let objects = ddl::default_object(object);
            let pk = defpriv_pkey(&role, schema.as_deref(), object);
            let bad = |g: &Grant| SchemaError::Unplannable {
                object: format!("default privileges for {role}"),
                reason: format!("unknown privilege {:?}", g.privilege),
            };
            let mut todo: Vec<(String, Grant)> = Vec::new();
            for (k, g) in &ti {
                let option_only = match mi.get(k) {
                    None => false,
                    Some(n) if g.grantable && !n.grantable => true,
                    Some(_) => continue,
                };
                let p = ddl::privilege(&g.privilege).ok_or_else(|| bad(g))?;
                todo.push((
                    format!(
                        "{prefix} REVOKE {}{p} ON {objects} FROM {}",
                        if option_only { "GRANT OPTION FOR " } else { "" },
                        ddl::role_ref(&g.grantee)
                    ),
                    g.clone(),
                ));
            }
            for (k, g) in &mi {
                if ti.get(k).is_some_and(|o| o.grantable || !g.grantable) {
                    continue;
                }
                let p = ddl::privilege(&g.privilege).ok_or_else(|| bad(g))?;
                todo.push((
                    format!(
                        "{prefix} GRANT {p} ON {objects} TO {}{}",
                        ddl::role_ref(&g.grantee),
                        if g.grantable {
                            " WITH GRANT OPTION"
                        } else {
                            ""
                        }
                    ),
                    g.clone(),
                ));
            }
            for (sql, g) in todo {
                let path = [
                    role.as_str(),
                    schema.as_deref().unwrap_or("*"),
                    objects,
                    ddl::grantee_name(&g.grantee),
                    g.privilege.as_str(),
                ];
                self.tx(Bucket::DefaultPrivilege, "default-privilege", &path, sql)
                    .key(pk.clone());
            }
        }
        Ok(())
    }
}
