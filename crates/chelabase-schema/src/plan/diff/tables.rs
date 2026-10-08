//! Tables and what hangs off them: columns, constraints, indexes, policies, RLS and grants,
//! with the online transforms for tables the target has (R5).

use super::super::OnlineCheck;
use super::super::ddl::{self, OWNER_ROLE};
use super::{
    Bucket, Gen, constraint_changed, default_grants, index_changed, owner_changed, pkey,
    policy_changed, sorted,
};
use crate::error::SchemaError;
use crate::exclusions::{MERGE_INDEX_PREFIX, MERGE_NOT_NULL_CHECK_PREFIX};
use crate::model::*;
use crate::sql::{ident, qualified};

impl<'a> Gen<'a> {
    // ---- tables ----

    pub(super) fn all_tables(&mut self) -> Result<(), SchemaError> {
        self.dropped_tables();
        for k in 0..self.tps.len() {
            self.existing_table(k)?;
        }
        for &mi in &self.tables.m_only.clone() {
            self.new_table(mi)?;
        }
        Ok(())
    }

    /// Dropped tables, tables that reference others first; a reference cycle loses its
    /// foreign keys first.
    pub(super) fn dropped_tables(&mut self) {
        let t = self.t;
        let dropped: Vec<usize> = self.tables.t_only.clone();
        let name_of = |i: usize| (t.tables[i].schema.as_str(), t.tables[i].name.as_str());
        // refs[x] = dropped tables x references.
        let refs: Vec<Vec<usize>> = dropped
            .iter()
            .map(|&x| {
                t.tables[x]
                    .constraints
                    .iter()
                    .filter_map(|c| c.references.as_ref())
                    .filter_map(|r| {
                        dropped
                            .iter()
                            .position(|&d| name_of(d) == (r.schema.as_str(), r.table.as_str()))
                    })
                    .filter(|&p| dropped[p] != x)
                    .collect()
            })
            .collect();
        let mut referenced_by = vec![0usize; dropped.len()];
        for r in &refs {
            for &p in r {
                referenced_by[p] += 1;
            }
        }
        let mut done = vec![false; dropped.len()];
        let mut order = Vec::new();
        while let Some(p) = (0..dropped.len()).find(|&p| !done[p] && referenced_by[p] == 0) {
            done[p] = true;
            order.push(p);
            for &q in &refs[p] {
                referenced_by[q] -= 1;
            }
        }
        let cycle: Vec<usize> = (0..dropped.len()).filter(|&p| !done[p]).collect();
        for &p in &cycle {
            let a = &t.tables[dropped[p]];
            let tq = self.cur_table_q(a);
            for c in &a.constraints {
                let Some(r) = &c.references else { continue };
                if cycle
                    .iter()
                    .any(|&q| name_of(dropped[q]) == (r.schema.as_str(), r.table.as_str()))
                {
                    let path = [a.schema.as_str(), a.name.as_str(), c.name.as_str()];
                    self.tx(
                        Bucket::DetachForeignKey,
                        "drop-constraint",
                        &path,
                        format!("ALTER TABLE {tq} DROP CONSTRAINT {}", ident(&c.name)),
                    )
                    .key(pkey("constraint", &path));
                }
            }
        }
        order.extend(cycle);
        for p in order {
            let a = &t.tables[dropped[p]];
            let tq = self.cur_table_q(a);
            let o = self.tx(
                Bucket::DropTable,
                "drop-table",
                &[&a.schema, &a.name],
                format!("DROP TABLE {tq}"),
            );
            o.drop = true;
            o.lock("ACCESS EXCLUSIVE", &tq);
            o.key(pkey("table", &[&a.schema, &a.name]));
            for c in &a.columns {
                o.key(pkey("column", &[&a.schema, &a.name, &c.name]));
            }
        }
    }

    /// A dropped target table's current qualified name.
    pub(super) fn cur_table_q(&self, a: &Table) -> String {
        qualified(
            &self.cur_schema(&a.schema),
            &self.cur_name((ObjClass::Table, a.oid, 0), &a.name),
        )
    }

    pub(super) fn existing_table(&mut self, k: usize) -> Result<(), SchemaError> {
        let (t, m) = (self.t, self.m);
        let tp = &self.tps[k];
        let (ti, mi) = (tp.ti, tp.mi);
        let (cols, cons, idx, pols) = (
            tp.cols.clone(),
            tp.cons.clone(),
            tp.idx.clone(),
            tp.pols.clone(),
        );
        let renames = tp.renames.clone();
        let (a, b) = (&t.tables[ti], &m.tables[mi]);
        let tq = qualified(&b.schema, &b.name);
        let s = b.schema.as_str();
        let n = b.name.as_str();

        // Policies.
        for &pi in &pols.t_only {
            let p = &a.policies[pi];
            let name = self.cur_name((ObjClass::Policy, p.oid, 0), &p.name);
            self.tx(
                Bucket::DetachPolicy,
                "drop-policy",
                &[s, n, &p.name],
                format!("DROP POLICY {} ON {tq}", ident(&name)),
            )
            .key(pkey("policy", &[&a.schema, &a.name, &p.name]));
        }
        // Postgres refuses `ALTER COLUMN … TYPE` on a column a policy uses, and `DROP COLUMN`
        // (a column turned generated is dropped and added again) too: those policies are
        // dropped before the change and created again after it, like changed ones.
        let is_conversion = |tc: &Column, mc: &Column| tc.generated.is_none() && mc.generated.is_some();
        let retyped: Vec<(&str, &str)> = cols
            .pairs
            .iter()
            .map(|&(ct, cm)| (&a.columns[ct], &b.columns[cm]))
            .filter(|(tc, mc)| {
                is_conversion(tc, mc)
                    || tc.data_type != mc.data_type
                    || tc.collation != mc.collation
            })
            .map(|(tc, mc)| (tc.name.as_str(), mc.name.as_str()))
            .collect();
        // T14 minor 13: the indexes, constraints and column grants `DROP COLUMN` takes with a
        // converted column are made again after it (as new ones). What the drop takes is read
        // from the target's objects, in the target's names (fix round M5): a merged index that
        // only now names the column is changed as usual.
        let converted: Vec<(&str, i16)> = cols
            .pairs
            .iter()
            .map(|&(ct, cm)| (&a.columns[ct], &b.columns[cm]))
            .filter(|(tc, mc)| is_conversion(tc, mc))
            .map(|(tc, _)| (tc.name.as_str(), tc.attnum))
            .collect();
        let converted_names: Vec<&str> = converted.iter().map(|c| c.0).collect();
        let uses_converted = |ix: &Index| {
            ix.keys
                .iter()
                .chain(&ix.include)
                .chain(&ix.predicate)
                .any(|e| ddl::mentions_identifier(e, &converted_names))
        };
        let on_converted = |c: &Constraint| c.columns.iter().any(|n| converted.iter().any(|x| x.1 == *n));
        let target_names: Vec<&str> = retyped.iter().map(|r| r.0).collect();
        let merged_names: Vec<&str> = retyped.iter().map(|r| r.1).collect();
        let uses_retyped = |p: &Policy, names: &[&str]| {
            [&p.using, &p.with_check]
                .into_iter()
                .flatten()
                .any(|e| ddl::mentions_identifier(e, names))
        };
        for &(pt, pm) in &pols.pairs {
            let (p, q) = (&a.policies[pt], &b.policies[pm]);
            if policy_changed(p, q, &renames)
                || uses_retyped(p, &target_names)
                || uses_retyped(q, &merged_names)
            {
                self.tx(
                    Bucket::DetachPolicy,
                    "drop-policy",
                    &[s, n, &q.name],
                    format!("DROP POLICY {} ON {tq}", ident(&q.name)),
                )
                .key(pkey("policy", &[s, n, &q.name]));
                self.tx(
                    Bucket::Policy,
                    "create-policy",
                    &[s, n, &q.name],
                    ddl::create_policy(q, &tq),
                )
                .key(pkey("policy", &[s, n, &q.name]));
                self.policy_comment(b, q, None);
            } else {
                self.policy_comment(b, q, Some(p));
            }
        }
        for &pm in &pols.m_only {
            let q = &b.policies[pm];
            self.tx(
                Bucket::Policy,
                "create-policy",
                &[s, n, &q.name],
                ddl::create_policy(q, &tq),
            )
            .key(pkey("policy", &[s, n, &q.name]));
            self.policy_comment(b, q, None);
        }

        // Constraints.
        for &ct in &cons.t_only {
            let c = &a.constraints[ct];
            let name = self.cur_name((ObjClass::Constraint, c.oid, 0), &c.name);
            let bucket = if c.kind == ConstraintKind::ForeignKey {
                Bucket::DetachForeignKey
            } else {
                Bucket::DetachConstraint
            };
            self.tx(
                bucket,
                "drop-constraint",
                &[s, n, &c.name],
                format!("ALTER TABLE {tq} DROP CONSTRAINT {}", ident(&name)),
            )
            .lock("ACCESS EXCLUSIVE", &tq)
            .key(pkey("constraint", &[&a.schema, &a.name, &c.name]));
        }
        for &(ct, cm) in &cons.pairs {
            let (c, d) = (&a.constraints[ct], &b.constraints[cm]);
            if on_converted(c) {
                // Dropped with the column: made again.
                self.add_constraint(b, d, true);
            } else if constraint_changed(c, d, &renames) {
                let bucket = if c.kind == ConstraintKind::ForeignKey {
                    Bucket::DetachForeignKey
                } else {
                    Bucket::DetachConstraint
                };
                self.tx(
                    bucket,
                    "drop-constraint",
                    &[s, n, &d.name],
                    format!("ALTER TABLE {tq} DROP CONSTRAINT {}", ident(&d.name)),
                )
                .lock("ACCESS EXCLUSIVE", &tq)
                .key(pkey("constraint", &[s, n, &d.name]));
                self.add_constraint(b, d, true);
            } else if !c.validated && d.validated {
                let bucket = if d.kind == ConstraintKind::ForeignKey {
                    Bucket::ForeignKey
                } else {
                    Bucket::Constraint
                };
                self.online(
                    bucket,
                    "validate-constraint",
                    &[s, n, &d.name],
                    format!("ALTER TABLE {tq} VALIDATE CONSTRAINT {}", ident(&d.name)),
                )
                .online(OnlineCheck::ConstraintValidated {
                    schema: s.to_string(),
                    table: n.to_string(),
                    name: d.name.clone(),
                })
                .lock("SHARE UPDATE EXCLUSIVE", &tq)
                .key(pkey("constraint", &[s, n, &d.name]));
            }
        }
        for &cm in &cons.m_only {
            self.add_constraint(b, &b.constraints[cm], true);
        }

        // Indexes.
        for &it in &idx.t_only {
            let ix = &a.indexes[it];
            let name = self.cur_name((ObjClass::Index, ix.oid, 0), &ix.name);
            self.tx(
                Bucket::DropIndex,
                "drop-index",
                &[s, n, &ix.name],
                format!("DROP INDEX {}", qualified(s, &name)),
            )
            .lock("ACCESS EXCLUSIVE", &tq)
            .key(pkey("index", &[&a.schema, &a.name, &ix.name]));
        }
        let mut created: Vec<usize> = idx.m_only.clone();
        for &(it, im) in &idx.pairs {
            let (x, y) = (&a.indexes[it], &b.indexes[im]);
            let key = pkey("index", &[s, n, &y.name]);
            if uses_converted(x) {
                // Dropped with the column: built again, as a new index.
                created.push(im);
                continue;
            }
            if index_changed(x, y, &renames) {
                self.ix += 1;
                // `chelabase_merge_ix_<n>_<merge id, 32 hex>`: at most 52 + 10 bytes (< 64),
                // unique per merge, so a leftover of another merge is never renamed into place.
                let tmp = format!(
                    "{MERGE_INDEX_PREFIX}{}_{}",
                    self.ix,
                    self.merge_id.simple()
                );
                self.online(
                    Bucket::Index,
                    "create-index",
                    &[s, n, &tmp],
                    ddl::create_index(y, &tmp, &tq, true),
                )
                .online(OnlineCheck::IndexValid {
                    schema: s.to_string(),
                    table: n.to_string(),
                    name: tmp.clone(),
                })
                .lock("SHARE UPDATE EXCLUSIVE", &tq)
                .key(key.clone());
                self.online(
                    Bucket::Index,
                    "drop-index",
                    &[s, n, &y.name],
                    format!(
                        "DROP INDEX CONCURRENTLY IF EXISTS {}",
                        qualified(s, &y.name)
                    ),
                )
                .lock("SHARE UPDATE EXCLUSIVE", &tq)
                .key(key.clone());
                self.online(
                    Bucket::Index,
                    "rename-index",
                    &[s, n, &tmp],
                    format!(
                        "ALTER INDEX IF EXISTS {} RENAME TO {}",
                        qualified(s, &tmp),
                        ident(&y.name)
                    ),
                )
                // Renaming an index takes SHARE UPDATE EXCLUSIVE (Postgres 12 and later).
                .lock("SHARE UPDATE EXCLUSIVE", &tq)
                .key(key.clone());
                if y.comment.is_some() {
                    self.online(
                        Bucket::Index,
                        "comment-index",
                        &[s, n, &y.name],
                        ddl::comment("INDEX", &qualified(s, &y.name), &y.comment),
                    )
                    .lock("SHARE UPDATE EXCLUSIVE", &tq)
                    .key(key);
                }
            } else if x.comment != y.comment {
                self.tx(
                    Bucket::Index,
                    "comment-index",
                    &[s, n, &y.name],
                    ddl::comment("INDEX", &qualified(s, &y.name), &y.comment),
                )
                .key(key);
            }
        }
        for &im in &created {
            let y = &b.indexes[im];
            let key = pkey("index", &[s, n, &y.name]);
            self.online(
                Bucket::Index,
                "create-index",
                &[s, n, &y.name],
                ddl::create_index(y, &y.name, &tq, true),
            )
            .online(OnlineCheck::IndexValid {
                schema: s.to_string(),
                table: n.to_string(),
                name: y.name.clone(),
            })
            .lock("SHARE UPDATE EXCLUSIVE", &tq)
            .key(key.clone());
            if y.comment.is_some() {
                self.online(
                    Bucket::Index,
                    "comment-index",
                    &[s, n, &y.name],
                    ddl::comment("INDEX", &qualified(s, &y.name), &y.comment),
                )
                .lock("SHARE UPDATE EXCLUSIVE", &tq)
                .key(key);
            }
        }

        // Columns.
        for &ct in &cols.t_only {
            let c = &a.columns[ct];
            let name = self.cur_name((ObjClass::Column, a.oid, c.attnum), &c.name);
            let o = self.tx(
                Bucket::DropColumn,
                "drop-column",
                &[s, n, &c.name],
                format!("ALTER TABLE {tq} DROP COLUMN {}", ident(&name)),
            );
            o.drop = true;
            o.lock("ACCESS EXCLUSIVE", &tq);
            o.key(pkey("column", &[&a.schema, &a.name, &c.name]));
        }
        for &(ct, cm) in &cols.pairs {
            self.alter_column(b, &a.columns[ct], &b.columns[cm]);
        }
        // New columns carry the attnum T3 gave them: after the target's last one (T3 contract),
        // so they never share an attnum with a target column and never pair with one, a
        // dropped one included.
        for &cm in &cols.m_only {
            let c = &b.columns[cm];
            self.add_column(b, c, c.attnum);
        }

        // Table properties.
        let key = pkey("table", &[s, n]);
        if owner_changed(&a.owner, &b.owner) {
            self.tx(
                Bucket::Table,
                "alter-table-owner",
                &[s, n],
                format!("ALTER TABLE {tq} OWNER TO {}", ident(&b.owner.name)),
            )
            .key(key.clone());
        }
        if a.comment != b.comment {
            self.tx(
                Bucket::Table,
                "comment-table",
                &[s, n],
                ddl::comment("TABLE", &tq, &b.comment),
            )
            .key(key.clone());
        }
        if a.unlogged != b.unlogged {
            let o = self.tx(
                Bucket::Table,
                "set-persistence",
                &[s, n],
                format!(
                    "ALTER TABLE {tq} SET {}",
                    if b.unlogged { "UNLOGGED" } else { "LOGGED" }
                ),
            );
            o.rewrite = true;
            o.lock("ACCESS EXCLUSIVE", &tq);
            o.key(key.clone());
        }
        if a.options != b.options {
            let removed: Vec<String> = a
                .options
                .iter()
                .map(|o| ddl::option_name(o))
                .filter(|k| !b.options.iter().any(|o| ddl::option_name(o) == *k))
                .map(ddl::option_key)
                .collect();
            if !removed.is_empty() {
                self.tx(
                    Bucket::Table,
                    "reset-options",
                    &[s, n],
                    format!("ALTER TABLE {tq} RESET ({})", removed.join(", ")),
                )
                .key(key.clone());
            }
            let set: Vec<String> = b
                .options
                .iter()
                .filter(|o| !a.options.contains(o))
                .cloned()
                .collect();
            if !set.is_empty() {
                self.tx(
                    Bucket::Table,
                    "set-options",
                    &[s, n],
                    format!("ALTER TABLE {tq} SET ({})", ddl::reloptions(&set)),
                )
                .key(key.clone());
            }
        }
        if a.rls_enabled != b.rls_enabled {
            self.tx(
                Bucket::Rls,
                "row-level-security",
                &[s, n],
                format!(
                    "ALTER TABLE {tq} {} ROW LEVEL SECURITY",
                    if b.rls_enabled { "ENABLE" } else { "DISABLE" }
                ),
            )
            .key(key.clone());
        }
        if a.rls_forced != b.rls_forced {
            self.tx(
                Bucket::Rls,
                "force-row-level-security",
                &[s, n],
                format!(
                    "ALTER TABLE {tq} {} ROW LEVEL SECURITY",
                    if b.rls_forced { "FORCE" } else { "NO FORCE" }
                ),
            )
            .key(key.clone());
        }

        // Grants: the table, its columns, its identity sequences.
        let on = format!("TABLE {tq}");
        self.grants(
            &on,
            &key,
            None,
            Some((&a.grants, &a.owner.name)),
            &b.grants,
            &b.owner.name,
            &[],
            &[s, n],
        )?;
        for &(ct, cm) in &cols.pairs {
            let (c, d) = (&a.columns[ct], &b.columns[cm]);
            let ck = pkey("column", &[s, n, &d.name]);
            // A converted column comes back without its grants.
            let had = (!is_conversion(c, d)).then_some((c.grants.as_slice(), a.owner.name.as_str()));
            self.grants(
                &on,
                &ck,
                Some(&d.name),
                had,
                &d.grants,
                &b.owner.name,
                &[],
                &[s, n],
            )?;
            if let Some(di) = &d.identity {
                let seq = qualified(s, &di.sequence_name);
                let sk = pkey("sequence", &[s, &di.sequence_name]);
                let path = [s, di.sequence_name.as_str()];
                match &c.identity {
                    Some(ci) => self.grants(
                        &format!("SEQUENCE {seq}"),
                        &sk,
                        None,
                        Some((&ci.grants, &a.owner.name)),
                        &di.grants,
                        &b.owner.name,
                        &[],
                        &path,
                    )?,
                    None => {
                        let base = default_grants(
                            &t.default_privileges,
                            s,
                            DefaultPrivilegeObject::Sequences,
                            &b.owner,
                        );
                        self.grants(
                            &format!("SEQUENCE {seq}"),
                            &sk,
                            None,
                            None,
                            &di.grants,
                            &b.owner.name,
                            &base,
                            &path,
                        )?
                    }
                }
            }
        }
        for &cm in &cols.m_only {
            let d = &b.columns[cm];
            let ck = pkey("column", &[s, n, &d.name]);
            self.grants(
                &on,
                &ck,
                Some(&d.name),
                None,
                &d.grants,
                &b.owner.name,
                &[],
                &[s, n],
            )?;
            if let Some(di) = &d.identity {
                let seq = qualified(s, &di.sequence_name);
                let base = default_grants(
                    &t.default_privileges,
                    s,
                    DefaultPrivilegeObject::Sequences,
                    &b.owner,
                );
                self.grants(
                    &format!("SEQUENCE {seq}"),
                    &pkey("sequence", &[s, &di.sequence_name]),
                    None,
                    None,
                    &di.grants,
                    &b.owner.name,
                    &base,
                    &[s, &di.sequence_name],
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn new_table(&mut self, mi: usize) -> Result<(), SchemaError> {
        let (t, m) = (self.t, self.m);
        let b = &m.tables[mi];
        let tq = qualified(&b.schema, &b.name);
        let s = b.schema.as_str();
        let n = b.name.as_str();
        let key = pkey("table", &[s, n]);

        let cols: Vec<String> = b
            .columns
            .iter()
            .map(|c| ddl::column_def(c, s, true))
            .collect();
        let mut sql = format!(
            "CREATE {}TABLE {tq} ({})",
            if b.unlogged { "UNLOGGED " } else { "" },
            cols.join(", ")
        );
        if !b.options.is_empty() {
            sql.push_str(&format!(" WITH ({})", ddl::reloptions(&b.options)));
        }
        let o = self.tx(Bucket::Table, "create-table", &[s, n], sql);
        o.key(key.clone());
        for c in &b.columns {
            o.key(pkey("column", &[s, n, &c.name]));
        }
        if b.owner.name != OWNER_ROLE {
            self.tx(
                Bucket::Table,
                "alter-table-owner",
                &[s, n],
                format!("ALTER TABLE {tq} OWNER TO {}", ident(&b.owner.name)),
            )
            .key(key.clone());
        }
        if b.comment.is_some() {
            self.tx(
                Bucket::Table,
                "comment-table",
                &[s, n],
                ddl::comment("TABLE", &tq, &b.comment),
            )
            .key(key.clone());
        }
        for c in b.columns.iter().filter(|c| c.comment.is_some()) {
            self.tx(
                Bucket::Table,
                "comment-column",
                &[s, n, &c.name],
                ddl::comment("COLUMN", &format!("{tq}.{}", ident(&c.name)), &c.comment),
            )
            .key(pkey("column", &[s, n, &c.name]));
        }
        for c in &b.constraints {
            self.add_constraint(b, c, false);
        }
        for y in &b.indexes {
            let k = pkey("index", &[s, n, &y.name]);
            self.tx(
                Bucket::Index,
                "create-index",
                &[s, n, &y.name],
                ddl::create_index(y, &y.name, &tq, false),
            )
            .lock("SHARE", &tq)
            .key(k.clone());
            if y.comment.is_some() {
                self.tx(
                    Bucket::Index,
                    "comment-index",
                    &[s, n, &y.name],
                    ddl::comment("INDEX", &qualified(s, &y.name), &y.comment),
                )
                .key(k);
            }
        }
        if b.rls_enabled {
            self.tx(
                Bucket::Rls,
                "row-level-security",
                &[s, n],
                format!("ALTER TABLE {tq} ENABLE ROW LEVEL SECURITY"),
            )
            .key(key.clone());
        }
        if b.rls_forced {
            self.tx(
                Bucket::Rls,
                "force-row-level-security",
                &[s, n],
                format!("ALTER TABLE {tq} FORCE ROW LEVEL SECURITY"),
            )
            .key(key.clone());
        }
        for q in &b.policies {
            self.tx(
                Bucket::Policy,
                "create-policy",
                &[s, n, &q.name],
                ddl::create_policy(q, &tq),
            )
            .key(pkey("policy", &[s, n, &q.name]));
            self.policy_comment(b, q, None);
        }
        let on = format!("TABLE {tq}");
        let base = default_grants(
            &t.default_privileges,
            s,
            DefaultPrivilegeObject::Tables,
            &b.owner,
        );
        self.grants(
            &on,
            &key,
            None,
            None,
            &b.grants,
            &b.owner.name,
            &base,
            &[s, n],
        )?;
        for d in &b.columns {
            self.grants(
                &on,
                &pkey("column", &[s, n, &d.name]),
                Some(&d.name),
                None,
                &d.grants,
                &b.owner.name,
                &[],
                &[s, n],
            )?;
            if let Some(di) = &d.identity {
                let base = default_grants(
                    &t.default_privileges,
                    s,
                    DefaultPrivilegeObject::Sequences,
                    &b.owner,
                );
                self.grants(
                    &format!("SEQUENCE {}", qualified(s, &di.sequence_name)),
                    &pkey("sequence", &[s, &di.sequence_name]),
                    None,
                    None,
                    &di.grants,
                    &b.owner.name,
                    &base,
                    &[s, &di.sequence_name],
                )?;
            }
        }
        Ok(())
    }

    /// `COMMENT ON POLICY` when `q`'s comment differs from the one it has (`was`: the target's
    /// unchanged policy; `None`: a policy made in this plan, which has none).
    fn policy_comment(&mut self, b: &Table, q: &Policy, was: Option<&Policy>) {
        if was.map_or(&None, |p| &p.comment) == &q.comment {
            return;
        }
        let tq = qualified(&b.schema, &b.name);
        let (s, n) = (b.schema.as_str(), b.name.as_str());
        self.tx(
            Bucket::Policy,
            "comment-policy",
            &[s, n, &q.name],
            ddl::comment("POLICY", &format!("{} ON {tq}", ident(&q.name)), &q.comment),
        )
        .key(pkey("policy", &[s, n, &q.name]));
    }

    /// Whether a foreign key's referenced key is only built in phase 2.
    pub(super) fn waits_for_online_unique(&self, c: &Constraint) -> bool {
        c.references.as_ref().is_some_and(|r| {
            self.online_uniques
                .contains(&(r.schema.clone(), r.table.clone(), sorted(&r.columns)))
        })
    }

    /// A new (or changed) constraint; `existing`: the table exists on the target (R5).
    pub(super) fn add_constraint(&mut self, b: &Table, c: &Constraint, existing: bool) {
        let tq = qualified(&b.schema, &b.name);
        let (s, n) = (b.schema.as_str(), b.name.as_str());
        let path = [s, n, c.name.as_str()];
        let key = pkey("constraint", &path);
        let def = ddl::strip_not_valid(&c.definition);
        let name = ident(&c.name);
        let add = |not_valid: bool| {
            format!(
                "ALTER TABLE {tq} ADD CONSTRAINT {name} {def}{}",
                if not_valid { " NOT VALID" } else { "" }
            )
        };
        let validated = OnlineCheck::ConstraintValidated {
            schema: s.to_string(),
            table: n.to_string(),
            name: c.name.clone(),
        };
        match c.kind {
            ConstraintKind::PrimaryKey | ConstraintKind::Exclusion => {
                self.tx(Bucket::Constraint, "add-constraint", &path, add(false))
                    .lock("ACCESS EXCLUSIVE", &tq)
                    .key(key);
            }
            ConstraintKind::Unique => {
                match ddl::unique_index_for(def, &c.name, &tq).filter(|_| existing) {
                    Some(index) => {
                        self.online(Bucket::Constraint, "create-unique-index", &path, index)
                            .online(OnlineCheck::IndexValid {
                                schema: s.to_string(),
                                table: n.to_string(),
                                name: c.name.clone(),
                            })
                            .lock("SHARE UPDATE EXCLUSIVE", &tq)
                            .key(key.clone());
                        let deferrable = match (c.deferrable, c.initially_deferred) {
                            (true, true) => " DEFERRABLE INITIALLY DEFERRED",
                            (true, false) => " DEFERRABLE",
                            _ => "",
                        };
                        self.online(
                            Bucket::Constraint,
                            "add-constraint",
                            &path,
                            format!("ALTER TABLE {tq} ADD CONSTRAINT {name} UNIQUE USING INDEX {name}{deferrable}"),
                        )
                        .online(validated)
                        // `ADD CONSTRAINT … USING INDEX` takes ACCESS EXCLUSIVE: brief (no
                        // scan), but it blocks reads and writes while queued (E19).
                        .lock("ACCESS EXCLUSIVE", &tq)
                        .key(key);
                    }
                    None => {
                        self.tx(Bucket::Constraint, "add-constraint", &path, add(false))
                            .lock("ACCESS EXCLUSIVE", &tq)
                            .key(key);
                    }
                }
            }
            ConstraintKind::Check => {
                self.tx(
                    Bucket::Constraint,
                    "add-constraint",
                    &path,
                    add(existing || !c.validated),
                )
                .lock("ACCESS EXCLUSIVE", &tq)
                .key(key.clone());
                if existing && c.validated {
                    self.online(
                        Bucket::Constraint,
                        "validate-constraint",
                        &path,
                        format!("ALTER TABLE {tq} VALIDATE CONSTRAINT {name}"),
                    )
                    .online(validated)
                    .lock("SHARE UPDATE EXCLUSIVE", &tq)
                    .key(key);
                }
            }
            ConstraintKind::ForeignKey => {
                let referenced = c
                    .references
                    .as_ref()
                    .map(|r| qualified(&r.schema, &r.table))
                    .unwrap_or_default();
                let not_valid = existing || !c.validated;
                let o = if self.waits_for_online_unique(c) {
                    self.online(Bucket::ForeignKey, "add-constraint", &path, add(not_valid))
                        .online(OnlineCheck::ConstraintExists {
                            schema: s.to_string(),
                            table: n.to_string(),
                            name: c.name.clone(),
                        })
                } else {
                    self.tx(Bucket::ForeignKey, "add-constraint", &path, add(not_valid))
                };
                o.lock("SHARE ROW EXCLUSIVE", &tq)
                    .lock("SHARE ROW EXCLUSIVE", &referenced)
                    .key(key.clone());
                if existing && c.validated {
                    self.online(
                        Bucket::ForeignKey,
                        "validate-constraint",
                        &path,
                        format!("ALTER TABLE {tq} VALIDATE CONSTRAINT {name}"),
                    )
                    .online(validated)
                    .lock("SHARE UPDATE EXCLUSIVE", &tq)
                    .key(key);
                }
            }
        }
    }

    /// `NOT NULL` on a table the target has: a temporary `CHECK … NOT VALID` in phase 1, then
    /// validated, `SET NOT NULL` (no scan thanks to the check) and the check dropped online.
    ///
    /// The check is `chelabase_merge_nn_<attnum>_<merge id, 32 hex>` (at most 57 bytes): the
    /// merge id keeps another merge's leftover from colliding with it (T14 minor 17). `attnum`
    /// only makes the name unique per column of the table: the target's attnum for a column it
    /// has, the merged model's for a column the merge adds (not necessarily the one Postgres
    /// gives it, when the table has dropped columns).
    pub(super) fn not_null_online(&mut self, b: &Table, column: &str, attnum: i16) {
        let tq = qualified(&b.schema, &b.name);
        let (s, n) = (b.schema.as_str(), b.name.as_str());
        let key = pkey("column", &[s, n, column]);
        let check = format!(
            "{MERGE_NOT_NULL_CHECK_PREFIX}{attnum}_{}",
            self.merge_id.simple()
        );
        let cq = ident(column);
        let path = [s, n, column];
        self.tx(
            Bucket::Column,
            "not-null-check",
            &path,
            format!(
                "ALTER TABLE {tq} ADD CONSTRAINT {} CHECK ({cq} IS NOT NULL) NOT VALID",
                ident(&check)
            ),
        )
        .lock("ACCESS EXCLUSIVE", &tq)
        .key(key.clone());
        self.online(
            Bucket::Column,
            "validate-not-null-check",
            &path,
            format!("ALTER TABLE {tq} VALIDATE CONSTRAINT {}", ident(&check)),
        )
        .online(OnlineCheck::ConstraintValidated {
            schema: s.to_string(),
            table: n.to_string(),
            name: check.clone(),
        })
        .lock("SHARE UPDATE EXCLUSIVE", &tq)
        .key(key.clone());
        self.online(
            Bucket::Column,
            "set-not-null",
            &path,
            format!("ALTER TABLE {tq} ALTER COLUMN {cq} SET NOT NULL"),
        )
        .online(OnlineCheck::NotNullSet {
            schema: s.to_string(),
            table: n.to_string(),
            column: column.to_string(),
        })
        .lock("ACCESS EXCLUSIVE", &tq)
        .key(key.clone());
        self.online(
            Bucket::Column,
            "drop-not-null-check",
            &path,
            format!(
                "ALTER TABLE {tq} DROP CONSTRAINT IF EXISTS {}",
                ident(&check)
            ),
        )
        .lock("ACCESS EXCLUSIVE", &tq)
        .key(key);
    }

    pub(super) fn add_column(&mut self, b: &Table, c: &Column, attnum: i16) {
        let tq = qualified(&b.schema, &b.name);
        let (s, n) = (b.schema.as_str(), b.name.as_str());
        let path = [s, n, c.name.as_str()];
        let key = pkey("column", &path);
        let deferred_not_null =
            c.not_null && c.default.is_none() && c.identity.is_none() && c.generated.is_none();
        let def = ddl::column_def(c, s, !deferred_not_null);
        let o = self.tx(
            Bucket::Column,
            "add-column",
            &path,
            format!("ALTER TABLE {tq} ADD COLUMN {def}"),
        );
        // T14 minor 15: a default that isn't constant (a volatile function) rewrites the table.
        o.rewrite = c.identity.is_some()
            || c.generated.is_some()
            || c.default.as_deref().is_some_and(ddl::volatile_default);
        o.lock("ACCESS EXCLUSIVE", &tq);
        o.key(key.clone());
        if c.comment.is_some() {
            self.tx(
                Bucket::Column,
                "comment-column",
                &path,
                ddl::comment("COLUMN", &format!("{tq}.{}", ident(&c.name)), &c.comment),
            )
            .key(key);
        }
        if deferred_not_null {
            self.not_null_online(b, &c.name, attnum);
        }
    }

    pub(super) fn alter_column(&mut self, b: &Table, tc: &Column, mc: &Column) {
        let tq = qualified(&b.schema, &b.name);
        let (s, n) = (b.schema.as_str(), b.name.as_str());
        let path = [s, n, mc.name.as_str()];
        let key = pkey("column", &path);
        let cq = ident(&mc.name);
        let alter = |what: String| format!("ALTER TABLE {tq} ALTER COLUMN {cq} {what}");

        if tc.generated.is_none() && mc.generated.is_some() {
            // Postgres can't make an existing column generated: drop it and add it again.
            let o = self.tx(
                Bucket::DropColumn,
                "drop-column",
                &path,
                format!("ALTER TABLE {tq} DROP COLUMN {cq}"),
            );
            o.drop = true;
            o.lock("ACCESS EXCLUSIVE", &tq);
            o.key(key.clone());
            let o = self.tx(
                Bucket::Column,
                "add-column",
                &path,
                format!(
                    "ALTER TABLE {tq} ADD COLUMN {}",
                    ddl::column_def(mc, s, true)
                ),
            );
            o.rewrite = true;
            o.lock("ACCESS EXCLUSIVE", &tq);
            o.key(key.clone());
            if mc.comment.is_some() {
                self.tx(
                    Bucket::Column,
                    "comment-column",
                    &path,
                    ddl::comment("COLUMN", &format!("{tq}.{cq}"), &mc.comment),
                )
                .key(key);
            }
            return;
        }

        let type_changed = tc.data_type != mc.data_type || tc.collation != mc.collation;
        let default_changed = tc.default != mc.default;
        if tc.default.is_some() && (default_changed || type_changed) {
            self.tx(
                Bucket::DetachDefault,
                "drop-default",
                &path,
                alter("DROP DEFAULT".into()),
            )
            .lock("ACCESS EXCLUSIVE", &tq)
            .key(key.clone());
        }
        if tc.identity.is_some() && mc.identity.is_none() {
            self.tx(
                Bucket::DetachDefault,
                "drop-identity",
                &path,
                alter("DROP IDENTITY".into()),
            )
            .lock("ACCESS EXCLUSIVE", &tq)
            .key(key.clone());
        }
        if type_changed {
            let collate = match (&mc.collation, &tc.collation) {
                (Some(c), _) => format!(" COLLATE {c}"),
                (None, Some(_)) => " COLLATE pg_catalog.\"default\"".to_string(),
                (None, None) => String::new(),
            };
            let o = self.tx(
                Bucket::Column,
                "alter-column-type",
                &path,
                alter(format!(
                    "TYPE {}{collate} USING {cq}::{}",
                    mc.data_type, mc.data_type
                )),
            );
            o.rewrite = true;
            o.type_change = true;
            o.lock("ACCESS EXCLUSIVE", &tq);
            o.key(key.clone());
        }
        match (&tc.generated, &mc.generated) {
            (Some(x), Some(y)) if x != y => {
                let o = self.tx(
                    Bucket::Column,
                    "set-expression",
                    &path,
                    alter(format!("SET EXPRESSION AS {}", ddl::paren(y))),
                );
                o.rewrite = true;
                o.lock("ACCESS EXCLUSIVE", &tq);
                o.key(key.clone());
            }
            (Some(_), None) => {
                self.tx(
                    Bucket::Column,
                    "drop-expression",
                    &path,
                    alter("DROP EXPRESSION".into()),
                )
                .lock("ACCESS EXCLUSIVE", &tq)
                .key(key.clone());
            }
            _ => {}
        }
        let identity_added = tc.identity.is_none() && mc.identity.is_some();
        match (&tc.identity, &mc.identity) {
            (None, Some(id)) => {
                if !tc.not_null {
                    self.tx(
                        Bucket::Column,
                        "set-not-null",
                        &path,
                        alter("SET NOT NULL".into()),
                    )
                    .lock("ACCESS EXCLUSIVE", &tq)
                    .key(key.clone());
                }
                self.tx(
                    Bucket::Column,
                    "add-identity",
                    &path,
                    alter(format!("ADD {}", ddl::identity_clause(id, s))),
                )
                .lock("ACCESS EXCLUSIVE", &tq)
                .key(key.clone());
            }
            (Some(x), Some(y)) => {
                if x.generation != y.generation {
                    self.tx(
                        Bucket::Column,
                        "set-generated",
                        &path,
                        alter(format!("SET GENERATED {}", ddl::generation(y.generation))),
                    )
                    .lock("ACCESS EXCLUSIVE", &tq)
                    .key(key.clone());
                }
                let changes = ddl::sequence_changes(&x.params, &y.params, false);
                if !changes.is_empty() {
                    let sets: Vec<String> = changes.iter().map(|c| format!("SET {c}")).collect();
                    self.tx(
                        Bucket::Column,
                        "alter-identity",
                        &path,
                        alter(sets.join(" ")),
                    )
                    .lock("ACCESS EXCLUSIVE", &tq)
                    .key(key.clone());
                }
            }
            _ => {}
        }
        if let Some(d) = &mc.default
            && (default_changed || type_changed)
        {
            self.tx(
                Bucket::Column,
                "set-default",
                &path,
                alter(format!("SET DEFAULT {d}")),
            )
            .lock("ACCESS EXCLUSIVE", &tq)
            .key(key.clone());
        }
        if !tc.not_null && mc.not_null && !identity_added {
            self.not_null_online(b, &mc.name, tc.attnum);
        }
        if tc.not_null && !mc.not_null {
            self.tx(
                Bucket::Column,
                "drop-not-null",
                &path,
                alter("DROP NOT NULL".into()),
            )
            .lock("ACCESS EXCLUSIVE", &tq)
            .key(key.clone());
        }
        if tc.comment != mc.comment {
            self.tx(
                Bucket::Column,
                "comment-column",
                &path,
                ddl::comment("COLUMN", &format!("{tq}.{cq}"), &mc.comment),
            )
            .key(key);
        }
    }
}
