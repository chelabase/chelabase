//! The stack registry: `<main>/.chelabase/stacks/registry.json`, every read-modify-write under an
//! exclusive `flock` on `registry.lock` (W2 spec section 1).

use super::slug::MAIN;
use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

pub const MAIN_GATEWAY_PORT: u16 = 8000;
pub const MAIN_PROXY_PORT: u16 = 4432;
pub const GATEWAY_PORTS: RangeInclusive<u16> = 8100..=8199;
pub const PROXY_PORTS: RangeInclusive<u16> = 4500..=4599;

const REGISTRY_FILE: &str = "registry.json";
const LOCK_FILE: &str = "registry.lock";
const TEMP_FILE: &str = "registry.json.tmp";

/// One stack: its checkout, its ports (kept until purge) and whether it holds a slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub slug: String,
    pub checkout: PathBuf,
    pub gateway_port: u16,
    pub proxy_port: u16,
    /// Unix seconds.
    pub created_at: u64,
    /// Unix seconds since the stack took its slot; `None` when stopped.
    pub running_since: Option<u64>,
    /// The stack's control-network slot `k` (0..=63): its subnet is `10.213.{4k}.0/22`. Kept until
    /// purge; entries from before the control network get one on the next `ensure`.
    #[serde(default)]
    pub control_slot: Option<u8>,
}

/// The control-network slots (`main` takes 0).
pub const CONTROL_SLOTS: RangeInclusive<u8> = 0..=63;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    pub stacks: Vec<Entry>,
}

impl Registry {
    pub fn get(&self, slug: &str) -> Option<&Entry> {
        self.stacks.iter().find(|e| e.slug == slug)
    }

    pub fn get_mut(&mut self, slug: &str) -> Option<&mut Entry> {
        self.stacks.iter_mut().find(|e| e.slug == slug)
    }

    /// The entry for `slug`, registering it first if it is new. A known entry comes back unchanged
    /// (its ports aren't probed: the stack may be running on them). A known entry recorded for
    /// another checkout is an error while that checkout still exists (two checkouts never share a
    /// stack); once it is gone, the new checkout takes the stack over (same ports, same data). New
    /// `main` gets the fixed ports; a new branch stack takes the lowest gateway and,
    /// independently, the lowest proxy port that no entry uses and `free` accepts.
    pub fn ensure(
        &mut self,
        slug: &str,
        checkout: &Path,
        now: u64,
        free: impl Fn(u16) -> bool,
    ) -> anyhow::Result<Entry> {
        if self.get(slug).is_some_and(|e| e.control_slot.is_none()) {
            let slot = self.pick_slot(slug)?;
            self.get_mut(slug).expect("checked above").control_slot = Some(slot);
        }
        if let Some(e) = self.get_mut(slug) {
            if e.checkout != checkout {
                if e.checkout.exists() {
                    anyhow::bail!(
                        "stack {slug} belongs to {}, so {} can't use it: two checkouts whose names \
                         map to one slug can't share a stack; rename one of them",
                        e.checkout.display(),
                        checkout.display(),
                    );
                }
                e.checkout = checkout.to_path_buf();
            }
            return Ok(e.clone());
        }
        let (gateway_port, proxy_port) = if slug == MAIN {
            (MAIN_GATEWAY_PORT, MAIN_PROXY_PORT)
        } else {
            let gateway = self.pick(GATEWAY_PORTS, "gateway", |e| e.gateway_port, &free)?;
            let proxy = self.pick(PROXY_PORTS, "proxy", |e| e.proxy_port, &free)?;
            (gateway, proxy)
        };
        let control_slot = Some(self.pick_slot(slug)?);
        let entry = Entry {
            slug: slug.to_string(),
            checkout: checkout.to_path_buf(),
            gateway_port,
            proxy_port,
            created_at: now,
            running_since: None,
            control_slot,
        };
        self.stacks.push(entry.clone());
        Ok(entry)
    }

    /// `main` gets slot 0 when it is free; any other stack the lowest slot no entry holds.
    fn pick_slot(&self, slug: &str) -> anyhow::Result<u8> {
        let taken = |slot: u8| self.stacks.iter().any(|e| e.control_slot == Some(slot));
        let mut slots = CONTROL_SLOTS.filter(|s| !taken(*s));
        let first = if slug == MAIN {
            slots.next()
        } else {
            // Keep 0 for `main` while any other slot is left.
            slots.find(|s| *s != 0).or_else(|| (!taken(0)).then_some(0))
        };
        first.with_context(|| {
            "no free control-network slot left (64 stacks): purge a stack you no longer need \
             (`just workbench purge --stack <slug>`)"
        })
    }

    fn pick(
        &self,
        range: RangeInclusive<u16>,
        what: &str,
        port_of: impl Fn(&Entry) -> u16,
        free: &impl Fn(u16) -> bool,
    ) -> anyhow::Result<u16> {
        let (lo, hi) = (*range.start(), *range.end());
        range
            .into_iter()
            .find(|p| !self.stacks.iter().any(|e| port_of(e) == *p) && free(*p))
            .with_context(|| {
                format!(
                    "no free {what} port left in {lo}–{hi}: purge a stack you no longer need \
                     (`just workbench purge --stack <slug>`)"
                )
            })
    }

    pub fn remove(&mut self, slug: &str) -> Option<Entry> {
        let i = self.stacks.iter().position(|e| e.slug == slug)?;
        Some(self.stacks.remove(i))
    }

    /// The stacks holding a slot, in registry order.
    pub fn running(&self) -> impl Iterator<Item = &Entry> {
        self.stacks.iter().filter(|e| e.running_since.is_some())
    }
}

/// Runs `f` on the registry in `stacks_dir` under an exclusive lock on `registry.lock`, held for
/// the whole read-modify-write. A missing `registry.json` is an empty registry; one that doesn't
/// parse is an error (never silently reset). The registry is written back (temp file, then
/// rename) only when `f` returns `Ok`.
pub fn with_registry<T>(
    stacks_dir: &Path,
    f: impl FnOnce(&mut Registry) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    std::fs::create_dir_all(stacks_dir)
        .with_context(|| format!("creating {}", stacks_dir.display()))?;
    let lock_path = stacks_dir.join(LOCK_FILE);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("locking {}", lock_path.display()))?;

    let path = stacks_dir.join(REGISTRY_FILE);
    let original = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Registry>(&text)
            .with_context(|| format!("{} is not a valid stack registry", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Registry::default(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut registry = original.clone();
    let out = f(&mut registry)?;
    if registry != original || !path.exists() {
        write_atomically(stacks_dir, &path, &registry)?;
    }
    // The lock is released when `lock` is dropped (closing the file).
    drop(lock);
    Ok(out)
}

fn write_atomically(stacks_dir: &Path, path: &Path, registry: &Registry) -> anyhow::Result<()> {
    let temp = stacks_dir.join(TEMP_FILE);
    let mut text = serde_json::to_string_pretty(registry).context("serializing the registry")?;
    text.push('\n');
    let mut file = File::create(&temp).with_context(|| format!("creating {}", temp.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, path)
        .with_context(|| format!("renaming {} to {}", temp.display(), path.display()))?;
    // The rename is durable only once the directory entry is on disk.
    File::open(stacks_dir)
        .and_then(|dir| dir.sync_all())
        .with_context(|| format!("syncing {}", stacks_dir.display()))
}

/// Whether nothing listens on `port` on the host: binds `127.0.0.1:port` and `[::1]:port`. A host
/// without IPv6 loopback can't have a listener on `[::1]`, so that address being unavailable
/// counts as free.
pub fn port_is_free(port: u16) -> bool {
    let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok();
    let v6 = match TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
        Ok(_) => true,
        Err(e) => e.kind() == std::io::ErrorKind::AddrNotAvailable,
    };
    v4 && v6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_takes_the_entry_out() {
        let mut r = Registry::default();
        r.ensure("a", Path::new("/w/a"), 1, |_| true).unwrap();
        r.ensure("b", Path::new("/w/b"), 1, |_| true).unwrap();
        assert_eq!(r.remove("a").map(|e| e.gateway_port), Some(8100));
        assert_eq!(r.remove("a"), None);
        assert_eq!(r.stacks.len(), 1);
        let c = r.ensure("c", Path::new("/w/c"), 1, |_| true).unwrap();
        assert_eq!(c.gateway_port, 8100, "a purged stack's port is reused");
    }

    #[test]
    fn a_listened_port_is_not_free() {
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        assert!(!port_is_free(port));
        drop(held);
    }
}
