//! blaze's own DEFLATE (RFC 1951) with its zlib (RFC 1950) and gzip (RFC 1952) framings, in
//! place of `flate2` and `miniz_oxide`: no dependency but blaze-own, `no_std`, no IO.
//!
//! | type | what |
//! |---|---|
//! | [`Inflate`] | raw DEFLATE read, fed in pieces, bounded by [`Limits`] |
//! | [`InflateInto`], [`ZlibInto`] | the same into one slice the caller sized: no window, no copy |
//! | [`ZlibDecoder`], [`GzipDecoder`] | the same in a frame, its checksum checked |
//! | [`MAX_RATIO`] | DEFLATE's largest expansion (1032), for sizing a bomb limit |
//! | [`Deflate`] | raw DEFLATE written at a [`Level`], deterministic |
//! | [`ZlibEncoder`], [`GzipEncoder`] | the same in a frame |
//! | [`Adler32`], [`Crc32`] | the frames' checksums |
//!
//! Each machine takes bytes and appends bytes; none reads a file or a socket (rule 14). The
//! one-shot functions below run one over a whole buffer and refuse input left after the stream
//! ([`Why::AfterEnd`]); the `_prefix` forms instead return how much of the input the stream was,
//! as a git pack, whose objects are zlib streams end to end, needs.
//!
//! Trust: a reader is held to its caller's [`Limits`] before it writes a byte past them, so
//! what a hostile stream can cost is the caller's choice. Nothing here is a digest: a fetched
//! archive is checked against its digest before it is inflated, not by its CRC.

#![no_std]
#![cfg_attr(test, feature(allocator_api))]
// `#![blaze::sans_io]` names blaze's tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes, portable_simd)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

mod checksum;
mod fast;
pub mod simd;
mod deflate;
mod error;
mod gzip;
mod inflate;
mod plant;
mod tables;
mod zlib;

#[cfg(test)]
mod tests;

use core::alloc::Allocator;

use blaze_own::Vec;

pub use checksum::{Adler32, Crc32, adler32, crc32};
pub use deflate::{Deflate, DeflateError, Level, PlantedDeflate};
pub use error::{Error, Format, Tree, Why};
pub use gzip::{GzipDecoder, GzipEncoder, PlantedGzipDecoder};
pub use inflate::{
    Fed, Inflate, InflateInto, Limits, MAX_RATIO, PlantedInflate, PlantedInflateInto, RATIO_SLACK,
};
pub use plant::DeflatePlant;
pub use zlib::{ZlibDecoder, ZlibEncoder, ZlibInto};

/// `input` deflated at `level`, appended to `out`; the deflater's buffers in `alloc`.
pub fn deflate<A: Allocator + Clone, B: Allocator>(
    input: &[u8],
    level: Level,
    alloc: A,
    out: &mut Vec<u8, B>,
) -> Result<(), DeflateError> {
    let mut d = Deflate::new_in(level, alloc);
    d.write(input, out)?;
    d.finish(out)
}

/// `input` as one zlib stream, appended to `out`.
pub fn zlib<A: Allocator + Clone, B: Allocator>(
    input: &[u8],
    level: Level,
    alloc: A,
    out: &mut Vec<u8, B>,
) -> Result<(), DeflateError> {
    let mut z = ZlibEncoder::new_in(level, alloc);
    z.write(input, out)?;
    z.finish(out)
}

/// `input` as one gzip member, appended to `out`.
pub fn gzip<A: Allocator + Clone, B: Allocator>(
    input: &[u8],
    level: Level,
    alloc: A,
    out: &mut Vec<u8, B>,
) -> Result<(), DeflateError> {
    let mut g = GzipEncoder::new_in(level, alloc);
    g.write(input, out)?;
    g.finish(out)
}

fn whole(format: Format, len: usize, stream: u64) -> Result<(), Error> {
    let extra = len as u64 - stream;
    match extra {
        0 => Ok(()),
        _ => Err(Error::new(format, stream, Why::AfterEnd { extra })),
    }
}

/// The raw DEFLATE stream that is all of `input`, inflated into `out`.
pub fn inflate<A: Allocator + Clone, B: Allocator>(
    input: &[u8],
    limits: Limits,
    alloc: A,
    out: &mut Vec<u8, B>,
) -> Result<(), Error> {
    let stream = inflate_prefix(input, limits, alloc, out)?;
    whole(Format::Deflate, input.len(), stream as u64)
}

/// The raw DEFLATE stream at the start of `input`, inflated into `out`; returns its length.
pub fn inflate_prefix<A: Allocator + Clone, B: Allocator>(
    input: &[u8],
    limits: Limits,
    alloc: A,
    out: &mut Vec<u8, B>,
) -> Result<usize, Error> {
    let mut i = Inflate::new_in(limits, alloc);
    let fed = i.feed(input, out)?;
    i.finish()?;
    Ok(fed.consumed - i.tail().len())
}

/// The zlib stream that is all of `input`, inflated into `out`.
pub fn unzlib<A: Allocator + Clone, B: Allocator>(
    input: &[u8],
    limits: Limits,
    alloc: A,
    out: &mut Vec<u8, B>,
) -> Result<(), Error> {
    let stream = unzlib_prefix(input, limits, alloc, out)?;
    whole(Format::Zlib, input.len(), stream as u64)
}

/// The zlib stream at the start of `input`, inflated into `out`; returns its length.
pub fn unzlib_prefix<A: Allocator + Clone, B: Allocator>(
    input: &[u8],
    limits: Limits,
    alloc: A,
    out: &mut Vec<u8, B>,
) -> Result<usize, Error> {
    let mut z = ZlibDecoder::new_in(limits, alloc);
    z.feed(input, out)?;
    z.finish()?;
    Ok(z.stream_len() as usize)
}

/// The gzip member that is all of `input`, inflated into `out`.
pub fn gunzip<A: Allocator + Clone, B: Allocator>(
    input: &[u8],
    limits: Limits,
    alloc: A,
    out: &mut Vec<u8, B>,
) -> Result<(), Error> {
    let mut g = GzipDecoder::new_in(limits, alloc);
    g.feed(input, out)?;
    g.finish()?;
    whole(Format::Gzip, input.len(), g.stream_len())
}
