//! The policy core of the workbench's Docker API proxy (workbench-stack-isolation).
//!
//! Four workbench services talk to Docker. Each gets its own socket on a per-stack proxy, and
//! this crate decides, with pure functions and no I/O:
//!
//! - which API call a request is ([`classify`]): a closed [`Op`] enum, so anything not listed
//!   is denied;
//! - whether a client may make it ([`Profile::allows`], [`Profile::allows_target`]);
//! - whether an object belongs to the stack ([`in_scope`]), and how creates and lists are
//!   confined to it ([`force_labels`], [`reserved_image_labels`], [`scope_list_query`],
//!   [`post_filter_list`]).
//!
//! Request bodies (m3) and the server that forwards (m4) build on this.

pub mod ops;
pub mod profile;
pub mod scope;

pub use ops::{ImagePull, ImageRef, Op, OpKind, classify, parse_query};
pub use profile::{MountType, OpRule, Preset, PresetConfig, Profile};
pub use scope::{
    COMPOSE_PROJECT_LABEL, Labels, STACK_LABEL, Stack, force_labels, in_scope, post_filter_list,
    reserved_image_labels, scope_list_query,
};

/// A refusal, with the reason the proxy logs and returns (403).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deny(pub String);

impl Deny {
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

impl std::fmt::Display for Deny {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Deny {}
