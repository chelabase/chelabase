//! `chelabase-init`: a Chelabase stack's one-shot setup. [`secrets::ensure_secrets`] makes every
//! key, secret and auth file the services read (the self-hosted `init` image runs it; the
//! workbench calls it for its stacks). [`kinds`] holds the per-kind generators the `chelabase`
//! CLI also uses. [`bootstrap::run`] makes the product's Zitadel objects and binds the seeded
//! tenant (`chelabase-init bootstrap`).

pub mod bootstrap;
pub mod cli;
pub mod fsx;
pub mod kinds;
pub mod owners;
pub mod random;
pub mod secrets;

pub use secrets::{InitSecrets, Report, SetupCode, ensure_secrets, read_setup_code};
