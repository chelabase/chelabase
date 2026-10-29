//! The NATS users file, the Valkey ACL and the storage auth files: moved to `chelabase-init`
//! (`kinds::service_auth`); `chelabase workbench up` makes them with
//! `chelabase_init::ensure_secrets`.

pub use chelabase_init::kinds::service_auth::*;
