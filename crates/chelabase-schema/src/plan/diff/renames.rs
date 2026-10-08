//! Renames: schema renames and new schemas, moves between schemas, then renames per
//! namespace, ordered so they never collide.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::super::Phase;
use super::super::ddl::{self, OWNER_ROLE};
use super::{Bucket, Gen, Pairs, TRef, pkey};
use crate::error::SchemaError;
use crate::model::*;
use crate::sql::{ident, literal, qualified};

// ---- namespaces: renames in an order that never collides ----

#[derive(Clone)]
enum RnKind {
    Schema,
    Table(String),
    Sequence(String),
    Index(String),
    Enum(String),
    Column(String),
    Constraint(String),
    Policy(String),
    Label(String),
}

impl RnKind {
    fn sql(&self, from: &str, to: &str) -> String {
        match self {
            RnKind::Schema => format!("ALTER SCHEMA {} RENAME TO {}", ident(from), ident(to)),
            RnKind::Table(s) => {
                format!("ALTER TABLE {} RENAME TO {}", qualified(s, from), ident(to))
            }
            RnKind::Sequence(s) => {
                format!(
                    "ALTER SEQUENCE {} RENAME TO {}",
                    qualified(s, from),
                    ident(to)
                )
            }
            RnKind::Index(s) => {
                format!("ALTER INDEX {} RENAME TO {}", qualified(s, from), ident(to))
            }
            RnKind::Enum(s) => format!("ALTER TYPE {} RENAME TO {}", qualified(s, from), ident(to)),
            RnKind::Column(t) => {
                format!(
                    "ALTER TABLE {t} RENAME COLUMN {} TO {}",
                    ident(from),
                    ident(to)
                )
            }
            RnKind::Constraint(t) => {
                format!(
                    "ALTER TABLE {t} RENAME CONSTRAINT {} TO {}",
                    ident(from),
                    ident(to)
                )
            }
            RnKind::Policy(t) => format!(
                "ALTER POLICY {} ON {t} RENAME TO {}",
                ident(from),
                ident(to)
            ),
            RnKind::Label(e) => {
                format!(
                    "ALTER TYPE {e} RENAME VALUE {} TO {}",
                    literal(from),
                    literal(to)
                )
            }
        }
    }

    fn noun(&self) -> &'static str {
        match self {
            RnKind::Schema => "schema",
            RnKind::Table(_) => "table",
            RnKind::Sequence(_) => "sequence",
            RnKind::Index(_) => "index",
            RnKind::Enum(_) => "enum",
            RnKind::Column(_) => "column",
            RnKind::Constraint(_) => "constraint",
            RnKind::Policy(_) => "policy",
            RnKind::Label(_) => "enum-value",
        }
    }
}

enum Fate {
    Keep,
    Rename(String),
    /// Dropped; `late` drops (sequences, enums) happen after the creates.
    Drop {
        late: bool,
    },
}

struct NsEntry {
    tref: TRef,
    kind: RnKind,
    cur: String,
    fate: Fate,
    /// Path for the step id and planner keys.
    path: Vec<String>,
    keys: Vec<String>,
}

/// Rename ops `(entry, from, to)` for one namespace, and every entry's final name. A dropped
/// object holding a name a rename needs (or, dropped late, a name the merged model uses) is
/// renamed aside first; a cycle of renames goes through a temporary name.
fn resolve(
    entries: &[NsEntry],
    reserved: &HashSet<String>,
    tmp: &mut u32,
) -> (Vec<(usize, String, String)>, Vec<String>) {
    let mut cur: Vec<String> = entries.iter().map(|e| e.cur.clone()).collect();
    let mut occupied: HashMap<String, usize> = cur
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, n)| (n, i))
        .collect();
    let dests: HashSet<&str> = entries
        .iter()
        .filter_map(|e| match &e.fate {
            Fate::Rename(to) => Some(to.as_str()),
            _ => None,
        })
        .collect();
    let mut ops = Vec::new();
    let mut next_tmp = |occupied: &HashMap<String, usize>| loop {
        *tmp += 1;
        let name = format!("chelabase_merge_tmp_{tmp}");
        if !occupied.contains_key(&name)
            && !reserved.contains(&name)
            && !dests.contains(name.as_str())
        {
            return name;
        }
    };
    for (i, e) in entries.iter().enumerate() {
        if let Fate::Drop { late } = e.fate
            && (dests.contains(cur[i].as_str()) || (late && reserved.contains(&cur[i])))
        {
            let to = next_tmp(&occupied);
            occupied.remove(&cur[i]);
            occupied.insert(to.clone(), i);
            ops.push((i, cur[i].clone(), to.clone()));
            cur[i] = to;
        }
    }
    let mut pending: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(i, e)| matches!(&e.fate, Fate::Rename(to) if *to != cur[*i]))
        .map(|(i, _)| i)
        .collect();
    while !pending.is_empty() {
        let ready = pending.iter().position(|&i| {
            let Fate::Rename(to) = &entries[i].fate else {
                return false;
            };
            occupied.get(to).is_none_or(|&j| j == i)
        });
        match ready {
            Some(pos) => {
                let i = pending.remove(pos);
                let Fate::Rename(to) = &entries[i].fate else {
                    continue;
                };
                occupied.remove(&cur[i]);
                occupied.insert(to.clone(), i);
                ops.push((i, cur[i].clone(), to.clone()));
                cur[i] = to.clone();
            }
            None => {
                let i = pending[0];
                let to = next_tmp(&occupied);
                occupied.remove(&cur[i]);
                occupied.insert(to.clone(), i);
                ops.push((i, cur[i].clone(), to.clone()));
                cur[i] = to;
            }
        }
    }
    (ops, cur)
}

impl<'a> Gen<'a> {
    // ---- renames (and the schemas they need) ----

    pub(super) fn renames(&mut self) -> Result<(), SchemaError> {
        let (t, m) = (self.t, self.m);

        // Schema renames, then new schemas (objects may move into them).
        let mut entries = Vec::new();
        for (ti, s) in t.schemas.iter().enumerate() {
            let fate = match self.schemas.m_of(ti) {
                Some(mi) if m.schemas[mi].name != s.name => {
                    Fate::Rename(m.schemas[mi].name.clone())
                }
                Some(_) => Fate::Keep,
                None => Fate::Drop { late: true },
            };
            let mut keys = vec![pkey("schema", &[&s.name])];
            if let Fate::Rename(to) = &fate {
                keys.push(pkey("schema", &[to]));
            }
            entries.push(NsEntry {
                tref: (ObjClass::Schema, s.oid, 0),
                kind: RnKind::Schema,
                cur: s.name.clone(),
                fate,
                path: vec![s.name.clone()],
                keys,
            });
        }
        let reserved: HashSet<String> = m.schemas.iter().map(|s| s.name.clone()).collect();
        self.apply_ns(Bucket::SchemaRename, entries, &reserved);
        for &mi in &self.schemas.m_only.clone() {
            let s = &m.schemas[mi];
            let k = pkey("schema", &[&s.name]);
            self.emit(
                Bucket::SchemaCreate,
                Phase::Tx,
                "create-schema",
                &[&s.name],
                format!("CREATE SCHEMA {}", ident(&s.name)),
            )
            .key(k.clone());
            if s.owner.name != OWNER_ROLE {
                self.tx(
                    Bucket::SchemaCreate,
                    "alter-schema-owner",
                    &[&s.name],
                    format!(
                        "ALTER SCHEMA {} OWNER TO {}",
                        ident(&s.name),
                        ident(&s.owner.name)
                    ),
                )
                .key(k.clone());
            }
            if s.comment.is_some() {
                self.tx(
                    Bucket::SchemaCreate,
                    "comment-schema",
                    &[&s.name],
                    ddl::comment("SCHEMA", &ident(&s.name), &s.comment),
                )
                .key(k);
            }
        }

        // Moves between schemas.
        for &(ti, mi) in &self.tables.pairs.clone() {
            let (a, b) = (&t.tables[ti], &m.tables[mi]);
            let from = self.cur_schema(&a.schema);
            if from != b.schema {
                self.tx(
                    Bucket::Rename,
                    "move-table",
                    &[&from, &a.name],
                    format!(
                        "ALTER TABLE {} SET SCHEMA {}",
                        qualified(&from, &a.name),
                        ident(&b.schema)
                    ),
                )
                .key(pkey("table", &[&a.schema, &a.name]))
                .key(pkey("table", &[&b.schema, &b.name]));
            }
        }
        for &(ti, mi) in &self.seqs.pairs.clone() {
            let (a, b) = (&t.sequences[ti], &m.sequences[mi]);
            let from = self.cur_schema(&a.schema);
            if from == b.schema || self.moves_with_its_table(a, b) {
                continue;
            }
            self.tx(
                Bucket::Rename,
                "move-sequence",
                &[&from, &a.name],
                format!(
                    "ALTER SEQUENCE {} SET SCHEMA {}",
                    qualified(&from, &a.name),
                    ident(&b.schema)
                ),
            )
            .key(pkey("sequence", &[&a.schema, &a.name]));
        }
        for &(ti, mi) in &self.enums.pairs.clone() {
            let (a, b) = (&t.enums[ti], &m.enums[mi]);
            let from = self.cur_schema(&a.schema);
            if from != b.schema {
                self.tx(
                    Bucket::Rename,
                    "move-enum",
                    &[&from, &a.name],
                    format!(
                        "ALTER TYPE {} SET SCHEMA {}",
                        qualified(&from, &a.name),
                        ident(&b.schema)
                    ),
                )
                .key(pkey("enum", &[&a.schema, &a.name]));
            }
        }

        // Relations (tables, sequences, indexes) and types, per schema.
        let mut rel: BTreeMap<String, Vec<NsEntry>> = BTreeMap::new();
        let mut types: BTreeMap<String, Vec<NsEntry>> = BTreeMap::new();
        for (ti, a) in t.tables.iter().enumerate() {
            let tp = self.tps.iter().find(|tp| tp.ti == ti);
            let b = tp.map(|tp| &m.tables[tp.mi]);
            let schema = b.map_or_else(|| self.cur_schema(&a.schema), |b| b.schema.clone());
            let fate = match b {
                Some(b) if b.name != a.name => Fate::Rename(b.name.clone()),
                Some(_) => Fate::Keep,
                None => Fate::Drop { late: false },
            };
            let mut keys = vec![pkey("table", &[&a.schema, &a.name])];
            if let Some(b) = b {
                keys.push(pkey("table", &[&b.schema, &b.name]));
            }
            rel.entry(schema.clone()).or_default().push(NsEntry {
                tref: (ObjClass::Table, a.oid, 0),
                kind: RnKind::Table(schema.clone()),
                cur: a.name.clone(),
                fate,
                path: vec![schema.clone(), a.name.clone()],
                keys,
            });
            for (ii, ix) in a.indexes.iter().enumerate() {
                let mb = tp
                    .and_then(|tp| tp.idx.m_of(ii))
                    .map(|mi| &b.expect("paired").indexes[mi]);
                let fate = match mb {
                    Some(n) if n.name != ix.name => Fate::Rename(n.name.clone()),
                    Some(_) => Fate::Keep,
                    None => Fate::Drop { late: false },
                };
                let mut keys = vec![pkey("index", &[&a.schema, &a.name, &ix.name])];
                if let (Some(b), Some(n)) = (b, mb) {
                    keys.push(pkey("index", &[&b.schema, &b.name, &n.name]));
                }
                rel.entry(schema.clone()).or_default().push(NsEntry {
                    tref: (ObjClass::Index, ix.oid, 0),
                    kind: RnKind::Index(schema.clone()),
                    cur: ix.name.clone(),
                    fate,
                    path: vec![schema.clone(), ix.name.clone()],
                    keys,
                });
            }
            for (ci, c) in a.columns.iter().enumerate() {
                let Some(id) = &c.identity else { continue };
                let mc = tp
                    .and_then(|tp| tp.cols.m_of(ci))
                    .map(|mi| &b.expect("paired").columns[mi]);
                let fate = match mc.and_then(|mc| mc.identity.as_ref()) {
                    Some(n) if n.sequence_name != id.sequence_name => {
                        Fate::Rename(n.sequence_name.clone())
                    }
                    Some(_) => Fate::Keep,
                    None => Fate::Drop { late: false },
                };
                rel.entry(schema.clone()).or_default().push(NsEntry {
                    tref: (ObjClass::Sequence, id.sequence_oid, 0),
                    kind: RnKind::Sequence(schema.clone()),
                    cur: id.sequence_name.clone(),
                    fate,
                    path: vec![schema.clone(), id.sequence_name.clone()],
                    keys: vec![pkey("column", &[&a.schema, &a.name, &c.name])],
                });
            }
        }
        for (ti, a) in t.sequences.iter().enumerate() {
            let b = self.seqs.m_of(ti).map(|mi| &m.sequences[mi]);
            let schema = b.map_or_else(|| self.cur_schema(&a.schema), |b| b.schema.clone());
            let fate = match b {
                Some(b) if b.name != a.name => Fate::Rename(b.name.clone()),
                Some(_) => Fate::Keep,
                None => Fate::Drop { late: true },
            };
            let mut keys = vec![pkey("sequence", &[&a.schema, &a.name])];
            if let Some(b) = b {
                keys.push(pkey("sequence", &[&b.schema, &b.name]));
            }
            rel.entry(schema.clone()).or_default().push(NsEntry {
                tref: (ObjClass::Sequence, a.oid, 0),
                kind: RnKind::Sequence(schema.clone()),
                cur: a.name.clone(),
                fate,
                path: vec![schema.clone(), a.name.clone()],
                keys,
            });
        }
        for (ti, a) in t.enums.iter().enumerate() {
            let b = self.enums.m_of(ti).map(|mi| &m.enums[mi]);
            let schema = b.map_or_else(|| self.cur_schema(&a.schema), |b| b.schema.clone());
            let fate = match b {
                Some(b) if b.name != a.name => Fate::Rename(b.name.clone()),
                Some(_) => Fate::Keep,
                None => Fate::Drop { late: true },
            };
            let mut keys = vec![pkey("enum", &[&a.schema, &a.name])];
            if let Some(b) = b {
                keys.push(pkey("enum", &[&b.schema, &b.name]));
            }
            types.entry(schema.clone()).or_default().push(NsEntry {
                tref: (ObjClass::Enum, a.oid, 0),
                kind: RnKind::Enum(schema.clone()),
                cur: a.name.clone(),
                fate,
                path: vec![schema.clone(), a.name.clone()],
                keys,
            });
        }
        for (schema, entries) in rel {
            let mut reserved = HashSet::new();
            for b in m.tables.iter().filter(|b| b.schema == schema) {
                reserved.insert(b.name.clone());
                reserved.extend(b.indexes.iter().map(|i| i.name.clone()));
                reserved.extend(
                    b.constraints
                        .iter()
                        .filter(|c| {
                            matches!(
                                c.kind,
                                ConstraintKind::PrimaryKey
                                    | ConstraintKind::Unique
                                    | ConstraintKind::Exclusion
                            )
                        })
                        .map(|c| c.name.clone()),
                );
                reserved.extend(
                    b.columns
                        .iter()
                        .filter_map(|c| c.identity.as_ref().map(|i| i.sequence_name.clone())),
                );
            }
            reserved.extend(
                m.sequences
                    .iter()
                    .filter(|q| q.schema == schema)
                    .map(|q| q.name.clone()),
            );
            self.apply_ns(Bucket::Rename, entries, &reserved);
        }
        for (schema, entries) in types {
            let mut reserved: HashSet<String> = m
                .enums
                .iter()
                .filter(|e| e.schema == schema)
                .map(|e| e.name.clone())
                .collect();
            reserved.extend(
                m.tables
                    .iter()
                    .filter(|b| b.schema == schema)
                    .map(|b| b.name.clone()),
            );
            self.apply_ns(Bucket::Rename, entries, &reserved);
        }

        // Columns, constraints and policies, per table.
        let none = HashSet::new();
        for k in 0..self.tps.len() {
            let tp = &self.tps[k];
            let (a, b) = (&t.tables[tp.ti], &m.tables[tp.mi]);
            let tq = qualified(&b.schema, &b.name);
            let sub = |kind: RnKind,
                       noun: &str,
                       pairs: &Pairs,
                       names_t: Vec<(String, TRef)>,
                       names_m: Vec<String>|
             -> Vec<NsEntry> {
                names_t
                    .into_iter()
                    .enumerate()
                    .map(|(ti, (name, tref))| {
                        let fate = match pairs.m_of(ti) {
                            Some(mi) if names_m[mi] != name => Fate::Rename(names_m[mi].clone()),
                            Some(_) => Fate::Keep,
                            None => Fate::Drop { late: false },
                        };
                        let mut keys = vec![pkey(noun, &[&a.schema, &a.name, &name])];
                        if let Fate::Rename(to) = &fate {
                            keys.push(pkey(noun, &[&b.schema, &b.name, to]));
                        }
                        NsEntry {
                            tref,
                            kind: kind.clone(),
                            cur: name.clone(),
                            fate,
                            path: vec![b.schema.clone(), b.name.clone(), name],
                            keys,
                        }
                    })
                    .collect()
            };
            let cols = sub(
                RnKind::Column(tq.clone()),
                "column",
                &tp.cols,
                a.columns
                    .iter()
                    .map(|c| (c.name.clone(), (ObjClass::Column, a.oid, c.attnum)))
                    .collect(),
                b.columns.iter().map(|c| c.name.clone()).collect(),
            );
            let cons = sub(
                RnKind::Constraint(tq.clone()),
                "constraint",
                &tp.cons,
                a.constraints
                    .iter()
                    .map(|c| (c.name.clone(), (ObjClass::Constraint, c.oid, 0)))
                    .collect(),
                b.constraints.iter().map(|c| c.name.clone()).collect(),
            );
            let pols = sub(
                RnKind::Policy(tq.clone()),
                "policy",
                &tp.pols,
                a.policies
                    .iter()
                    .map(|p| (p.name.clone(), (ObjClass::Policy, p.oid, 0)))
                    .collect(),
                b.policies.iter().map(|p| p.name.clone()).collect(),
            );
            self.apply_ns(Bucket::Rename, cols, &none);
            self.apply_ns(Bucket::Rename, cons, &none);
            self.apply_ns(Bucket::Rename, pols, &none);
        }

        // Enum values: renamed, never removed.
        for k in 0..self.enums.pairs.len() {
            let (ti, mi) = self.enums.pairs[k];
            let (a, b) = (&t.enums[ti], &m.enums[mi]);
            let lp = &self.labels[k];
            if !lp.t_only.is_empty() {
                let gone: Vec<&str> = lp
                    .t_only
                    .iter()
                    .map(|&i| a.labels[i].label.as_str())
                    .collect();
                return Err(SchemaError::Unplannable {
                    object: format!("{}.{}", b.schema, b.name),
                    reason: format!(
                        "Postgres can't remove enum values ({}); merge this enum with own SQL",
                        gone.join(", ")
                    ),
                });
            }
            let eq = qualified(&b.schema, &b.name);
            let entries = a
                .labels
                .iter()
                .enumerate()
                .map(|(ti, l)| {
                    let to = &b.labels[lp.m_of(ti).expect("every label is paired")].label;
                    NsEntry {
                        tref: (ObjClass::EnumLabel, l.oid, 0),
                        kind: RnKind::Label(eq.clone()),
                        cur: l.label.clone(),
                        fate: if *to == l.label {
                            Fate::Keep
                        } else {
                            Fate::Rename(to.clone())
                        },
                        path: vec![b.schema.clone(), b.name.clone(), l.label.clone()],
                        keys: vec![
                            pkey("enum", &[&a.schema, &a.name]),
                            pkey("enum", &[&b.schema, &b.name]),
                        ],
                    }
                })
                .collect();
            self.apply_ns(Bucket::Rename, entries, &none);
        }
        Ok(())
    }

    /// A sequence owned by a column of a table that moves to the sequence's new schema moves
    /// with it (`ALTER TABLE … SET SCHEMA` takes owned sequences along).
    fn moves_with_its_table(&self, a: &Sequence, b: &Sequence) -> bool {
        let Some(ob) = &a.owned_by else { return false };
        self.tables.pairs.iter().any(|&(ti, mi)| {
            let (ta, tb) = (&self.t.tables[ti], &self.m.tables[mi]);
            ta.schema == ob.schema
                && ta.name == ob.table
                && self.cur_schema(&ta.schema) != tb.schema
                && tb.schema == b.schema
        })
    }

    fn apply_ns(&mut self, bucket: Bucket, entries: Vec<NsEntry>, reserved: &HashSet<String>) {
        let (ops, finals) = resolve(&entries, reserved, &mut self.tmp);
        for (i, from, to) in ops {
            let e = &entries[i];
            let sql = e.kind.sql(&from, &to);
            let path: Vec<&str> = e.path.iter().map(String::as_str).collect();
            let verb = format!("rename-{}", e.kind.noun());
            let keys = e.keys.clone();
            let o = self.tx(bucket, &verb, &path, sql);
            for k in keys {
                o.key(k);
            }
        }
        for (e, name) in entries.iter().zip(finals) {
            if matches!(e.fate, Fate::Drop { .. }) {
                self.cur.insert(e.tref, name);
            }
        }
    }
}
