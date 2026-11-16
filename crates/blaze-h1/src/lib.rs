//! blaze's own HTTP/1.1 client core, in place of `http` and `httparse`: no dependency but
//! blaze-own, `no_std`, no IO. It writes requests ([`write_request`]), reads responses
//! ([`Response`], fed in pieces), checks byte ranges ([`Response::ranged`]) and decides
//! redirects ([`Follow`]). The connection, TLS and name lookup are a driver's: blaze-h1-net
//! runs this over bstd's `Net`.
//!
//! Trust: a response is held to its caller's [`Limits`] before it writes a byte past them;
//! nothing here makes fetched bytes trusted. A record fetched by range is checked against its
//! digest by the caller, after the fetch, before it is used.

#![no_std]
#![cfg_attr(test, feature(allocator_api))]
// `#![blaze::sans_io]` in the modules names blaze's tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

#[cfg(test)]
extern crate std;

mod error;
mod plant;
mod redirect;
mod request;
mod response;

#[cfg(test)]
mod tests;

pub use error::{Error, Why};
pub use redirect::{Follow, Hop, Origin, Redirects, is_redirect};
pub use request::{Range, Request, write_request};
pub use response::{ContentRange, Fed, Limits, Response};
