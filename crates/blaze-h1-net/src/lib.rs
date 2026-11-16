//! The driver for blaze-h1 over bstd's [`Net`]: [`Client::send`] looks the host up through the
//! capability's resolver, connects, wraps the connection in TLS for `https` ([`Tls`]), writes
//! one request (`Connection: close`), feeds the answer to [`blaze_h1::Response`], finishes the
//! connection on every path, and follows redirects by the client's [`Redirects`] policy.
//!
//! Everything that decides is in blaze-h1, a sans-IO machine tested on its own; this crate only
//! moves bytes. URLs and addresses are bstd's checked [`Url`] and [`HostPort`], not a second
//! parser.
//!
//! **TLS.** This crate writes no TLS. [`Tls`] is the seam, implemented outside: see its docs
//! for the options and what each costs. Until one is given, [`NoTls`] refuses `https` by name.

#![no_std]
// `#[blaze::linear]` and `#[blaze::consumes]` name blaze's tool; inert under rustc.
#![feature(register_tool)]
#![register_tool(blaze)]

#[cfg(test)]
extern crate std;

mod client;
mod plant;
mod tls;

#[cfg(test)]
mod tests;

pub use client::{Ask, Client, FetchError, FetchWhy, Got};
pub use tls::{NoStream, NoTls, Plain, Stream, StreamError, Tls, TlsError, TlsOp};
