//! The machines' simulations (rules 43 and 64): each draws a schedule from one tape, drives the
//! real machine through it with the simulation as its driver, and checks an oracle; each oracle
//! has a planted-bug control that must be caught (`blaze_sim::control`).

mod declare;
mod first_call;
mod heat_tier;
mod judge;
mod parent;
mod plan;
mod promote;
mod property;
mod registry;
mod report;
mod result;
mod slots;
mod steps;
mod suite;
mod toggle;
mod zygote;

use std::fmt::Display;

use blaze_sim::Broke;

/// A 32-byte key whose first eight bytes are `n`: the simulations' keys, ids and records.
pub(crate) fn key(n: u64) -> [u8; 32] {
    let mut out = [0; 32];
    out[..8].copy_from_slice(&n.to_le_bytes());
    out
}

/// The harness went wrong: a machine refused a call the simulation should not have made.
pub(crate) fn harness(error: impl Display) -> Broke {
    Broke::Harness(error.to_string())
}

/// An oracle's finding when `holds` is false.
pub(crate) fn oracle(holds: bool, why: impl FnOnce() -> String) -> Result<(), Broke> {
    match holds {
        true => Ok(()),
        false => Err(Broke::Oracle(why())),
    }
}
