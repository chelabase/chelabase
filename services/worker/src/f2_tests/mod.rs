//! F2 RED tests (fork round 4, final fix round): the worker's storage-token refresh, its
//! settings and personal logins (resync adoption, the reserved-name filter, whole SCRAM
//! verifiers). One file per area.
//!
//! Names these tests pin (the coder implements them):
//! - `Worker::refresh_storage_tokens_with(&self, clock: impl Fn() -> u64 + Send + Sync) ->
//!   RefreshPass`: a pass that reads `clock` (seconds since the epoch) for each endpoint once
//!   its lock is held, just before minting; `refresh_storage_tokens_once(now)` is it with a
//!   clock that always answers `now`, and the loop's passes use the system clock;
//! - `FakePusher::panic_report: Mutex<bool>`: while set, `report()` panics;
//! - `ObservedRole::marked: bool` (false in `ObservedRole::plain`): the role carries the
//!   personal-login marker (SP22's predicate);
//! - the catalog read's `m` lines: `RESYNC_SQL` gains a third `UNION ALL` branch, one row per
//!   role the SP22 predicate marks (`pg_db_role_setting`, `setdatabase = 0`, a `setconfig`
//!   entry starting `chelabase.managed_role=`): `'m'`, the hex name, ten NULLs, so psql prints
//!   `m|<hex name>||||||||||`; `parse_observed` sets `marked` on the `r` role of that name
//!   (whatever the line order), ignores an `m` line naming no `r` role, and refuses an `m` line
//!   with a malformed name or any other non-empty field;
//! - `chelabase_neon::scram::is_verifier(&str) -> bool` (see `crates/chelabase-neon/tests/
//!   f2_scram.rs`): `create_personal_login` and the resync's recreate take only a whole verifier.

mod clock_panic;
mod config;
mod personal;
mod personal_marker;
mod refresh;

/// RFC 7677's example verifier (password "pencil"): a whole SCRAM-SHA-256 verifier.
pub(crate) const PENCIL: &str = "SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=";

/// Only the prefix of a verifier: what the old prefix check let through.
pub(crate) const PREFIX_ONLY: &str = "SCRAM-SHA-256$4096:c2FsdA==$a:b";
