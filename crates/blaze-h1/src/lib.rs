//! blaze's own HTTP/1.1 client core, in place of `http` and `httparse`: no dependency but
//! blaze-own, `no_std`, no IO. It writes requests ([`write_request`]), reads responses
//! ([`Response`], a codec fed in pieces), checks byte ranges ([`Response::ranged`]) and decides
//! redirects ([`Follow`]). The connection, TLS and name lookup are a driver's: bstd's
//! `requests::Http::over` runs this over bstd's `Net` (D63).
//!
//! Trust: a response is held to its caller's [`Limits`] before it writes a byte past them;
//! nothing here makes fetched bytes trusted. A record fetched by range is checked against its
//! digest by the caller, after the fetch, before it is used.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

// `alloc`'s `Vec` alone, for `Out`'s impl for it (out.rs), kept while blaze-http's client
// holds its buffers in one.
extern crate alloc;

#[cfg(test)]
extern crate std;

mod error;
mod out;
mod plant;
mod redirect;
mod request;
mod response;

#[cfg(test)]
mod tests;

pub use error::{Error, Why};
pub use out::Out;
pub use plant::H1Plant;
pub use redirect::{Follow, Hop, Origin, Redirects, is_redirect};
pub use request::{HeaderFault, Range, Request, check_header, write_request};
pub use response::{ContentRange, Fed, Limits, Method, Response, ResponseOpts};
