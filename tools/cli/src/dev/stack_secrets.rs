//! The stack's generated service secrets: moved to `chelabase-init` (`kinds::stack_secrets`).

// Used by the workbench's tests.
#[cfg_attr(not(test), allow(unused_imports))]
pub use chelabase_init::kinds::stack_secrets::*;
