//! Mission state, attention routing, and workspace-bound proof.

#![cfg_attr(
    not(unix),
    allow(
        dead_code,
        reason = "the durable mission runtime is unavailable on non-Unix platforms"
    )
)]

pub(crate) mod attention;
pub(crate) mod claims;
mod digest;
pub(crate) mod evidence;
pub(crate) mod evidence_pack;
pub(crate) mod executor;
pub(crate) mod handoff;
mod journal;
pub(crate) mod model;
pub(crate) mod proof;
pub(crate) mod run_state;
pub(crate) mod runtime;
pub(crate) mod store;
pub(crate) mod verifier;

#[cfg(test)]
mod tests;
