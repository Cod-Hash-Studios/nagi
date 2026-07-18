//! Mission state, attention routing, and workspace-bound proof.

pub(crate) mod attention;
pub(crate) mod claims;
mod digest;
pub(crate) mod evidence;
mod journal;
pub(crate) mod model;
pub(crate) mod proof;
pub(crate) mod run_state;
pub(crate) mod runtime;
pub(crate) mod store;
pub(crate) mod verifier;

#[cfg(test)]
mod tests;
