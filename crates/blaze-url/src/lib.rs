//! Addresses as checked types (docs/v2/plan/design/std.md, "Names are typed, and authority is
//! stored"), in one home: [`HostPort`] (a host and a port) in [`host`], and [`Scheme`],
//! [`Origin`] and [`Url`] in [`url`]. Each is checked as it is made; the host and port's check
//! is a `const fn`, so bstd's `host!` checks a literal as its crate compiles.
//!
//! bstd re-exports these unchanged (`bstd::net::HostPort`, `bstd::url`); blaze-http's client,
//! which bstd depends on, parses its URLs with them, so there is one URL rule in the tree.

#![no_std]
// `#[blaze::pure]` and the other markers name blaze's tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
// Not yet `blaze::implicit_allocator`: `Scheme`, `HostPort`, `Url` and both errors hold
// global-heap text (`Arc<str>`, `String`) in their public types, which bstd and blaze-http
// name; they move as one port with those callers (wave-1124-sansio, W1 group a's report).
#![deny(blaze::sans_io_core, blaze::std_use, blaze::process_static)]
#![forbid(unsafe_code)]

// The global heap, until that port: every use says `// alloc:` beside it.
extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod host;
pub mod url;

#[cfg(test)]
mod tests;

pub use host::{HOST_MOST, HostPort, HostPortError, HostPortWhy};
pub use url::{Origin, Scheme, Url, UrlError, UrlWhy};
