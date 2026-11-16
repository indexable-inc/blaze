//! How blaze's records are squeezed for cold storage and transfer (docs/v2/plan/design/columns.md,
//! "Cold and moving bytes, measured"). Nothing here is on the edit loop's read path: a record a
//! build reads is kept raw or in blaze-columns' encodings and read in place (rule 51,
//! store-next.md (h)).
//!
//! What the measurement chose (examples/squeeze.rs): zstd with a dictionary trained per record
//! kind, for the kinds where it beats plain zstd by more than 1.5 times counting the dictionary's
//! own bytes (scope records: 2.77 times). The dictionary is a record of its own, named by the
//! BLAKE3 content id of its bytes ([`DictId`]); a squeezed record names the dictionary it needs in
//! its header, and a reader without that dictionary is refused by name
//! ([`Refused::MissingDictionary`]), never handed other bytes.
//!
//! A squeezed record:
//!
//! | bytes | what |
//! |---|---|
//! | 4 | `BSQ1` |
//! | 1 | 0: no dictionary; 1: a dictionary |
//! | 32 | the dictionary's [`DictId`], when there is one |
//! | rest | one zstd frame with its content size and its checksum, and nothing after it |
//!
//! The frame's content size is read and checked against the caller's limit before any byte is
//! allocated for it ([`Refused::TooLarge`]); the checksum refuses a corrupt frame that would
//! still decode. The decoder is the C library (vendor/zstd-sys), so that it reads only inside the
//! slices it is given is that library's promise, not this crate's; design/columns.md's note on a
//! decoder in safe Rust is the plan to make it ours.

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]
#![forbid(unsafe_code)]

extern crate alloc;

mod plant;
#[cfg(test)]
mod tests;

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;

use blaze_ids::ContentId;
use zstd::zstd_safe::{self, CCtx, CParameter, DCtx, DDict};

/// The first bytes of a squeezed record.
pub const MAGIC: [u8; 4] = *b"BSQ1";

/// The domain a dictionary's id is hashed in.
const DICT_DOMAIN: &str = "blaze squeeze dictionary";

/// The name of a dictionary record: the content id of its bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DictId(pub ContentId);

impl fmt::Display for DictId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dictionary {}", self.0)
    }
}

/// Why bytes were not squeezed, a dictionary not made, or a squeezed record not read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The bytes do not start with [`MAGIC`].
    NotSqueezed,
    /// The bytes end before the part named.
    Truncated { part: &'static str },
    /// The dictionary flag is neither 0 nor 1.
    UnknownFlag(u8),
    /// The record needs a dictionary the reader was not given.
    MissingDictionary(DictId),
    /// The frame says it holds `size` bytes, more than the caller's `limit`.
    TooLarge { size: u64, limit: usize },
    /// The frame does not say how many bytes it holds.
    NoContentSize,
    /// Bytes follow the frame.
    Trailing { bytes: usize },
    /// zstd refused the frame (its error's name), or it decoded to another length than it said.
    Corrupt { what: &'static str },
    /// zstd refused to compress, train or load a dictionary (its error's name).
    Zstd { what: &'static str },
    /// The output could not grow to `bytes`.
    NoMemory { bytes: usize },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::NotSqueezed => write!(f, "not a squeezed record: it does not start with `BSQ1`"),
            Refused::Truncated { part } => write!(f, "a squeezed record ends inside its {part}"),
            Refused::UnknownFlag(flag) => write!(f, "a squeezed record's dictionary flag is {flag}, not 0 or 1"),
            Refused::MissingDictionary(id) => write!(f, "a squeezed record needs {id}, which the reader was not given"),
            Refused::TooLarge { size, limit } => write!(f, "a squeezed record holds {size} bytes, over the limit of {limit}"),
            Refused::NoContentSize => write!(f, "a squeezed record's frame does not say its size"),
            Refused::Trailing { bytes } => write!(f, "{bytes} bytes follow a squeezed record's frame"),
            Refused::Corrupt { what } => write!(f, "a squeezed record's frame is corrupt: {what}"),
            Refused::Zstd { what } => write!(f, "zstd refused: {what}"),
            Refused::NoMemory { bytes } => write!(f, "no memory for {bytes} bytes"),
        }
    }
}

impl core::error::Error for Refused {}

fn zstd_error(code: usize) -> Refused {
    Refused::Zstd { what: zstd_safe::get_error_name(code) }
}

/// A dictionary record: its bytes, its name, and the decoder's table of it.
pub struct Dictionary<A: Allocator> {
    id: DictId,
    bytes: Vec<u8, A>,
    ddict: DDict<'static>,
}

impl<A: Allocator> Dictionary<A> {
    /// A dictionary of at most `max` bytes trained on `samples` (records of one kind), in
    /// `alloc`.
    pub fn train(samples: &[&[u8]], max: usize, alloc: A) -> Result<Dictionary<A>, Refused>
    where
        A: Clone,
    {
        let mut joined = Vec::new_in(alloc.clone());
        let mut sizes = Vec::new_in(alloc.clone());
        for s in samples {
            joined.try_reserve(s.len()).map_err(|_| Refused::NoMemory { bytes: joined.len() + s.len() })?;
            joined.extend_from_slice(s);
            sizes.try_reserve(1).map_err(|_| Refused::NoMemory { bytes: sizes.len() + 1 })?;
            sizes.push(s.len());
        }
        let mut bytes = Vec::new_in(alloc);
        bytes.try_reserve_exact(max).map_err(|_| Refused::NoMemory { bytes: max })?;
        bytes.resize(max, 0);
        let n = zstd_safe::train_from_buffer(&mut bytes[..], &joined, &sizes).map_err(zstd_error)?;
        bytes.truncate(n);
        Dictionary::from_record(bytes)
    }

    /// The dictionary a record's bytes are, named by their content id.
    pub fn from_record(bytes: Vec<u8, A>) -> Result<Dictionary<A>, Refused> {
        if zstd_safe::get_dict_id_from_dict(&bytes).is_none() {
            return Err(Refused::Zstd { what: "not a zstd dictionary" });
        }
        let ddict = DDict::try_create(&bytes).ok_or(Refused::Zstd { what: "the decoder's dictionary could not be made" })?;
        Ok(Dictionary { id: DictId(ContentId::of(DICT_DOMAIN, &bytes)), bytes, ddict })
    }

    /// Its name.
    pub fn id(&self) -> DictId {
        self.id
    }

    /// Its bytes: the record that is stored and fetched.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Squeezes records at one level with one dictionary or none, fixed for its life (zstd keeps a
/// loaded dictionary across frames).
pub struct Encoder {
    cctx: CCtx<'static>,
    dict: Option<DictId>,
}

impl Encoder {
    /// An encoder at zstd `level`, with `dict` if given.
    pub fn new<A: Allocator>(level: i32, dict: Option<&Dictionary<A>>) -> Result<Encoder, Refused> {
        let mut cctx = CCtx::try_create().ok_or(Refused::Zstd { what: "no compression context" })?;
        for p in [CParameter::CompressionLevel(level), CParameter::ContentSizeFlag(true), CParameter::ChecksumFlag(true)] {
            cctx.set_parameter(p).map_err(zstd_error)?;
        }
        if let Some(d) = dict {
            cctx.load_dictionary(d.bytes()).map_err(zstd_error)?;
        }
        Ok(Encoder { cctx, dict: dict.map(Dictionary::id) })
    }

    /// `input` squeezed, appended to `out`.
    pub fn squeeze<A: Allocator>(&mut self, input: &[u8], out: &mut Vec<u8, A>) -> Result<(), Refused> {
        let head = MAGIC.len() + 1 + if self.dict.is_some() { 32 } else { 0 };
        let bound = head + zstd_safe::compress_bound(input.len());
        let start = out.len();
        out.try_reserve(bound).map_err(|_| Refused::NoMemory { bytes: start + bound })?;
        out.extend_from_slice(&MAGIC);
        match self.dict {
            Some(id) => {
                out.push(1);
                let mut name = id.0.0;
                if plant::on(plant::Plant::NamesOtherDictionary) {
                    name[0] ^= 1;
                }
                out.extend_from_slice(&name);
            }
            None => out.push(0),
        }
        let at = out.len();
        out.resize(start + bound, 0);
        match self.cctx.compress2(&mut out[at..], input) {
            Ok(n) => {
                out.truncate(at + n);
                Ok(())
            }
            Err(code) => {
                out.truncate(start);
                Err(zstd_error(code))
            }
        }
    }
}

/// The parts of a squeezed record's header.
struct Header<'a> {
    dict: Option<DictId>,
    frame: &'a [u8],
}

fn header(input: &[u8]) -> Result<Header<'_>, Refused> {
    let Some((magic, rest)) = input.split_first_chunk::<4>() else {
        return if MAGIC.starts_with(input) { Err(Refused::Truncated { part: "magic" }) } else { Err(Refused::NotSqueezed) };
    };
    if *magic != MAGIC {
        return Err(Refused::NotSqueezed);
    }
    let Some((&flag, rest)) = rest.split_first() else { return Err(Refused::Truncated { part: "dictionary flag" }) };
    match flag {
        0 => Ok(Header { dict: None, frame: rest }),
        1 => {
            let Some((id, frame)) = rest.split_first_chunk::<32>() else { return Err(Refused::Truncated { part: "dictionary id" }) };
            Ok(Header { dict: Some(DictId(ContentId(*id))), frame })
        }
        flag => Err(Refused::UnknownFlag(flag)),
    }
}

/// The dictionary a squeezed record needs, if any: what a reader fetches before [`Decoder::unsqueeze`].
pub fn needs(input: &[u8]) -> Result<Option<DictId>, Refused> {
    header(input).map(|h| h.dict)
}

/// Reads squeezed records.
pub struct Decoder {
    dctx: DCtx<'static>,
}

impl Decoder {
    pub fn new() -> Result<Decoder, Refused> {
        Ok(Decoder { dctx: DCtx::try_create().ok_or(Refused::Zstd { what: "no decompression context" })? })
    }

    /// The bytes `input` squeezes, appended to `out`: at most `limit` of them, checked before
    /// `out` grows; the dictionary it names found by `find` among those the reader holds.
    pub fn unsqueeze<'d, A: Allocator, D: Allocator + 'd>(
        &mut self,
        input: &[u8],
        find: impl Fn(DictId) -> Option<&'d Dictionary<D>>,
        limit: usize,
        out: &mut Vec<u8, A>,
    ) -> Result<(), Refused> {
        let head = header(input)?;
        let dict = match head.dict {
            Some(id) => Some(find(id).filter(|d| d.id == id).ok_or(Refused::MissingDictionary(id))?),
            None => None,
        };
        let size = match zstd_safe::get_frame_content_size(head.frame) {
            Ok(Some(size)) => size,
            Ok(None) => return Err(Refused::NoContentSize),
            Err(_) => return Err(Refused::Corrupt { what: "the frame header" }),
        };
        if size > limit as u64 && !plant::on(plant::Plant::LimitIgnored) {
            return Err(Refused::TooLarge { size, limit });
        }
        let size = usize::try_from(size).map_err(|_| Refused::TooLarge { size, limit })?;
        match zstd_safe::find_frame_compressed_size(head.frame) {
            Ok(n) if n == head.frame.len() || plant::on(plant::Plant::TrailingKept) => {}
            Ok(n) => return Err(Refused::Trailing { bytes: head.frame.len().saturating_sub(n) }),
            Err(code) => return Err(Refused::Corrupt { what: zstd_safe::get_error_name(code) }),
        }
        let start = out.len();
        out.try_reserve_exact(size).map_err(|_| Refused::NoMemory { bytes: start + size })?;
        out.resize(start + size, 0);
        let dst = &mut out[start..];
        let decoded = match dict {
            Some(d) => self.dctx.decompress_using_ddict(dst, head.frame, &d.ddict),
            None => self.dctx.decompress(dst, head.frame),
        };
        match decoded {
            Ok(n) if n == size => Ok(()),
            Ok(_) => {
                out.truncate(start);
                Err(Refused::Corrupt { what: "the frame decoded to another size than it says" })
            }
            Err(code) => {
                out.truncate(start);
                Err(Refused::Corrupt { what: zstd_safe::get_error_name(code) })
            }
        }
    }
}
