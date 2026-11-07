//! Where each branch's database is, and a pool of connections to it: [`chelabase_pgsession`],
//! shared with the other services that run statements on a branch.

pub use chelabase_pgsession::*;
