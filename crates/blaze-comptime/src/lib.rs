//! COMPILE-TIME CODE AS BLAZE'S OWN CODE (D175 step 1): WebAssembly is a bridge; a proc macro
//! or build script runs as native code from blaze's records, in its own confined process, and
//! reaches the world only through this protocol, answered from its grants alone
//! (docs/v2/plan/design/comptime-native.md).
//!
//! - [`Grants`] (grant.rs): everything one run may reach, with the two constructors
//!   [`Grants::proc_macro`] and [`Grants::build_script`]; roots and the read list are
//!   blaze-roots'.
//! - [`Call`] and [`Answer`] (call.rs): every reach of the system the wasm path serves today,
//!   typed both ways; [`SysError`] the real system's errors a refusal is answered with.
//! - The wire (wire.rs): a length-prefixed frame of deterministic CBOR, every length capped.
//! - [`Host`] (host.rs) and [`Guest`] (guest.rs): the two sans-IO machines (C5).
//! - [`first_difference`] (equal.rs): the equality harness's core, wasm against native.
//!
//! Nothing here runs a process, reads a file or reads a clock: the drivers do, and step the
//! machines with what they saw.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes, default_field_values)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

mod call;
mod equal;
mod grant;
mod guest;
mod host;
mod plant;
pub mod wire;

#[cfg(test)]
mod tests;

pub use call::{Answer, Call, CallKind, ClockKind, FileKind, Stream, SysError, Want, Words};
pub use equal::{Difference, Outcome, first_difference};
pub use grant::{
    BuildScriptOpts, CallKey, Clock, Deadline, EnvGrant, EnvValue, Fuel, Grants, GrantsError, OutputCaps, ProcMacroOpts, RunKind,
    Seed, Spawns,
};
pub use guest::{Guest, GuestEffect, GuestError, GuestEvent, GuestOpts, GuestState, fits};
pub use host::{
    Crash, Done, Effect, Ended, Event, EventKind, Fact, FactKind, Facts, Finding, Findings, Found, Grant, Host, HostError, HostOpts,
    HostState, Nanos, OutFile,
};
pub use plant::HostPlant;
pub use wire::{WireError, WireLimits};

/// The protocol's version: the guest's `Hello` names it, and the host refuses another. Every
/// change to call.rs or wire.rs is a new version.
pub const PROTOCOL: u32 = 2;
