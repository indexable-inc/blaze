//! The host of the keyed driver (docs/v2/plan/design/host.md, "blaze-host"): one process runs a
//! request to its outcome. It builds blaze-keyed's sans-IO [`Driver`](blaze_keyed::driver::Driver)
//! from the request's [`Plan`](blaze_keyed::driver::Plan) and performs every effect it asks on a
//! real [`Store`](blaze_keyed::store::Store) and blaze-graph's worker pool, with the front end
//! given as a [`Front`](blaze_keyed::front::Front).
//!
//! - [`run`]: the one entry: a request in, its [`Outcome`](blaze_keyed::driver::Outcome) and the
//!   records proved per unit out ([`Answered`]).
//! - [`outside`]: the request's outside reads and their answers now, over an injected
//!   [`outside::Files`] (sans-IO); [`disk`] is the real one.
//! - the unit graph is plain data, the plan's: no dependency on blaze-cargo or blaze-units.
//!
//! - a fact is answered by [`blaze_keyed::front::Universe::answer`] over mapped tables (each
//!   unit's surface record and the made table of its manifest root), never by the front end.
//!
//! Floors (rule 67): a no-op costs the certificate's lookup (its shape read, one answer per
//! outside read, its value read) and nothing else: no task, no worker, no put. A unit settled by
//! its mark costs one read. Any other request costs its tasks' own time plus one store read per
//! record the driver asks, one binary search per fact it asks answered, one put per record it
//! files, and one commit; a worker starts only while two or more tasks are ready.

#![forbid(unsafe_code)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
// `#![blaze::sans_io]` in each sans-IO module; inert under rustc.
#![register_tool(blaze)]

#[cfg(unix)]
pub mod disk;
mod host;
pub mod outside;
mod workers;

#[cfg(test)]
mod tests;

pub use self::host::{Answered, HostError, Request, UnitProved, run};
