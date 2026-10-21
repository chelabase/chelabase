//! The change source (plan "Socket protocol" pin, S16): T7 ships the trait and a fake; T8
//! implements it (leases, slots, the decoder, per-role filtering). The sink is per (socket,
//! channel); T7 assigns `seq`. Dropping the [`WatchHandle`] unwatches. Production uses
//! [`crate::replication::ReplicationSource`]; the fake is for tests only.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::channel::TableChannel;
use crate::protocol::{Change, StatusReason};

/// Who a table is checked as: `role: None` is the owner (whose checks go through the
/// orchestrator with `token`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckIdentity {
    pub role: Option<String>,
    pub token: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinInfo {
    pub pk: Option<Vec<String>>,
    pub live: bool,
    pub reason: Option<StatusReason>,
    pub db_oid: u32,
    pub rel_oid: u32,
}

/// Maps to the `error` codes `forbidden`, `not_found`, `unavailable`, `invalid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinRefusal {
    Forbidden,
    NotFound,
    Unavailable,
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelEvent {
    Rows(Vec<Change>),
    Status {
        live: bool,
        reason: Option<StatusReason>,
    },
    Resync {
        reason: String,
    },
}

/// A watch; dropping it unwatches (runs the source's hook once).
pub struct WatchHandle {
    id: u64,
    on_drop: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl WatchHandle {
    pub fn new(id: u64, on_drop: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self {
            id,
            on_drop: Some(Box::new(on_drop)),
        }
    }
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        if let Some(f) = self.on_drop.take() {
            f();
        }
    }
}

impl std::fmt::Debug for WatchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchHandle").finish_non_exhaustive()
    }
}

#[async_trait]
pub trait ChangeSource: Send + Sync + 'static {
    async fn watch(
        &self,
        ch: &TableChannel,
        id: &CheckIdentity,
        sink: mpsc::Sender<ChannelEvent>,
    ) -> Result<(WatchHandle, JoinInfo), JoinRefusal>;
    /// R19: the keys on the socket's screen. Only keys the handle's check identity is confirmed
    /// to see (one visibility check per frame, through the R12 cache) enter its view set; a
    /// guessed key never earns a keyed delete.
    async fn view(&self, h: &WatchHandle, keys: Vec<String>);
    /// The socket's verified `auth` renewal (T7 calls it for every joined table channel): the
    /// owner path's next `/visible` call carries `token` (`CheckIdentity.token` is the join's).
    async fn renew(&self, h: &WatchHandle, token: Arc<str>);
    /// R15: whether the handle's check identity sees each key; `Err` = treat as not visible.
    async fn visible(&self, h: &WatchHandle, keys: &[String]) -> Result<Vec<bool>, ()>;
    /// S6: the privilege probe again (re-authorization, resync).
    async fn reprobe(&self, h: &WatchHandle) -> Result<JoinInfo, JoinRefusal>;
    /// SIGTERM (T8): close replication connections, release leases and slot indexes.
    async fn shutdown(&self) {}
    /// A crash (tests): stop at once, releasing nothing.
    fn abort(&self) {}
}

pub mod fake {
    //! A change source for tests, keyed by table name. By default a join answers
    //! [`FakeChangeSource::default_info`] and every key is visible to every role.

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio::sync::mpsc;

    use super::{ChangeSource, ChannelEvent, CheckIdentity, JoinInfo, JoinRefusal, WatchHandle};
    use crate::channel::TableChannel;

    struct Watch {
        table: String,
        role: Option<String>,
        sink: mpsc::Sender<ChannelEvent>,
    }

    #[derive(Default)]
    struct State {
        joins: HashMap<String, Result<JoinInfo, JoinRefusal>>,
        visible: HashMap<(Option<String>, String), bool>,
        fail_visible: bool,
        watches: HashMap<u64, Watch>,
        next_id: u64,
        last_view: HashMap<String, Vec<String>>,
        visible_calls: usize,
        reprobes: usize,
        /// `renew` calls: (table, token).
        renewals: Vec<(String, String)>,
        /// `view` calls, per table.
        view_calls: HashMap<String, usize>,
        /// Each `view` takes this long (a slow check).
        view_delay: Option<std::time::Duration>,
        /// `shutdown` never returns (a source stuck releasing its leases).
        hang_shutdown: bool,
    }

    #[derive(Default)]
    pub struct FakeChangeSource {
        state: Arc<Mutex<State>>,
    }

    impl FakeChangeSource {
        pub fn new() -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self::default())
        }
        /// `{pk: ["id"], live: true, reason: None, db_oid: 16384, rel_oid: 16400}`.
        pub fn default_info() -> JoinInfo {
            JoinInfo {
                pk: Some(vec!["id".into()]),
                live: true,
                reason: None,
                db_oid: 16384,
                rel_oid: 16400,
            }
        }
        /// What a join of `table` (and a reprobe of its watches) answers from now on.
        pub fn set_join(&self, table: &str, result: Result<JoinInfo, JoinRefusal>) {
            self.state
                .lock()
                .unwrap()
                .joins
                .insert(table.to_string(), result);
        }
        /// Whether `role` (`None` = the owner) sees `key`.
        pub fn set_visible(&self, role: Option<&str>, key: &str, visible: bool) {
            self.state
                .lock()
                .unwrap()
                .visible
                .insert((role.map(str::to_string), key.to_string()), visible);
        }
        /// While on, `visible` answers `Err`.
        pub fn fail_visible(&self, on: bool) {
            self.state.lock().unwrap().fail_visible = on;
        }
        /// Sends `event` to every live watch of `table`; how many got it.
        pub async fn emit(&self, table: &str, event: ChannelEvent) -> usize {
            let sinks: Vec<mpsc::Sender<ChannelEvent>> = self
                .state
                .lock()
                .unwrap()
                .watches
                .values()
                .filter(|w| w.table == table)
                .map(|w| w.sink.clone())
                .collect();
            let mut sent = 0;
            for sink in sinks {
                if sink.send(event.clone()).await.is_ok() {
                    sent += 1;
                }
            }
            sent
        }
        /// Live (not dropped) watches of `table`.
        pub fn watchers(&self, table: &str) -> usize {
            self.state
                .lock()
                .unwrap()
                .watches
                .values()
                .filter(|w| w.table == table)
                .count()
        }
        /// The last `view` keys any watch of `table` sent.
        pub fn last_view(&self, table: &str) -> Option<Vec<String>> {
            self.state.lock().unwrap().last_view.get(table).cloned()
        }
        /// Calls of `visible` so far.
        pub fn visible_calls(&self) -> usize {
            self.state.lock().unwrap().visible_calls
        }
        /// Calls of `reprobe` so far.
        pub fn reprobes(&self) -> usize {
            self.state.lock().unwrap().reprobes
        }
        /// How many `view` calls watches of `table` made.
        pub fn view_calls(&self, table: &str) -> usize {
            self.state
                .lock()
                .unwrap()
                .view_calls
                .get(table)
                .copied()
                .unwrap_or(0)
        }
        /// From now on each `view` takes `delay` before it records its keys.
        pub fn set_view_delay(&self, delay: Option<std::time::Duration>) {
            self.state.lock().unwrap().view_delay = delay;
        }
        /// While on, `shutdown` never returns.
        pub fn hang_shutdown(&self, on: bool) {
            self.state.lock().unwrap().hang_shutdown = on;
        }
        /// The tokens `renew` passed for watches of `table`, in order.
        pub fn renewals(&self, table: &str) -> Vec<String> {
            self.state
                .lock()
                .unwrap()
                .renewals
                .iter()
                .filter(|(t, _)| t == table)
                .map(|(_, tok)| tok.clone())
                .collect()
        }
    }

    fn join_answer(state: &State, table: &str) -> Result<JoinInfo, JoinRefusal> {
        state
            .joins
            .get(table)
            .cloned()
            .unwrap_or_else(|| Ok(FakeChangeSource::default_info()))
    }

    #[async_trait]
    impl ChangeSource for FakeChangeSource {
        async fn watch(
            &self,
            ch: &TableChannel,
            id: &CheckIdentity,
            sink: mpsc::Sender<ChannelEvent>,
        ) -> Result<(WatchHandle, JoinInfo), JoinRefusal> {
            let mut state = self.state.lock().unwrap();
            let info = join_answer(&state, &ch.table)?;
            state.next_id += 1;
            let watch_id = state.next_id;
            state.watches.insert(
                watch_id,
                Watch {
                    table: ch.table.clone(),
                    role: id.role.clone(),
                    sink,
                },
            );
            let shared = self.state.clone();
            let handle = WatchHandle::new(watch_id, move || {
                if let Ok(mut s) = shared.lock() {
                    s.watches.remove(&watch_id);
                }
            });
            Ok((handle, info))
        }
        async fn view(&self, h: &WatchHandle, keys: Vec<String>) {
            let delay = {
                let mut state = self.state.lock().unwrap();
                if let Some(table) = state.watches.get(&h.id()).map(|w| w.table.clone()) {
                    *state.view_calls.entry(table).or_default() += 1;
                }
                state.view_delay
            };
            if let Some(d) = delay {
                tokio::time::sleep(d).await;
            }
            let mut state = self.state.lock().unwrap();
            if let Some(table) = state.watches.get(&h.id()).map(|w| w.table.clone()) {
                state.last_view.insert(table, keys);
            }
        }
        async fn shutdown(&self) {
            let hang = self.state.lock().unwrap().hang_shutdown;
            if hang {
                std::future::pending::<()>().await;
            }
        }
        async fn renew(&self, h: &WatchHandle, token: Arc<str>) {
            let mut state = self.state.lock().unwrap();
            if let Some(table) = state.watches.get(&h.id()).map(|w| w.table.clone()) {
                state.renewals.push((table, token.to_string()));
            }
        }
        async fn visible(&self, h: &WatchHandle, keys: &[String]) -> Result<Vec<bool>, ()> {
            let mut state = self.state.lock().unwrap();
            state.visible_calls += 1;
            if state.fail_visible {
                return Err(());
            }
            let role = state.watches.get(&h.id()).ok_or(())?.role.clone();
            Ok(keys
                .iter()
                .map(|k| {
                    state
                        .visible
                        .get(&(role.clone(), k.clone()))
                        .copied()
                        .unwrap_or(true)
                })
                .collect())
        }
        async fn reprobe(&self, h: &WatchHandle) -> Result<JoinInfo, JoinRefusal> {
            let mut state = self.state.lock().unwrap();
            state.reprobes += 1;
            let table = state
                .watches
                .get(&h.id())
                .map(|w| w.table.clone())
                .ok_or(JoinRefusal::NotFound)?;
            join_answer(&state, &table)
        }
    }
}
