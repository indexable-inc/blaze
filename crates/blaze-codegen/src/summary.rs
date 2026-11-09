//! Release builds: what each function tells the others before any is compiled (#899, #176),
//! as ThinLTO's summaries do, and what a function's compile is given from them.
//!
//! A [`Summary`] is a function of the function alone, kept in the store under its instance
//! key, so a warm build decides every function's [`Imports`] from summaries without building
//! any IR. What a compile is given is in its blob's key ([`Imports::blob_key`]): every body it
//! may build in, by instance key (which holds the callee's code hash), and every fact of the
//! whole program it was compiled with. A callee's edit changes the key of every function that
//! imported it, and of no other.
//!
//! The store keeps a summary as [`Summary::encode`]'s bytes, under
//! [`keys::summary`](crate::query::keys::summary), read back by [`Summary::decode`], which
//! checks every length and tag.

#![forbid(unsafe_code)]

use std::cell::RefCell;

use blaze_format::{
    cursor::{self, Put, Reader, Wanted},
    leb,
};
use blaze_ids::{ContentId, Hasher};
use serde::{Deserialize, Serialize};

use crate::InstanceKey;

/// 2: the fields are hashed as fixed-width little-endian bytes, not as JSON.
pub const IMPORTS_DOMAIN: &str = "blaze v2 imports 2";

/// What a function's `#[inline]` asks, as rustc tells LLVM.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Hint {
    None,
    Inline,
    Always,
    Never,
}

/// An argument at a call site, as far as the caller's IR knows it. 16 bytes.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Arg {
    Unknown,
    /// An integer or pointer-free constant of `bits` bits, at most 64.
    Int {
        bits: u8,
        value: u64,
    },
}

const _: () = assert!(size_of::<Arg>() == 16);

impl Arg {
    /// Its bytes in [`Imports::blob_key`]: a tag, the width and the value, 10 in all.
    fn encode(self, out: &mut Vec<u8>) {
        match self {
            Arg::Unknown => out.put_bytes(&[0; 10]),
            Arg::Int { bits, value } => {
                out.put_bytes(&[1, bits]);
                out.put_u64(value);
            }
        }
    }
}

/// What LLVM's function attribute inference says of a function, which its callers may be
/// compiled knowing. 6 bytes; [`Attrs::bits`] packs it in one.
#[derive(Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq, Hash, Debug)]
pub struct Attrs {
    pub nounwind: bool,
    pub readonly: bool,
    pub readnone: bool,
    pub norecurse: bool,
    pub willreturn: bool,
    pub noreturn: bool,
}

const _: () = assert!(size_of::<Attrs>() == 6);

impl Attrs {
    pub const NOUNWIND: u8 = 1 << 0;
    pub const READONLY: u8 = 1 << 1;
    pub const READNONE: u8 = 1 << 2;
    pub const NORECURSE: u8 = 1 << 3;
    pub const WILLRETURN: u8 = 1 << 4;
    pub const NORETURN: u8 = 1 << 5;

    pub fn bits(self) -> u8 {
        u8::from(self.nounwind) * Attrs::NOUNWIND
            | u8::from(self.readonly) * Attrs::READONLY
            | u8::from(self.readnone) * Attrs::READNONE
            | u8::from(self.norecurse) * Attrs::NORECURSE
            | u8::from(self.willreturn) * Attrs::WILLRETURN
            | u8::from(self.noreturn) * Attrs::NORETURN
    }

    pub fn from_bits(bits: u8) -> Attrs {
        Attrs {
            nounwind: bits & Attrs::NOUNWIND != 0,
            readonly: bits & Attrs::READONLY != 0,
            readnone: bits & Attrs::READNONE != 0,
            norecurse: bits & Attrs::NORECURSE != 0,
            willreturn: bits & Attrs::WILLRETURN != 0,
            noreturn: bits & Attrs::NORETURN != 0,
        }
    }
}

/// A call site of a function. 64 bytes, its `args` on the heap.
#[derive(PartialEq, Eq, Hash, Debug, Clone)] // clone: inside `Summary`
pub struct Call {
    pub callee: InstanceKey,
    /// How many call sites of it the function has.
    pub count: u32,
    /// Per parameter of the callee: what every one of these call sites passes, if they agree.
    pub args: Vec<Arg>,
}

// The callee's key and `count` (36 bytes) and a `Vec`, padded to a word: 64 bytes on a 64-bit
// target.
const _: () = assert!(
    size_of::<Call>() == (36 + size_of::<Vec<Arg>>()).next_multiple_of(align_of::<usize>())
);

// clone: the memo keeps one copy of each summary a build made for the builds after it (driver.rs, flight.rs)
#[derive(PartialEq, Eq, Hash, Debug, Clone)]
pub struct Summary {
    pub key: InstanceKey,
    /// Its instructions, as LLVM counts them after its pre-link pipeline.
    pub size: u32,
    pub hint: Hint,
    pub calls: Vec<Call>,
    /// Functions whose address it takes: they can be called from where no summary says.
    pub addressed: Vec<InstanceKey>,
    pub params: u32,
    /// The parameters its body never reads, by place.
    pub unused: Vec<u32>,
    /// What LLVM infers of it alone, knowing nothing of what it calls.
    pub attrs: Attrs,
    /// What LLVM infers of it when every function of the build it calls (every
    /// [`Call::callee`]) is nounwind, readnone, willreturn and norecurse: what holds of its
    /// own instructions, for [`crate::imports::decide`] to meet with its callees'. Its
    /// `noreturn` is [`Summary::attrs`]'s, as a noreturn callee on one path says nothing of
    /// the others.
    pub body: Attrs,
    /// Whether code outside the build can call it: an exported symbol, a patchable function
    /// of a live build. Nothing is assumed of its callers.
    pub external: bool,
}

/// What a summary's store bytes start with: the format and its version. `BZU1`, the first
/// form after JSON, came with the store key's domain going to 2 ([`keys::summary`]), so no
/// JSON summary is ever read as one.
///
/// [`keys::summary`]: crate::query::keys::summary
const SUMMARY_MAGIC: [u8; 4] = *b"BZU1";

/// The fewest bytes one [`Call`] takes: its callee, then a count and an argument count of a
/// byte each.
const CALL_LEAST: usize = 32 + 1 + 1;

/// [`Summary::encode`]'s flags: [`Summary::attrs`]' bits in the low 6, [`Summary::body`]'s in
/// the next 6, [`Summary::external`] above them; no other bit is set.
const BODY_SHIFT: u16 = 6;
const EXTERNAL: u16 = 1 << 12;
const FLAGS: u16 = (1 << 13) - 1;
/// One `Attrs` set's bits: `attrs` sits below `BODY_SHIFT`, `body` from it. A new attr bit
/// that does not fit is a compile error here; growing the flags needs `BZU2` and a new key domain.
const ATTRS_MASK: u16 = (1 << BODY_SHIFT) - 1;
const _: () = assert!((Attrs::NORETURN as u16) << 1 == 1 << BODY_SHIFT);

/// Why bytes are not a summary: at which byte, and what was wrong there.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SummaryError {
    pub at: usize,
    pub what: &'static str,
}

impl std::fmt::Display for SummaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not a codegen summary (format `BZU1`) at byte {}: {}", self.at, self.what)
    }
}

impl std::error::Error for SummaryError {}

impl From<cursor::Error> for SummaryError {
    fn from(error: cursor::Error) -> SummaryError {
        let what = match error.wanted {
            Wanted::Bytes { .. } => "ends early",
            Wanted::Count { .. } => "a count longer than what is left",
            Wanted::End { .. } => "bytes after the end",
            _ => error.as_str(),
        };
        SummaryError { at: error.at, what }
    }
}

impl Hint {
    fn tag(self) -> u8 {
        match self {
            Hint::None => 0,
            Hint::Inline => 1,
            Hint::Always => 2,
            Hint::Never => 3,
        }
    }

    fn from_tag(tag: u8) -> Option<Hint> {
        match tag {
            0 => Some(Hint::None),
            1 => Some(Hint::Inline),
            2 => Some(Hint::Always),
            3 => Some(Hint::Never),
            _ => None,
        }
    }
}

impl Arg {
    /// Its bytes in a summary: a tag, then for an integer its width and its value as a
    /// LEB128. One byte for [`Arg::Unknown`], the common case.
    fn put_summary(self, out: &mut Vec<u8>) {
        match self {
            Arg::Unknown => out.put_u8(0),
            Arg::Int { bits, value } => {
                out.put_u8(1);
                out.put_u8(bits);
                out.put_uleb(value);
            }
        }
    }

    fn read_summary(input: &mut Reader<'_>) -> Result<Arg, SummaryError> {
        let at = input.at();
        match input.u8()? {
            0 => Ok(Arg::Unknown),
            1 => Ok(Arg::Int { bits: input.u8()?, value: read_u64(input)? }),
            _ => Err(SummaryError {
                at,
                what: "an argument tag that is neither unknown (0) nor an integer (1)",
            }),
        }
    }
}

fn put_count(out: &mut Vec<u8>, len: usize) {
    out.put_uleb(u64::from(u32::try_from(len).expect("a summary's lists are under 2^32 long")));
}

fn read_key(input: &mut Reader<'_>) -> Result<InstanceKey, SummaryError> {
    Ok(InstanceKey(ContentId(*input.array::<32>()?)))
}

/// Whether the LEB128 read from `start` to where `input` is now is `value`'s shortest form, the
/// one [`Summary::encode`] writes: so bytes that decode encode again to themselves.
fn shortest(input: &Reader<'_>, start: usize, value: u64) -> Result<(), SummaryError> {
    match input.at() - start == leb::len_u64(value) {
        true => Ok(()),
        false => Err(SummaryError { at: start, what: "a LEB128 longer than its shortest form" }),
    }
}

fn read_u32(input: &mut Reader<'_>) -> Result<u32, SummaryError> {
    let start = input.at();
    let value = input.uleb32()?;
    shortest(input, start, u64::from(value))?;
    Ok(value)
}

fn read_u64(input: &mut Reader<'_>) -> Result<u64, SummaryError> {
    let start = input.at();
    let value = input.uleb()?;
    shortest(input, start, value)?;
    Ok(value)
}

/// A count of items of at least `each` bytes, as [`Reader::count_uleb`].
fn read_count(input: &mut Reader<'_>, each: usize) -> Result<usize, SummaryError> {
    let start = input.at();
    let count = input.count_uleb(each)?;
    shortest(input, start, count as u64)?;
    Ok(count)
}

impl Summary {
    /// The summary as the store keeps it, one way to write each summary, little-endian:
    ///
    /// | field | bytes |
    /// |---|---|
    /// | magic | `BZU1` |
    /// | [`Summary::key`] | 32 |
    /// | [`Summary::hint`] | 1: `None` 0, `Inline` 1, `Always` 2, `Never` 3 |
    /// | flags | 2: [`Attrs::bits`] of `attrs`, of `body` shifted left 6, `external` as bit 12 |
    /// | [`Summary::size`], [`Summary::params`] | a LEB128 each |
    /// | [`Summary::calls`] | a LEB128 count, then each: callee 32, `count` a LEB128, an argument count a LEB128, then each argument a tag byte (0 unknown; 1 an integer, then its width byte and its value a LEB128) |
    /// | [`Summary::addressed`] | a LEB128 count, then 32 each |
    /// | [`Summary::unused`] | a LEB128 count, then a LEB128 each |
    ///
    /// Nothing follows. Equal summaries are equal bytes: no field is a map, every list keeps
    /// its order, and every LEB128 is written in its shortest form.
    pub fn encode(&self) -> Vec<u8> {
        let args: usize = self.calls.iter().map(|it| 12 * it.args.len()).sum();
        let len = 4
            + 32
            + 3
            + 10
            + 15
            + (CALL_LEAST + 8) * self.calls.len()
            + args
            + 32 * self.addressed.len()
            + 5 * self.unused.len();
        // alloc: the summary's store bytes, moved into the build's put
        let mut out = Vec::with_capacity(len);
        out.put_bytes(&SUMMARY_MAGIC);
        out.put_bytes(&self.key.0.0);
        out.put_u8(self.hint.tag());
        let flags = u16::from(self.attrs.bits())
            | (u16::from(self.body.bits()) << BODY_SHIFT)
            | (u16::from(self.external) * EXTERNAL);
        out.put_u16(flags);
        out.put_uleb(u64::from(self.size));
        out.put_uleb(u64::from(self.params));
        put_count(&mut out, self.calls.len());
        for call in &self.calls {
            out.put_bytes(&call.callee.0.0);
            out.put_uleb(u64::from(call.count));
            put_count(&mut out, call.args.len());
            for arg in &call.args {
                arg.put_summary(&mut out);
            }
        }
        put_count(&mut out, self.addressed.len());
        for key in &self.addressed {
            out.put_bytes(&key.0.0);
        }
        put_count(&mut out, self.unused.len());
        for param in &self.unused {
            out.put_uleb(u64::from(*param));
        }
        out
    }

    /// [`Summary::encode`]'s bytes read back: every length checked against what is left before
    /// anything is allocated for it, every tag and flag against what it can be, and no byte
    /// left over. Bytes that are not a summary are a [`SummaryError`], never a panic.
    pub fn decode(bytes: &[u8]) -> Result<Summary, SummaryError> {
        let mut input = Reader::new(bytes);
        if input.array::<4>()? != &SUMMARY_MAGIC {
            return Err(SummaryError { at: 0, what: "a magic that is not this format's `BZU1`" });
        }
        let key = read_key(&mut input)?;
        let at = input.at();
        let Some(hint) = Hint::from_tag(input.u8()?) else {
            return Err(SummaryError { at, what: "a hint tag past `Never` (3)" });
        };
        let at = input.at();
        let flags = input.u16()?;
        if flags & !FLAGS != 0 {
            return Err(SummaryError { at, what: "flags with a bit set past `external` (bit 12)" });
        }
        let size = read_u32(&mut input)?;
        let params = read_u32(&mut input)?;
        let len = read_count(&mut input, CALL_LEAST)?;
        // alloc: the summary's lists, owned by the build's map of summaries
        let mut calls = Vec::with_capacity(len);
        for _ in 0..len {
            let callee = read_key(&mut input)?;
            let count = read_u32(&mut input)?;
            let len = read_count(&mut input, 1)?;
            // alloc: as `calls`
            let mut args = Vec::with_capacity(len);
            for _ in 0..len {
                args.push(Arg::read_summary(&mut input)?);
            }
            calls.push(Call { callee, count, args });
        }
        let len = read_count(&mut input, 32)?;
        // alloc: as `calls`
        let mut addressed = Vec::with_capacity(len);
        for _ in 0..len {
            addressed.push(read_key(&mut input)?);
        }
        let len = read_count(&mut input, 1)?;
        // alloc: as `calls`
        let mut unused = Vec::with_capacity(len);
        for _ in 0..len {
            unused.push(read_u32(&mut input)?);
        }
        input.finish()?;
        let attrs = |shift: u16| Attrs::from_bits(((flags >> shift) & ATTRS_MASK) as u8);
        Ok(Summary {
            key,
            size,
            hint,
            calls,
            addressed,
            params,
            unused,
            attrs: attrs(0),
            body: attrs(BODY_SHIFT),
            external: flags & EXTERNAL != 0,
        })
    }
}

/// What one release compile is given, decided from the summaries of the whole build.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Eq, Hash, Debug)] // clone: the build takes each release function's copy to drop what it may not build in
pub struct Imports {
    /// The functions whose exports it may build in, at any depth, sorted.
    pub imported: Vec<InstanceKey>,
    /// Its parameters every caller passes the same constant for: it is compiled with them
    /// known.
    pub constants: Vec<(u32, Arg)>,
    /// What it may assume of the functions it calls, sorted by callee.
    pub callees: Vec<(InstanceKey, Attrs)>,
    /// Parameters of callees it need not compute, as no callee reads them: by callee, sorted.
    pub dead: Vec<(InstanceKey, Vec<u32>)>,
}

impl Imports {
    /// The store's key of the code compiled for `key` with these imports: one BLAKE3 over
    /// every field laid out in a buffer the thread reuses.
    pub fn blob_key(&self, key: InstanceKey) -> ContentId {
        thread_local! {
            static BYTES: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) }; // alloc: empty, the buffer the thread reuses
        }
        BYTES.with_borrow_mut(|bytes| {
            bytes.clear();
            self.encode(bytes);
            let mut hasher = Hasher::new(IMPORTS_DOMAIN);
            hasher.id(&key.0).update(bytes);
            hasher.finish()
        })
    }

    /// Every field, each list as its length then its elements at fixed widths, little-endian:
    /// two different `Imports` never give the same bytes.
    fn encode(&self, out: &mut Vec<u8>) {
        let dead: usize = self.dead.iter().map(|it| 36 + 4 * it.1.len()).sum();
        out.reserve(
            16 + 32 * self.imported.len()
                + 14 * self.constants.len()
                + 33 * self.callees.len()
                + dead,
        );
        out.put_u32(self.imported.len() as u32);
        for key in &self.imported {
            out.put_bytes(&key.0.0);
        }
        out.put_u32(self.constants.len() as u32);
        for &(param, arg) in &self.constants {
            out.put_u32(param);
            arg.encode(out);
        }
        out.put_u32(self.callees.len() as u32);
        for (key, attrs) in &self.callees {
            out.put_bytes(&key.0.0);
            out.put_u8(attrs.bits());
        }
        out.put_u32(self.dead.len() as u32);
        for (key, params) in &self.dead {
            out.put_bytes(&key.0.0);
            out.put_u32(params.len() as u32);
            for param in params {
                out.put_u32(*param);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    #[test]
    fn an_import_is_in_the_key() {
        let none = Imports::default();
        let one = Imports { imported: vec![key(2)], ..Imports::default() };
        let other = Imports { imported: vec![key(3)], ..Imports::default() };
        let keys = [none.blob_key(key(1)), one.blob_key(key(1)), other.blob_key(key(1))];
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[1], keys[2]);
    }

    #[test]
    fn every_whole_program_fact_is_in_the_key() {
        let base = Imports::default().blob_key(key(1));
        let constant =
            Imports { constants: vec![(0, Arg::Int { bits: 32, value: 7 })], ..Imports::default() };
        let attrs = Imports {
            callees: vec![(key(2), Attrs { nounwind: true, ..Attrs::default() })],
            ..Imports::default()
        };
        let dead = Imports { dead: vec![(key(2), vec![1])], ..Imports::default() };
        for it in [constant, attrs, dead] {
            assert_ne!(it.blob_key(key(1)), base);
        }
    }

    #[test]
    fn the_key_tells_apart_what_json_did() {
        let int = |bits, value| Arg::Int { bits, value };
        let all = [
            Imports::default(),
            Imports { constants: vec![(0, int(32, 7))], ..Imports::default() },
            Imports { constants: vec![(0, int(64, 7))], ..Imports::default() },
            Imports { constants: vec![(1, int(32, 7))], ..Imports::default() },
            Imports { constants: vec![(0, Arg::Unknown)], ..Imports::default() },
            Imports { constants: vec![(0, int(0, 0))], ..Imports::default() },
            Imports { dead: vec![(key(2), vec![])], ..Imports::default() },
            Imports { dead: vec![(key(2), vec![0])], ..Imports::default() },
            Imports { dead: vec![(key(2), vec![]), (key(3), vec![])], ..Imports::default() },
            Imports { callees: vec![(key(2), Attrs::default())], ..Imports::default() },
            Imports {
                callees: vec![(key(2), Attrs { noreturn: true, ..Attrs::default() })],
                ..Imports::default()
            },
            Imports { imported: vec![key(2), key(3)], ..Imports::default() },
            Imports { imported: vec![key(3), key(2)], ..Imports::default() },
        ];
        let keys: Vec<ContentId> = all.iter().map(|it| it.blob_key(key(1))).collect();
        for (at, one) in keys.iter().enumerate() {
            assert!(keys[at + 1..].iter().all(|other| other != one), "{at}");
        }
        assert_ne!(all[1].blob_key(key(1)), all[1].blob_key(key(2)));
        // The buffer the thread reuses holds nothing over.
        assert_eq!(all[11].blob_key(key(1)), all[11].clone().blob_key(key(1)));
        assert_eq!(keys[0], Imports::default().blob_key(key(1)));
    }

    #[test]
    fn attrs_round_trip_through_their_bits() {
        for bits in 0..64u8 {
            assert_eq!(Attrs::from_bits(bits).bits(), bits);
        }
        let one = Attrs { readonly: true, willreturn: true, ..Attrs::default() };
        assert_eq!(one.bits(), Attrs::READONLY | Attrs::WILLRETURN);
    }

    /// A summary with every field set, each list longer than one and each argument kind in it.
    fn full() -> Summary {
        Summary {
            key: key(1),
            size: 300,
            hint: Hint::Always,
            calls: vec![
                Call { callee: key(2), count: 1, args: vec![] },
                Call {
                    callee: key(3),
                    count: 200,
                    args: vec![
                        Arg::Unknown,
                        Arg::Int { bits: 64, value: u64::MAX },
                        Arg::Int { bits: 0, value: 0 },
                        Arg::Int { bits: 8, value: 200 },
                    ],
                },
            ],
            addressed: vec![key(4), key(5)],
            params: u32::MAX,
            unused: vec![0, 127, 128, u32::MAX],
            attrs: Attrs {
                nounwind: true,
                readonly: true,
                readnone: true,
                norecurse: true,
                willreturn: true,
                noreturn: true,
            },
            body: Attrs { readnone: true, noreturn: true, ..Attrs::default() },
            external: true,
        }
    }

    fn empty() -> Summary {
        Summary {
            key: key(0),
            size: 0,
            hint: Hint::None,
            calls: vec![],
            addressed: vec![],
            params: 0,
            unused: vec![],
            attrs: Attrs::default(),
            body: Attrs::default(),
            external: false,
        }
    }

    /// Every summary decodes from its bytes to itself, and encodes to the same bytes each time
    /// and again from what was decoded (rule 26).
    #[test]
    fn a_summary_round_trips_through_its_store_bytes() {
        let mut each_hint = Vec::new();
        for hint in [Hint::None, Hint::Inline, Hint::Always, Hint::Never] {
            each_hint.push(Summary { hint, ..empty() });
        }
        let only_external = Summary { external: true, ..empty() };
        let only_body = Summary { body: Attrs { nounwind: true, ..Attrs::default() }, ..empty() };
        for summary in [full(), empty(), only_external, only_body].into_iter().chain(each_hint) {
            let bytes = summary.encode();
            assert_eq!(bytes, summary.encode(), "{summary:?}");
            let decoded = Summary::decode(&bytes).unwrap();
            assert_eq!(decoded, summary);
            assert_eq!(decoded.encode(), bytes);
        }
        let (one, other) = (full().encode(), Summary { external: false, ..full() }.encode());
        assert_ne!(one, other, "external is in the bytes");
        assert!(empty().encode().len() < 64, "an empty summary is its key and a few bytes");
    }

    /// Bytes that are not a summary are an error naming the byte, never a panic: every cut of
    /// a summary, a byte over, a wrong magic, tag or flag, and a LEB128 longer than it need be.
    #[test]
    fn bytes_that_are_no_summary_are_refused() {
        let bytes = full().encode();
        for len in 0..bytes.len() {
            assert!(Summary::decode(&bytes[..len]).is_err(), "cut at {len}");
        }
        let mut over = bytes.clone();
        over.push(0);
        assert_eq!(Summary::decode(&over).unwrap_err().what, "bytes after the end");
        let mut magic = bytes.clone();
        magic[0] = b'{';
        assert_eq!(Summary::decode(&magic).unwrap_err().at, 0);
        let mut hint = bytes.clone();
        hint[36] = 4;
        assert_eq!(Summary::decode(&hint).unwrap_err().at, 36);
        let mut flags = bytes.clone();
        flags[38] |= 0x20;
        assert_eq!(Summary::decode(&flags).unwrap_err().at, 37);
        // `size` of an empty summary, 0, as two bytes.
        let mut long = empty().encode();
        long[39] = 0x80;
        long.insert(40, 0x00);
        assert_eq!(Summary::decode(&long).unwrap_err().at, 39);
        // A count of calls that the bytes left can't hold.
        let mut count = empty().encode();
        count[41] = 1;
        assert!(Summary::decode(&count).is_err());
        // An argument tag past an integer: the full summary's first argument, `Unknown`.
        let tail = (1 + 1 + 10) + (1 + 1 + 1) + (1 + 1 + 2) // the three integer arguments after it
            + (1 + 2 * 32) // `addressed`
            + (1 + 1 + 1 + 2 + 5); // `unused`
        let at = bytes.len() - tail - 1;
        let mut tag = bytes.clone();
        assert_eq!(tag[at], 0);
        tag[at] = 2;
        assert_eq!(Summary::decode(&tag).unwrap_err().at, at);
    }

    #[test]
    fn sizes() {
        assert_eq!(size_of::<Arg>(), 16);
        assert_eq!(size_of::<Attrs>(), 6);
        assert_eq!(size_of::<Call>(), 64);
    }
}
