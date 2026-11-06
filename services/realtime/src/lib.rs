//! The realtime service (RT1 T7 core; T8 adds the change source): tickets, one WebSocket per
//! tab, `table:` and `project:` channels, re-authorization, presence across instances and
//! platform events. Spec `docs/superpowers/specs/2026-10-14-realtime-design.md` §1, §2, §5, §8
//! with R4, R9, R10, R15, R20–R22 and the RT1 plan's P3, P5, P11–P13, P16 and S6, S13–S18, S24.
//!
//! - [`config`]: settings from the environment, timings and limits.
//! - [`app`]: the two listeners and the [`app::Realtime`] handle; [`app::build`] wires the
//!   production dependencies, [`app::build_with`] takes them from the caller (tests).
//! - [`ticket`]: sealed single-use tickets in Valkey (P5, W15).
//! - [`protocol`]: every frame (S16, S17) and the close codes.
//! - [`channel`]: join targets, canonical channel ids and R4's channel hash.
//! - [`socket`]: the upgrade and one socket's session; [`outq`]: its outbound queue, bounded
//!   by frames and bytes.
//! - [`auth`]: member-token verification of `auth` frames (R9, S14).
//! - [`orchestrator`]: `/internal/realtime/*` (P13, S13, S14, S15).
//! - [`reauth`]: re-authorization outcomes, coalescing and the per-cycle cache (W19, W28, R15).
//! - [`presence`]: members across instances (§5.1, §5.2, R4, R15, R20).
//! - [`project`]: platform events, narrowed per database (R21, W34).
//! - [`limits`]: rates and the per-member socket count in Valkey (P16, S6, W33, W35).
//! - [`source`]: the [`source::ChangeSource`] T8 implements, and its fake.
//! - `hub` (private): one instance's presence for one channel and its bus traffic.
//! - [`bus`]: core NATS fan-out between instances (and an in-memory bus for tests).
//! - [`store`]: Valkey (and an in-memory store for tests).
//! - [`shutdown`]: SIGTERM → 1012 on every socket.

//!
//! T8's change source:
//!
//! - [`replication`]: the [`replication::ReplicationSource`] (watching, holding, filtering).
//! - [`decoder`]: the replication session's commands and the `chelabase_keys` messages.
//! - [`merge`]: the 100 ms merge window; [`schema`]: schema changes (W10);
//!   [`partition`]: relation kinds and partition fan-out (R16, W25).
//! - [`ack`]: confirmed LSNs and the lag watchdog (W8); [`slots`]: slot indexes, creation and
//!   errors (S5, W7); [`lease`]: KV leases with fencing (R17, W12, S4).
//! - [`check`]: check groups, the concurrency bound, the R12 cache; [`filter`]: per-role
//!   delivery (R2, R19, W26); [`owner`]: `/internal/realtime/visible`; [`pool`]: check
//!   connections (P8).

pub mod ack;
pub mod app;
pub mod auth;
pub mod bus;
pub mod channel;
pub mod check;
pub mod client_tls;
pub mod config;
pub mod decoder;
pub mod filter;
mod hub;
pub mod lease;
pub mod limits;
pub mod merge;
pub mod orchestrator;
pub mod outq;
pub mod owner;
pub mod partition;
pub mod pool;
pub mod presence;
pub mod project;
pub mod protocol;
pub mod reauth;
pub mod replication;
pub mod schema;
pub mod shutdown;
pub mod slots;
pub mod socket;
pub mod source;
pub mod store;
pub mod ticket;
