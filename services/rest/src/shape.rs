//! The shape hash: the plan cache's key for a tree with its values removed.
//!
//! It covers everything that changes the SQL text or the bind plan (operation kinds,
//! names as written, column lists, filter structure and operators, the `is` constant,
//! which optional parts are present) and nothing else: no values, no row counts, no `in`
//! lengths. Two trees with the same hash produce the same [`crate::sql::Plan`] for the
//! same catalog, `max_rows` and `default_schema`.
//!
//! It's keyed by one [`RandomState`] per process: stable for the process's lifetime, which
//! is all the cache needs, and not precomputable from outside, so crafted trees can't be
//! aimed at colliding cache keys.

use std::hash::{BuildHasher, DefaultHasher, Hash, Hasher, RandomState};
use std::sync::OnceLock;

use serde_json::Value;

use crate::query::{Filter, Op, Returning, SelectItem};

pub fn shape_hash(op: &Op) -> u64 {
    static KEY: OnceLock<RandomState> = OnceLock::new();
    let mut h = KEY.get_or_init(RandomState::new).build_hasher();
    hash_op(op, &mut h);
    h.finish()
}

fn sorted_keys(v: &Value) -> Vec<&String> {
    let mut keys: Vec<&String> = v
        .as_object()
        .map(|m| m.keys().collect())
        .unwrap_or_default();
    keys.sort();
    keys
}

fn hash_op(op: &Op, h: &mut DefaultHasher) {
    match op {
        Op::Select(s) => {
            0u8.hash(h);
            s.from.hash(h);
            match &s.select {
                None => 0u8.hash(h),
                Some(items) => {
                    1u8.hash(h);
                    items.len().hash(h);
                    for item in items {
                        match item {
                            SelectItem::Col(c) => (0u8, c).hash(h),
                            SelectItem::Aliased { col, alias } => (1u8, col, alias).hash(h),
                        }
                    }
                }
            }
            hash_filter_opt(s.filter.as_ref(), h);
            s.order_by.len().hash(h);
            for o in &s.order_by {
                (&o.col, o.dir, o.nulls).hash(h);
            }
            (s.limit.is_some(), s.offset.is_some()).hash(h);
        }
        Op::Insert(i) => {
            1u8.hash(h);
            i.into.hash(h);
            let first = i.values.as_array().and_then(|rows| rows.first());
            sorted_keys(first.unwrap_or(&Value::Null)).hash(h);
            hash_returning(i.returning.as_ref(), h);
        }
        Op::Update(u) => {
            2u8.hash(h);
            u.table.hash(h);
            sorted_keys(&u.set).hash(h);
            hash_filter_opt(u.filter.as_ref(), h);
            u.all.hash(h);
            hash_returning(u.returning.as_ref(), h);
        }
        Op::Delete(d) => {
            3u8.hash(h);
            d.from.hash(h);
            hash_filter_opt(d.filter.as_ref(), h);
            d.all.hash(h);
            hash_returning(d.returning.as_ref(), h);
        }
        Op::Call(c) => {
            4u8.hash(h);
            c.function.hash(h);
            let mut names: Vec<&String> = c.args.keys().collect();
            names.sort();
            names.hash(h);
        }
        Op::Batch(ops) => {
            5u8.hash(h);
            ops.len().hash(h);
            for op in ops {
                hash_op(op, h);
            }
        }
    }
}

fn hash_returning(r: Option<&Returning>, h: &mut DefaultHasher) {
    match r {
        None => 0u8.hash(h),
        Some(Returning::All) => 1u8.hash(h),
        Some(Returning::Columns(cols)) => (2u8, cols).hash(h),
    }
}

fn hash_filter_opt(f: Option<&Filter>, h: &mut DefaultHasher) {
    match f {
        None => 0u8.hash(h),
        Some(f) => {
            1u8.hash(h);
            hash_filter(f, h);
        }
    }
}

fn hash_filter(f: &Filter, h: &mut DefaultHasher) {
    match f {
        Filter::And(fs) | Filter::Or(fs) => {
            (if matches!(f, Filter::And(_)) {
                0u8
            } else {
                1u8
            })
            .hash(h);
            fs.len().hash(h);
            for f in fs {
                hash_filter(f, h);
            }
        }
        Filter::Not(f) => {
            2u8.hash(h);
            hash_filter(f, h);
        }
        Filter::Cond { col, op, value } => {
            (3u8, col, op).hash(h);
            // `is` writes its constant into the SQL (IS NULL / IS TRUE / IS FALSE).
            if *op == crate::query::CondOp::Is {
                value.as_bool().hash(h);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hash is keyed once per process: stable across calls, but not the unkeyed
    /// `DefaultHasher::new()` value an attacker could precompute collisions for.
    #[test]
    fn keyed_once_per_process() {
        let op = crate::query::parse(br#"{"v":1,"op":"select","from":"t"}"#).unwrap();
        assert_eq!(shape_hash(&op), shape_hash(&op));
        let mut unkeyed = DefaultHasher::new();
        hash_op(&op, &mut unkeyed);
        assert_ne!(shape_hash(&op), unkeyed.finish());
    }
}
