//! `chelabase workbench`: one isolated stack per checkout (W2).

pub mod argv;
pub mod commands;
pub mod env;
pub mod migrate;
pub mod registry;
pub mod slots;
pub mod slug;
pub mod target;

#[cfg(test)]
#[allow(clippy::field_reassign_with_default, reason = "the coordinator's RED tests, kept as written")]
mod tests;

#[cfg(test)]
mod fix_tests;

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod t5_fix_tests;

#[cfg(test)]
mod eos_tests;

#[cfg(test)]
mod stack_target_tests;

#[cfg(test)]
mod stack_fix_tests;
