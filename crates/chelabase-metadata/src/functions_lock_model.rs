//! A model of the per-function schedule and trigger caps' locking ([`MetadataClient::add_schedule`],
//! [`MetadataClient::add_trigger`]), checked over every interleaving of the contending
//! writes.
//!
//! **Invariant (caps).** A function never has more than `CAP` schedules (or triggers) on a branch,
//! whatever the interleaving of concurrent writes; no interleaving deadlocks; and no write is
//! lost: the rows left are the starting rows plus the adds that succeeded minus the deletes.
//!
//! **Lock order (the code's, `functions.rs` module docs).** A writer takes, in this order: the
//! function row (`FOR KEY SHARE`; a kind change or function delete takes `FOR UPDATE`), then for
//! an add the transaction advisory lock `fn-schedules:` / `fn-triggers:{branch}:{function}`
//! (exclusive, held to commit), then only the count and the insert of the new row; a schedule
//! update or delete takes the function row and then its schedule row `FOR UPDATE`. Nothing
//! takes a lower lock while holding a higher one, so no wait cycle can form.
//!
//! **What the model assumes** (each checked against Postgres by
//! `tests/functions_live.rs::caps_hold_under_concurrent_adds` and
//! `function_writes_keep_the_lock_order`): a `FOR KEY SHARE` row lock doesn't conflict with
//! another `FOR KEY SHARE` and does with `FOR UPDATE`; an advisory transaction lock is exclusive
//! and held to commit; the count after the lock is a fresh `READ COMMITTED` snapshot, so it
//! sees every row an earlier holder committed; an insert of a new row waits for no lock
//! another writer holds here; and an uncommitted insert is invisible to others' counts. The
//! model's own check: with the advisory step removed it finds the cap broken, so the model can
//! see a lost cap; and its scenarios include a kind change and a delete beside the adds.

use std::collections::HashSet;

const CAP: i32 = 3;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Lock {
    FunctionShare,
    FunctionUpdate,
    Advisory,
    ScheduleRow,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Step {
    Lock(Lock),
    ReadCount,
    Insert,
    Delete,
    Commit,
}

fn conflicts(held: Lock, wanted: Lock) -> bool {
    use Lock::*;
    match (held, wanted) {
        (FunctionShare, FunctionShare) => false,
        (FunctionShare, FunctionUpdate) | (FunctionUpdate, FunctionShare | FunctionUpdate) => true,
        (Advisory, Advisory) | (ScheduleRow, ScheduleRow) => true,
        _ => false,
    }
}

fn add(with_advisory: bool) -> Vec<Step> {
    let mut s = vec![Step::Lock(Lock::FunctionShare)];
    if with_advisory {
        s.push(Step::Lock(Lock::Advisory));
    }
    s.extend([Step::ReadCount, Step::Insert, Step::Commit]);
    s
}

fn kind_change() -> Vec<Step> {
    vec![Step::Lock(Lock::FunctionUpdate), Step::Commit]
}

fn delete() -> Vec<Step> {
    vec![
        Step::Lock(Lock::FunctionShare),
        Step::Lock(Lock::ScheduleRow),
        Step::Delete,
        Step::Commit,
    ]
}

#[derive(Clone)]
struct World {
    programs: Vec<Vec<Step>>,
    pc: Vec<usize>,
    held: Vec<Vec<Lock>>,
    seen: Vec<i32>,
    inserted: Vec<bool>,
    deleted: Vec<bool>,
    refused: Vec<bool>,
    committed: i32,
}

#[derive(Default, Debug)]
struct Outcome {
    deadlocks: usize,
    cap_broken: usize,
    lost_update: usize,
    wrong_refusal: usize,
    runs: usize,
}

impl World {
    fn new(programs: Vec<Vec<Step>>, start: i32) -> Self {
        let n = programs.len();
        World {
            programs,
            pc: vec![0; n],
            held: vec![vec![]; n],
            seen: vec![0; n],
            inserted: vec![false; n],
            deleted: vec![false; n],
            refused: vec![false; n],
            committed: start,
        }
    }

    fn done(&self, t: usize) -> bool {
        self.pc[t] == self.programs[t].len()
    }

    /// Whether thread `t`'s next step can run now (a lock another thread holds blocks it).
    fn runnable(&self, t: usize) -> bool {
        match self.programs[t].get(self.pc[t]) {
            None => false,
            Some(Step::Lock(l)) => !self
                .held
                .iter()
                .enumerate()
                .any(|(o, h)| o != t && h.iter().any(|&x| conflicts(x, *l))),
            Some(_) => true,
        }
    }

    fn step(&mut self, t: usize) {
        let step = self.programs[t][self.pc[t]];
        self.pc[t] += 1;
        match step {
            Step::Lock(l) => self.held[t].push(l),
            Step::ReadCount => self.seen[t] = self.committed,
            Step::Insert => {
                if self.seen[t] >= CAP {
                    // Refused: the transaction ends without writing.
                    self.refused[t] = true;
                    self.pc[t] = self.programs[t].len();
                    self.held[t].clear();
                } else {
                    self.inserted[t] = true;
                }
            }
            Step::Delete => self.deleted[t] = true,
            Step::Commit => {
                self.committed += i32::from(self.inserted[t]) - i32::from(self.deleted[t]);
                self.held[t].clear();
            }
        }
    }
}

fn explore(w: &World, start: i32, out: &mut Outcome) {
    let n = w.programs.len();
    if (0..n).all(|t| w.done(t)) {
        out.runs += 1;
        let (adds, dels) = (
            w.inserted.iter().filter(|&&b| b).count() as i32,
            w.deleted.iter().filter(|&&b| b).count() as i32,
        );
        if w.committed > CAP {
            out.cap_broken += 1;
        }
        if w.committed != start + adds - dels {
            out.lost_update += 1;
        }
        // A refusal needs a full function as the add read it.
        if (0..n).any(|t| w.refused[t] && w.seen[t] < CAP) {
            out.wrong_refusal += 1;
        }
        return;
    }
    let runnable: Vec<usize> = (0..n).filter(|&t| w.runnable(t)).collect();
    if runnable.is_empty() {
        out.deadlocks += 1;
        return;
    }
    for t in runnable {
        let mut next = w.clone();
        next.step(t);
        explore(&next, start, out);
    }
}

fn check(programs: Vec<Vec<Step>>, start: i32) -> Outcome {
    let mut out = Outcome::default();
    explore(&World::new(programs, start), start, &mut out);
    out
}

fn sound(o: &Outcome) -> bool {
    o.deadlocks + o.cap_broken + o.lost_update + o.wrong_refusal == 0 && o.runs > 0
}

/// Every interleaving of the contending writes, from each starting count at and below the cap,
/// keeps the cap, loses nothing and never deadlocks.
#[test]
fn caps_hold_over_every_interleaving() {
    let scenarios: Vec<(&str, Vec<Vec<Step>>)> = vec![
        ("three adds", vec![add(true), add(true), add(true)]),
        (
            "two adds and a kind change",
            vec![add(true), add(true), kind_change()],
        ),
        (
            "two adds and a delete",
            vec![add(true), add(true), delete()],
        ),
        (
            "an add, a delete and a kind change",
            vec![add(true), delete(), kind_change()],
        ),
        (
            "two adds, a delete and a kind change",
            vec![add(true), add(true), delete(), kind_change()],
        ),
    ];
    let mut total = 0;
    for (name, programs) in scenarios {
        let deletes = programs.iter().filter(|p| **p == delete()).count() as i32;
        for start in deletes.max(0)..=CAP {
            let o = check(programs.clone(), start);
            assert!(sound(&o), "{name} from {start}: {o:?}");
            total += o.runs;
        }
    }
    assert!(
        total > 10_000,
        "the enumeration is real: {total} interleavings"
    );
}

/// Without the advisory lock two adds at one below the cap both pass it: the model sees the
/// cap broken, so a pass above means the lock does the work.
#[test]
fn without_the_advisory_lock_the_cap_breaks() {
    let o = check(vec![add(false), add(false)], CAP - 1);
    assert!(o.cap_broken > 0, "{o:?}");
    let o = check(vec![add(true), add(true)], CAP - 1);
    assert!(sound(&o), "{o:?}");
}

/// Exactly the room left is filled: with no deletes, `n` adds from `start` succeed `min(n,
/// CAP - start)` times, in every interleaving.
#[test]
fn exactly_the_room_left_is_filled() {
    fn all_ends(programs: &[Vec<Step>], start: i32, outcomes: &mut HashSet<i32>) {
        fn go(w: &World, outcomes: &mut HashSet<i32>) {
            let n = w.programs.len();
            if (0..n).all(|t| w.done(t)) {
                outcomes.insert(w.committed);
                return;
            }
            for t in (0..n).filter(|&t| w.runnable(t)) {
                let mut next = w.clone();
                next.step(t);
                go(&next, outcomes);
            }
        }
        go(&World::new(programs.to_vec(), start), outcomes);
    }
    for start in 0..=CAP {
        let mut ends = HashSet::new();
        all_ends(&[add(true), add(true), add(true)], start, &mut ends);
        assert_eq!(ends, HashSet::from([(start + 3).min(CAP)]), "from {start}");
    }
}
