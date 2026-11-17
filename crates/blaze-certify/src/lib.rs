//! The request certificate with no compiler in it (docs/v2/plan/design/certificate.md;
//! end-state/daemon.md 4.2, "Slot"): one check, two drivers.
//!
//! - [`certificate`]: the record a build that answered files (base, read-list, answer) and
//!   the local evidence beside it.
//! - [`request`]: the request machine, which checks a request against its record and writes
//!   the record of a build. The daemon drives it over its store and view.
//! - [`slot`]: the one file a request the daemon writes for its client: everything a check
//!   needs but the disk (the base, the record as the machine read it, the stamps that vouch
//!   for each read, the outputs' stamps).
//! - [`solo`]: the client's driver of the same machine, over a slot and `lstat`s: a client
//!   alone decides "nothing changed" with no daemon, no store opened and no lease, or says
//!   why the daemon must answer.
//! - [`kept`]: what such an answer keeps of its spans for the next daemon (the record and its
//!   directory), and the daemon's take of them into its profile, over its driver's effects.
//!
//! Every module is a sans-IO core (rule 14): no file, clock or store is touched here; each
//! look at the world is a question its driver answers.

#![forbid(unsafe_code)]
// `#![blaze::sans_io]` marks a sans-IO core for `blaze::sans_io_core`; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

pub mod certificate;
pub mod kept;
pub mod request;
pub mod slot;
pub mod solo;

/// The write-once store and the model clock the tests of this crate and of its drivers share
/// (blaze-cargo's tests of the daemon's driver): one home, not a copy each.
#[cfg(any(test, feature = "plants"))]
pub mod testing;
