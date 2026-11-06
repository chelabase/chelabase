//! Typed access to the identity service (`services/identity`), and an in-memory fake of it.
//!
//! - [`Identity`] is the client: one method per route in `services/identity/openapi.json`
//!   (the table in [`ops`]), refusals as [`IdentityError`].
//! - [`mock::FakeIdentity`] (feature `mock`) is an HTTP stand-in with the service's behaviour,
//!   for tests: the real client talks to it, so tests port without changing their wiring.
//! - [`rules`] holds the input rules every caller shares.

mod client;
mod codes;
mod error;
#[cfg(feature = "mock")]
pub mod mock;
pub mod ops;
pub mod rules;
mod types;

pub use client::Identity;
pub use codes::{CodeKind, CodeRequest, Delivery, EmailStatus, IssuedCode};
pub use error::{IdentityError, Missing, classify_status};
pub use types::{NewUser, Pool, SessionInfo, SessionView, SignedIn, User, UserDetail, UserPage};
