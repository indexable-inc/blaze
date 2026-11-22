//! THE SYSTEM LAYER OF A COMPILE-TIME CHILD (D175; comptime-native.md, "The system layer").
//! A proc macro or build script runs natively from blaze's code records in the launcher's
//! process; its code's outside C names are bound by the launcher, and for a compile-time run
//! ONLY through this crate:
//!
//! - [`find`] (table.rs): the table of every C name such code may name and how it is answered:
//!   the process's own definition ([`How::Own`]: memory, strings, unwinding, the one thread's
//!   locks), the same after an argument check ([`How::OwnChecked`]: `mmap`, `mprotect`), the
//!   layer's state ([`How::Local`]), a protocol call ([`How::Asks`]), a refusal with the real
//!   `errno` and a finding ([`How::Refused`]), or a data word ([`How::Data`]). A name with no
//!   row is refused at bind time: the wall.
//! - [`Sys`] (sys.rs): the sans-IO core (C5) holding descriptors, directory streams, the
//!   environment, the working directory and spawned commands, over blaze-comptime's guest
//!   machine.
//! - child.rs (feature `child`): the C functions themselves, the transport trait the launcher
//!   implements over its protocol descriptor, and `binding`, which the launcher's binder
//!   calls for each outside name.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes, default_field_values)]
#![cfg_attr(feature = "child", feature(c_variadic))]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![cfg_attr(not(feature = "child"), deny(blaze::sans_io_core))]
#![cfg_attr(not(feature = "child"), deny(blaze::process_static))]
#![cfg_attr(not(feature = "child"), forbid(unsafe_code))]
#![cfg_attr(feature = "child", deny(unsafe_code))]

#[cfg(test)]
extern crate std;

mod errno;
mod plant;
mod sys;
mod table;

#[cfg(feature = "child")]
pub mod child;

#[cfg(test)]
mod tests;

pub use errno::{Errno, Os};
pub use plant::SysPlant;
pub use sys::{
    Consts, DirEnt, Fault, Fd, Kind, LINUX, Limits, MAC, PROT_READ, PROT_WRITE, Ret, StatOut, Sys, SysOpts, SysState, consts,
    mmap_allowed, mprotect_allowed,
};
pub use table::{BindRefused, How, NAMES, Name, On, Unbound, find};
