//! `chelabase-init`: a Chelabase stack's one-shot setup. [`secrets::ensure_secrets`] makes every
//! key, secret and auth file the services read (the self-hosted `init` image runs it; the
//! workbench calls it for its stacks), writing an operator's chosen values ([`seed`], from the
//! compose `.env`) instead of generated ones where given. [`kinds`] holds the per-kind
//! generators the `chelabase` CLI also uses. [`bootstrap::run`] sets up the identity service
//! and, for the workbench, the dev member (`chelabase-init bootstrap`).

pub mod bootstrap;
pub mod cli;
pub mod example;
pub mod fsx;
pub mod kinds;
pub mod owners;
pub mod random;
pub mod roles;
pub mod secrets;
pub mod seed;

pub use secrets::{InitSecrets, Report, SetupCode, ensure_secrets, read_setup_code};
pub use seed::Seeds;
