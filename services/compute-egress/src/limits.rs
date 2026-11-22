//! Connection limits: a total, and a share per source address, so one client can't take every
//! slot. A slot is held by a [`Permit`] and freed when it drops.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub struct Limits {
    total: usize,
    per_client: usize,
    used: Mutex<(usize, HashMap<IpAddr, usize>)>,
}

/// One held slot.
#[derive(Debug)]
pub struct Permit {
    limits: Arc<Limits>,
    client: IpAddr,
}

impl Limits {
    pub fn new(total: usize, per_client: usize) -> Arc<Self> {
        Arc::new(Limits {
            total,
            per_client,
            used: Mutex::new((0, HashMap::new())),
        })
    }

    /// A slot for `client`, or `None` when the total or the client's share is used up.
    pub fn try_acquire(self: &Arc<Self>, client: IpAddr) -> Option<Permit> {
        let mut used = self.lock();
        let (total, per) = &mut *used;
        let mine = per.entry(client).or_insert(0);
        if *total >= self.total || *mine >= self.per_client {
            if *mine == 0 {
                per.remove(&client);
            }
            return None;
        }
        *mine += 1;
        *total += 1;
        Some(Permit {
            limits: self.clone(),
            client,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (usize, HashMap<IpAddr, usize>)> {
        // The counts stay consistent across a panic elsewhere: every update is complete
        // before the guard drops.
        self.used.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Slots in use.
    pub fn in_use(&self) -> usize {
        self.lock().0
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut used = self.limits.lock();
        let (total, per) = &mut *used;
        *total = total.saturating_sub(1);
        if let Some(mine) = per.get_mut(&self.client) {
            *mine = mine.saturating_sub(1);
            if *mine == 0 {
                per.remove(&self.client);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_total_bounds_every_client() {
        let l = Limits::new(2, 2);
        let a = l.try_acquire(ip("10.0.0.1")).unwrap();
        let _b = l.try_acquire(ip("10.0.0.2")).unwrap();
        assert!(l.try_acquire(ip("10.0.0.3")).is_none());
        drop(a);
        assert!(l.try_acquire(ip("10.0.0.3")).is_some());
    }

    #[test]
    fn one_client_has_its_own_share() {
        let l = Limits::new(10, 2);
        let held: Vec<_> = (0..2)
            .map(|_| l.try_acquire(ip("10.0.0.1")).unwrap())
            .collect();
        assert!(l.try_acquire(ip("10.0.0.1")).is_none());
        assert!(l.try_acquire(ip("10.0.0.2")).is_some());
        drop(held);
        assert_eq!(l.in_use(), 0);
        assert!(l.try_acquire(ip("10.0.0.1")).is_some());
    }

    #[test]
    fn a_refused_attempt_takes_no_slot() {
        let l = Limits::new(1, 1);
        let _a = l.try_acquire(ip("10.0.0.1")).unwrap();
        for _ in 0..5 {
            assert!(l.try_acquire(ip("10.0.0.1")).is_none());
            assert!(l.try_acquire(ip("10.0.0.2")).is_none());
        }
        assert_eq!(l.in_use(), 1);
    }
}
