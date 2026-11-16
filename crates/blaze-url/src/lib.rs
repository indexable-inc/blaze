//! Addresses as checked types (docs/v2/plan/design/std.md, "Names are typed, and authority is
//! stored"), in one home: [`HostPort`] (a host and a port) in [`host`], and [`Scheme`],
//! [`Origin`] and [`Url`] in [`url`]. Each is checked as it is made; the host and port's check
//! is a `const fn`, so bstd's `host!` checks a literal as its crate compiles.
//!
//! bstd re-exports these unchanged (`bstd::net::HostPort`, `bstd::url`); blaze-http's client,
//! which bstd depends on, parses its URLs with them, so there is one URL rule in the tree.

#![no_std]
// `#[blaze::pure]` and the other markers name blaze's tool; inert under rustc.
#![feature(register_tool)]
#![register_tool(blaze)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod host;
pub mod url;

#[cfg(test)]
mod tests;

pub use host::{HOST_MOST, HostPort, HostPortError, HostPortWhy};
pub use url::{Origin, Scheme, Url, UrlError, UrlWhy};
