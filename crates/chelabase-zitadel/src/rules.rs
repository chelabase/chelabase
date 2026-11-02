//! Input rules shared by every caller of Zitadel. They live in `chelabase-identity` now; this
//! re-export keeps the old paths until the Zitadel crate is deleted (m13).

pub use chelabase_identity::rules::{normalize_email, strong_password};
