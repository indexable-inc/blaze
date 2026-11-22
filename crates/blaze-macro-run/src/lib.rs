//! A PROC MACRO RUN AS BLAZE'S OWN CODE (D175, D193 point 1, piece 0): the pinned `proc_macro`
//! library unchanged in the macro's process, with blaze's bridge server linked beside it; only
//! the call, the world's answers and the outcome cross blaze-comptime's protocol, and the host
//! takes back data it checks, never handles it trusts.
//!
//! - bridge.rs: the bridge's byte encoding, the one cursor of every format here.
//! - method.rs: the 31 methods of the pinned bridge, held to its source by a test.
//! - call.rs: the program, a macro's id, one call and its bytes (no mark crosses).
//! - server.rs: the bridge's server, in the macro's process: handle tables and the 31 methods.
//! - session.rs: the process's side of a call, a sans-IO machine between blaze, the client and
//!   the process's own library.
//! - outcome.rs: the outcome's bytes, and the host's check of every token, mark and diagnostic.
//! - host.rs: blaze's side of a call, over blaze-comptime's `Host`.
//! - refused.rs: a refused call, naming the macro and the rule.
//!
//! Nothing here runs a process, reads a file or reads a clock.

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

mod bridge;
mod call;
mod host;
mod method;
mod outcome;
mod refused;
mod server;
mod session;

#[cfg(test)]
mod tests;

pub use bridge::{Field, Malformed, Reader, Writer};
pub use call::{Call, CallError, CallView, FORMAT, MacroDecl, MacroId, MacroProgram, Tokens};
pub use host::{MacroHost, MacroHostError, MacroHostOpts, Outcome};
pub use method::{METHODS, Method};
pub use outcome::{Counted, OutcomeAt, OutcomeError, OutcomeLimits, OutcomeWriter, write_stopped};
pub use refused::{FileWhy, Refused, Why};
pub use server::{Ask, Dispatched, Place, Server, ServerError, ServerLimit, ServerLimits};
pub use session::{Input, InputKind, Output, Session, SessionError, SessionOpts, SessionState};
