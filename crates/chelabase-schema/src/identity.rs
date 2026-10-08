//! Sides, directions and the identity map.
//!
//! Objects that existed at the base are matched by catalog class and OID (columns: table OID +
//! `attnum`; enum values: their `pg_enum` OID). Each side allocates OIDs independently after the
//! branch point (from the same counter, so a new object on one side can get the same OID as an
//! unrelated one on the other), so OIDs are never assumed to agree. The map is **full** (ruling
//! E6): every base write records a pair for every base object, with the object's OID on each side,
//! or `None` where it doesn't exist. A lookup with no pair means "no base identity": the object is
//! newer than the base and is matched by name.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::model::{ObjClass, Oid, SchemaModel};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Parent,
    Branch,
}

impl Side {
    pub fn other(self) -> Side {
        match self {
            Side::Parent => Side::Branch,
            Side::Branch => Side::Parent,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Merge the branch's changes into its parent: source = branch, target = parent.
    IntoParent,
    /// Update the branch from its parent: source = parent, target = branch.
    FromParent,
}

impl Direction {
    pub fn source_side(self) -> Side {
        match self {
            Direction::IntoParent => Side::Branch,
            Direction::FromParent => Side::Parent,
        }
    }

    pub fn target_side(self) -> Side {
        self.source_side().other()
    }
}

/// An object as one model names it: its OID, and for a column its table's OID plus `attnum`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjAt {
    pub oid: Oid,
    /// A column's `attnum`; `None` for every other class.
    pub sub: Option<i16>,
}

impl ObjAt {
    pub fn new(oid: Oid, sub: Option<i16>) -> Self {
        ObjAt { oid, sub }
    }
}

/// One base object: as the base model names it, and as each side names it now (`None` where the
/// object doesn't exist on that side, e.g. after a keep-target resolution).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityPair {
    pub class: ObjClass,
    pub base: ObjAt,
    pub parent: Option<ObjAt>,
    pub branch: Option<ObjAt>,
}

impl IdentityPair {
    /// The object as `side` names it.
    pub fn on(&self, side: Side) -> Option<ObjAt> {
        match side {
            Side::Parent => self.parent,
            Side::Branch => self.branch,
        }
    }

    fn key(&self) -> (ObjClass, ObjAt) {
        (self.class, self.base)
    }
}

/// The stored shape: `{"pairs": [...]}`.
#[derive(Serialize, Deserialize)]
struct StoredIdentityMap {
    pairs: Vec<IdentityPair>,
}

/// The full identity map of one base. The stored default `{"pairs":[]}` maps nothing: no object
/// has a base identity (the base is then read again or re-baselined).
///
/// Pairs are kept sorted by `(class, base)`, one per base object, so the stored JSON is
/// deterministic; `from_base` is a binary search and `to_base` a hash lookup in a per-side index
/// that isn't serialized (rebuilt on load).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(from = "StoredIdentityMap", into = "StoredIdentityMap")]
pub struct IdentityMap {
    pairs: Vec<IdentityPair>,
    /// Side object → the base objects that name it there. Normally one; two base objects
    /// claiming one side OID (a corrupt or hand-edited map) is ambiguous, and neither maps
    /// (T14 M4): on that side, both base objects then read as dropped and the side's object as
    /// newer than the base (matched by name), so the analysis sees a drop plus a create, the
    /// drop flagged as data loss, rather than silently following the wrong base object.
    parent_index: HashMap<(ObjClass, ObjAt), Vec<ObjAt>>,
    branch_index: HashMap<(ObjClass, ObjAt), Vec<ObjAt>>,
}

impl PartialEq for IdentityMap {
    fn eq(&self, other: &Self) -> bool {
        // The indexes are derived from the pairs.
        self.pairs == other.pairs
    }
}

impl Eq for IdentityMap {}

impl From<StoredIdentityMap> for IdentityMap {
    fn from(stored: StoredIdentityMap) -> Self {
        IdentityMap::from_pairs(stored.pairs)
    }
}

impl From<IdentityMap> for StoredIdentityMap {
    fn from(map: IdentityMap) -> Self {
        StoredIdentityMap { pairs: map.pairs }
    }
}

impl IdentityMap {
    /// A map from pairs in any order; for one `(class, base)` the last pair wins.
    pub fn from_pairs(pairs: impl IntoIterator<Item = IdentityPair>) -> Self {
        let mut pairs: Vec<IdentityPair> = pairs.into_iter().collect();
        // Stable, so equal keys keep their input order; then keep the last of each run.
        pairs.sort_by_key(IdentityPair::key);
        pairs.dedup_by(|later, kept| {
            if later.key() == kept.key() {
                std::mem::swap(later, kept);
                true
            } else {
                false
            }
        });
        let mut map = IdentityMap {
            pairs,
            ..IdentityMap::default()
        };
        for i in 0..map.pairs.len() {
            let pair = map.pairs[i].clone();
            map.index_pair(&pair);
        }
        map
    }

    fn index_pair(&mut self, pair: &IdentityPair) {
        if let Some(at) = pair.parent {
            self.parent_index
                .entry((pair.class, at))
                .or_default()
                .push(pair.base);
        }
        if let Some(at) = pair.branch {
            self.branch_index
                .entry((pair.class, at))
                .or_default()
                .push(pair.base);
        }
    }

    fn unindex_pair(&mut self, pair: &IdentityPair) {
        for (index, at) in [
            (&mut self.parent_index, pair.parent),
            (&mut self.branch_index, pair.branch),
        ] {
            if let Some(at) = at
                && let Some(bases) = index.get_mut(&(pair.class, at))
            {
                bases.retain(|b| *b != pair.base);
                if bases.is_empty() {
                    index.remove(&(pair.class, at));
                }
            }
        }
    }

    /// The pairs, sorted by `(class, base)`.
    pub fn pairs(&self) -> &[IdentityPair] {
        &self.pairs
    }

    /// Every object of `base` mapped to itself on both sides: the map of a base read at branch
    /// create or reset, when both sides have exactly the base's OIDs.
    pub fn self_mapped(base: &SchemaModel) -> Self {
        let mut objects: Vec<(ObjClass, ObjAt)> = Vec::new();
        let mut add = |class, oid, sub| objects.push((class, ObjAt::new(oid, sub)));
        for s in &base.schemas {
            add(ObjClass::Schema, s.oid, None);
        }
        for t in &base.tables {
            add(ObjClass::Table, t.oid, None);
            for c in &t.columns {
                add(ObjClass::Column, t.oid, Some(c.attnum));
            }
            for c in &t.constraints {
                add(ObjClass::Constraint, c.oid, None);
            }
            for i in &t.indexes {
                add(ObjClass::Index, i.oid, None);
            }
            for p in &t.policies {
                add(ObjClass::Policy, p.oid, None);
            }
        }
        for e in &base.enums {
            add(ObjClass::Enum, e.oid, None);
            for l in &e.labels {
                add(ObjClass::EnumLabel, l.oid, None);
            }
        }
        for s in &base.sequences {
            add(ObjClass::Sequence, s.oid, None);
        }
        for r in &base.roles {
            add(ObjClass::Role, r.oid, None);
        }
        IdentityMap::from_pairs(objects.into_iter().map(|(class, at)| IdentityPair {
            class,
            base: at,
            parent: Some(at),
            branch: Some(at),
        }))
    }

    fn index(&self, side: Side) -> &HashMap<(ObjClass, ObjAt), Vec<ObjAt>> {
        match side {
            Side::Parent => &self.parent_index,
            Side::Branch => &self.branch_index,
        }
    }

    /// The base identity of `side`'s object, or `None` when it has none (newer than the base)
    /// or when several base objects claim it (ambiguous: matched by name).
    pub fn to_base(
        &self,
        side: Side,
        class: ObjClass,
        oid: Oid,
        sub: Option<i16>,
    ) -> Option<(Oid, Option<i16>)> {
        self.index(side)
            .get(&(class, ObjAt::new(oid, sub)))
            .and_then(|bases| match bases.as_slice() {
                [base] => Some((base.oid, base.sub)),
                _ => None,
            })
    }

    /// The base object `(base_oid, sub)` as `side` names it, or `None` when it doesn't exist there
    /// (or isn't a base object).
    pub fn from_base(
        &self,
        side: Side,
        class: ObjClass,
        base_oid: Oid,
        sub: Option<i16>,
    ) -> Option<(Oid, Option<i16>)> {
        let key = (class, ObjAt::new(base_oid, sub));
        let i = self
            .pairs
            .binary_search_by_key(&key, IdentityPair::key)
            .ok()?;
        self.pairs[i].on(side).map(|at| (at.oid, at.sub))
    }

    /// Records one base object, replacing any pair for the same `(class, base)`; the pairs stay
    /// sorted.
    pub fn record(
        &mut self,
        class: ObjClass,
        base: ObjAt,
        parent: Option<ObjAt>,
        branch: Option<ObjAt>,
    ) {
        let pair = IdentityPair {
            class,
            base,
            parent,
            branch,
        };
        match self
            .pairs
            .binary_search_by_key(&pair.key(), IdentityPair::key)
        {
            Ok(i) => {
                let old = std::mem::replace(&mut self.pairs[i], pair.clone());
                self.unindex_pair(&old);
            }
            Err(i) => self.pairs.insert(i, pair.clone()),
        }
        self.index_pair(&pair);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::sample::full_model;

    fn at(oid: u32, sub: Option<i16>) -> ObjAt {
        ObjAt::new(Oid(oid), sub)
    }

    fn pair(
        class: ObjClass,
        base: ObjAt,
        parent: Option<ObjAt>,
        branch: Option<ObjAt>,
    ) -> IdentityPair {
        IdentityPair {
            class,
            base,
            parent,
            branch,
        }
    }

    #[test]
    fn direction_sides() {
        assert_eq!(Direction::IntoParent.source_side(), Side::Branch);
        assert_eq!(Direction::IntoParent.target_side(), Side::Parent);
        assert_eq!(Direction::FromParent.source_side(), Side::Parent);
        assert_eq!(Direction::FromParent.target_side(), Side::Branch);
    }

    /// The pairs after a merge with keep: the base holds the source's (branch's) OIDs.
    fn after_merge_pairs() -> Vec<IdentityPair> {
        vec![
            // Existed at the branch point: the same OID everywhere.
            pair(
                ObjClass::Table,
                at(100, None),
                Some(at(100, None)),
                Some(at(100, None)),
            ),
            // A table the merge created on the parent as 500 from the branch's 900.
            pair(
                ObjClass::Table,
                at(900, None),
                Some(at(500, None)),
                Some(at(900, None)),
            ),
            // A column the merge added to table 100: attnum 6 on the branch, 4 on the parent.
            pair(
                ObjClass::Column,
                at(100, Some(6)),
                Some(at(100, Some(4))),
                Some(at(100, Some(6))),
            ),
            // A branch table the parent kept out (keep target): no parent counterpart.
            pair(ObjClass::Table, at(901, None), None, Some(at(901, None))),
            // An enum value, by its own pg_enum OID.
            pair(
                ObjClass::EnumLabel,
                at(300, None),
                Some(at(310, None)),
                Some(at(300, None)),
            ),
        ]
    }

    fn after_merge() -> IdentityMap {
        let mut map = IdentityMap::default();
        for p in after_merge_pairs() {
            map.record(p.class, p.base, p.parent, p.branch);
        }
        map
    }

    #[test]
    fn identity_map_round_trips() {
        let map = after_merge();
        assert_eq!(map.pairs().len(), 5);
        for pair in map.pairs() {
            for side in [Side::Parent, Side::Branch] {
                let base = (pair.base.oid, pair.base.sub);
                match pair.on(side) {
                    Some(here) => {
                        assert_eq!(
                            map.from_base(side, pair.class, base.0, base.1),
                            Some((here.oid, here.sub))
                        );
                        assert_eq!(
                            map.to_base(side, pair.class, here.oid, here.sub),
                            Some(base)
                        );
                    }
                    None => assert_eq!(map.from_base(side, pair.class, base.0, base.1), None),
                }
            }
        }
        // Translations across sides.
        assert_eq!(
            map.to_base(Side::Parent, ObjClass::Table, Oid(500), None),
            Some((Oid(900), None))
        );
        assert_eq!(
            map.to_base(Side::Parent, ObjClass::Column, Oid(100), Some(4)),
            Some((Oid(100), Some(6)))
        );
        // Through JSON (the index isn't stored; it's rebuilt).
        let json = serde_json::to_string(&map).unwrap();
        assert!(json.starts_with(r#"{"pairs":["#), "{json}");
        let back: IdentityMap = serde_json::from_str(&json).unwrap();
        assert_eq!(back, map);
        assert_eq!(
            back.to_base(Side::Parent, ObjClass::Table, Oid(500), None),
            Some((Oid(900), None))
        );
    }

    #[test]
    fn unpaired_oid_has_no_base_identity() {
        let map = after_merge();
        // The parent's own new object that happens to get the branch's OID 900 (same counter).
        assert_eq!(
            map.to_base(Side::Parent, ObjClass::Table, Oid(900), None),
            None
        );
        // Nor 901, which the parent doesn't have.
        assert_eq!(
            map.to_base(Side::Parent, ObjClass::Table, Oid(901), None),
            None
        );
        // An unrecorded OID or column, and a pair of another class, give nothing.
        assert_eq!(
            map.to_base(Side::Branch, ObjClass::Table, Oid(42), None),
            None
        );
        assert_eq!(
            map.to_base(Side::Branch, ObjClass::Column, Oid(100), Some(1)),
            None
        );
        assert_eq!(
            map.to_base(Side::Branch, ObjClass::Index, Oid(100), None),
            None
        );
        assert_eq!(
            map.from_base(Side::Branch, ObjClass::Table, Oid(500), None),
            None
        );
        // The stored default maps nothing.
        let stored: IdentityMap = serde_json::from_str(r#"{"pairs":[]}"#).unwrap();
        assert_eq!(stored, IdentityMap::default());
        assert_eq!(
            stored.to_base(Side::Branch, ObjClass::Schema, Oid(7), None),
            None
        );
    }

    #[test]
    fn record_replaces_the_pair_for_the_same_base_object() {
        let mut map = after_merge();
        map.record(
            ObjClass::Table,
            at(900, None),
            Some(at(502, None)),
            Some(at(900, None)),
        );
        assert_eq!(
            map.pairs()
                .iter()
                .filter(|p| p.class == ObjClass::Table && p.base == at(900, None))
                .count(),
            1
        );
        assert_eq!(
            map.to_base(Side::Parent, ObjClass::Table, Oid(502), None),
            Some((Oid(900), None))
        );
        // The replaced pair's old side OID is gone from the index.
        assert_eq!(
            map.to_base(Side::Parent, ObjClass::Table, Oid(500), None),
            None
        );
    }

    #[test]
    fn pairs_stay_sorted_whatever_the_record_order() {
        let forward = after_merge();
        let mut backward = IdentityMap::default();
        for p in after_merge_pairs().into_iter().rev() {
            backward.record(p.class, p.base, p.parent, p.branch);
        }
        let bulk = IdentityMap::from_pairs(after_merge_pairs().into_iter().rev());
        for map in [&forward, &backward, &bulk] {
            let keys: Vec<_> = map.pairs().iter().map(|p| (p.class, p.base)).collect();
            let mut sorted = keys.clone();
            sorted.sort();
            assert_eq!(keys, sorted);
        }
        let json = serde_json::to_string(&forward).unwrap();
        assert_eq!(serde_json::to_string(&backward).unwrap(), json);
        assert_eq!(serde_json::to_string(&bulk).unwrap(), json);
        // A stored map in any order loads sorted.
        let shuffled = format!(
            r#"{{"pairs":{}}}"#,
            serde_json::to_string(&after_merge_pairs().into_iter().rev().collect::<Vec<_>>())
                .unwrap()
        );
        let loaded: IdentityMap = serde_json::from_str(&shuffled).unwrap();
        assert_eq!(serde_json::to_string(&loaded).unwrap(), json);
    }

    // T14 M4: two base objects claiming one side OID are ambiguous there: neither maps, on that
    // side only (both base objects read as dropped there, the side's object as new); recording a
    // fix restores the mapping.
    #[test]
    fn a_side_oid_claimed_twice_maps_to_no_base_object() {
        let mut map = IdentityMap::from_pairs([
            pair(ObjClass::Table, at(1, None), Some(at(1, None)), Some(at(100, None))),
            pair(ObjClass::Table, at(2, None), Some(at(2, None)), Some(at(100, None))),
        ]);
        assert_eq!(map.to_base(Side::Branch, ObjClass::Table, Oid(100), None), None);
        assert_eq!(
            map.to_base(Side::Parent, ObjClass::Table, Oid(2), None),
            Some((Oid(2), None))
        );
        // from_base is per base object, unaffected.
        assert_eq!(
            map.from_base(Side::Branch, ObjClass::Table, Oid(1), None),
            Some((Oid(100), None))
        );
        map.record(ObjClass::Table, at(2, None), Some(at(2, None)), Some(at(200, None)));
        assert_eq!(
            map.to_base(Side::Branch, ObjClass::Table, Oid(100), None),
            Some((Oid(1), None))
        );
        assert_eq!(
            map.to_base(Side::Branch, ObjClass::Table, Oid(200), None),
            Some((Oid(2), None))
        );
        // And record can create the ambiguity too.
        map.record(ObjClass::Table, at(3, None), None, Some(at(200, None)));
        assert_eq!(map.to_base(Side::Branch, ObjClass::Table, Oid(200), None), None);
        // A stored map round-trips with its ambiguity.
        let back: IdentityMap =
            serde_json::from_str(&serde_json::to_string(&map).unwrap()).unwrap();
        assert_eq!(back.to_base(Side::Branch, ObjClass::Table, Oid(200), None), None);
    }

    #[test]
    fn from_pairs_keeps_the_last_pair_per_base_object() {
        let map = IdentityMap::from_pairs([
            pair(
                ObjClass::Table,
                at(900, None),
                Some(at(500, None)),
                Some(at(900, None)),
            ),
            pair(
                ObjClass::Table,
                at(900, None),
                Some(at(502, None)),
                Some(at(900, None)),
            ),
        ]);
        assert_eq!(map.pairs().len(), 1);
        assert_eq!(
            map.from_base(Side::Parent, ObjClass::Table, Oid(900), None),
            Some((Oid(502), None))
        );
        assert_eq!(
            map.to_base(Side::Parent, ObjClass::Table, Oid(500), None),
            None
        );
    }

    #[test]
    fn self_mapped_covers_every_object_class_including_columns() {
        let model = full_model();
        let map = IdentityMap::self_mapped(&model);

        let mut expected: Vec<(ObjClass, ObjAt)> = Vec::new();
        for s in &model.schemas {
            expected.push((ObjClass::Schema, at(s.oid.0, None)));
        }
        for t in &model.tables {
            expected.push((ObjClass::Table, at(t.oid.0, None)));
            for c in &t.columns {
                expected.push((ObjClass::Column, at(t.oid.0, Some(c.attnum))));
            }
            for c in &t.constraints {
                expected.push((ObjClass::Constraint, at(c.oid.0, None)));
            }
            for i in &t.indexes {
                expected.push((ObjClass::Index, at(i.oid.0, None)));
            }
            for p in &t.policies {
                expected.push((ObjClass::Policy, at(p.oid.0, None)));
            }
        }
        for e in &model.enums {
            expected.push((ObjClass::Enum, at(e.oid.0, None)));
            for l in &e.labels {
                expected.push((ObjClass::EnumLabel, at(l.oid.0, None)));
            }
        }
        for s in &model.sequences {
            expected.push((ObjClass::Sequence, at(s.oid.0, None)));
        }
        for r in &model.roles {
            expected.push((ObjClass::Role, at(r.oid.0, None)));
        }
        // Every class appears.
        for class in [
            ObjClass::Schema,
            ObjClass::Table,
            ObjClass::Column,
            ObjClass::Constraint,
            ObjClass::Index,
            ObjClass::Enum,
            ObjClass::EnumLabel,
            ObjClass::Sequence,
            ObjClass::Policy,
            ObjClass::Role,
        ] {
            assert!(
                expected.iter().any(|(c, _)| *c == class),
                "{class:?} missing from the sample"
            );
        }

        assert_eq!(map.pairs().len(), expected.len());
        for (class, obj) in expected {
            for side in [Side::Parent, Side::Branch] {
                assert_eq!(
                    map.to_base(side, class, obj.oid, obj.sub),
                    Some((obj.oid, obj.sub)),
                    "{class:?} {obj:?}"
                );
                assert_eq!(
                    map.from_base(side, class, obj.oid, obj.sub),
                    Some((obj.oid, obj.sub))
                );
            }
        }
    }
}
