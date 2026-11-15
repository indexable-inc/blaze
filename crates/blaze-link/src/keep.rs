//! An image's state as bytes: [`Image::keep`] writes it once, [`verify`] checks it once, and
//! [`Image::open`] makes the image again from it, so a later command stages a change and commits
//! it with nothing relinked (docs/v2/plan/design/command.md, "Code": the `ProgramManifest`'s
//! payload). blaze-keyed wraps these bytes as its record; this module knows nothing of keys.
//!
//! # Layout ([`LAYOUT`])
//!
//! One blaze-format buffer: its [`blaze_format::Header`] (magic, the layout tag as its schema,
//! the length, the head's offset), then one section per [`Section`] in [`Section::ALL`]'s order,
//! each starting on a 16-byte boundary and padded to the next, then the head: the section
//! count, then each section's offset, length and digest, then the head's own digest (of the
//! header and the table). Every byte is under exactly one digest. A section is the image's
//! scalars, its symbols, its file's digest, or one field of [`State`]: a column, a list arena
//! (chunk by chunk, garbage and counters too, so every [`crate::cow::ListRef`] still points
//! where it pointed), or a table. A region's bytes and the writer's parts are written as they
//! are, up to their last byte that is not room ([`crate::cow::CowBytes::held`]), and opened in
//! place: the region reads the mapping until a stage writes a page of it.
//!
//! Nothing of the machine is in the bytes (rule 25): no address, no map order (the digest's
//! nodes are sorted, the debug delta's keys too), no process counter (the writer's synthesis
//! number is not kept: an opened state's is 0, so its writer makes its tables again). Two
//! things the caller gave are kept as given: each native input's name (a path on the link line,
//! which resolution matches Mach-O install names against) and the [`Output`] (kept as a hash of
//! its `Debug` form, which holds its paths: interpreter, rpaths, link arguments).
//!
//! # What is not kept, and why
//!
//! Rebuilt on open, each cheaper to rebuild than to read, or tied to the process:
//! - the symbol tables' indexes: each shard's entries are kept in order and put back in order,
//!   which gives the same ids (O(symbols) hashing, no probe table in the bytes);
//! - each native input's parse (`NativeFile`): parsed again from its kept bytes;
//! - the native resolution's graph and its marks: `None`, so the first stage after an open
//!   builds the graph again (`native::resolve`'s path for a changed input);
//! - the writer, its plan and its kept tables but `.rela.dyn`'s slots and the ELF symbol
//!   table's places (the section `rela`: a patched table holds empty slots and entries where a
//!   table made whole would not, so they are history, not a function of the state; the tables'
//!   bytes are the state's `.rela.dyn` region and its file's tail part, opened in place, not
//!   kept a second time). Every other kept table of
//!   either writer (ELF's `.eh_frame_hdr`; Mach-O's symbol table, reused
//!   `__LINKEDIT` tables, `__unwind_info` and page hashes) is patched only where the patch is
//!   the bytes a build whole makes, so made whole after an open it is the same. Also the
//!   objects' cache, the stage arena, the
//!   history (an opened image rolls back nothing), the code memory's cell address and the
//!   debug side file's state (its first link after an open writes it whole).
//!
//! Refused by name at `keep`: a commit in flight, a lazy image, pairs the lazy placer gave
//! that no commit took, a PE or wasm image, and a wasm import (none of them is kept yet).
//!
//! # Floor and cost
//!
//! The floor of an open is one map and a head check; of a stage after it, bytes in proportion
//! to the change. This design: an open maps once and checks the head, then hashes every
//! section once ([`verify`], rule 74) and decodes every column and arena into the stage's
//! copy-on-write chunks; only region bytes and the writer's parts are read in place. Estimate,
//! unmeasured: the hash is ~1 ms per MB, the decode ~2-4 ns per element, so for an image of a
//! million pieces with 100 MB of code the open costs ~40-60 ms against a floor of well under
//! 1 ms, a ratio of ~50x, almost all of it decoding columns and hashing them. The first stage
//! after it also rebuilds the native graph (O(natives' symbols)) and the writer's tables
//! (O(imports and exports)), where the floor is the change. `keep` writes the whole state,
//! O(image), where the floor is the sections the commit changed. The path to the floor:
//! columns as blaze-format plain arrays read in place (a `Chunk` that borrows the mapping), the
//! digest checked once per fetch by the store and not again here, and sections shared by
//! content so a keep writes only the ones a commit changed.

#![forbid(unsafe_code)]

use std::alloc::{Allocator, Global};
use std::fmt;
use std::mem::offset_of;
use std::sync::Arc;

use blaze_codegen::plan::{Origin, Package};
use blaze_codegen::{Backend, InstanceKey, Line, Made};
use blaze_core::{FileName, Mapping};
use blaze_format::cursor::{self, Reader};
use blaze_format::{ALIGN, HEADER_LEN, Header, MAGIC, SchemaHash, SpanRef};
use blaze_ids::{ContentId, Hasher};

use crate::cow::ListRef;
use crate::model::{
    Apply, Bind, Class, Export, Exported, Fixup, Function, GotEntry, Holds, Library, Prot,
    Region, Reserve, SectionName, Sym, SymbolKind, Tls, TlsDescStep, WindowsUnwind,
};
use crate::patch::Filed;
use crate::reloc::GotKind;
use crate::state::State;
use crate::symbols::{IdNo, NameId, Obj, Symbols};
use crate::model::Formats;
use crate::{Format, Image, NativeKind, Output, premap};

/// The layout tag: the header's schema. Any change to what a section holds or how it is
/// written changes this text, and a state of another tag is refused by name, never read.
pub const LAYOUT: SchemaHash = blaze_format::schema("blaze-link kept state 5");

const SECTION_DOMAIN: &str = "blaze-link kept state section 1";
const HEAD_DOMAIN: &str = "blaze-link kept state head 1";
const OUTPUT_DOMAIN: &str = "blaze-link kept state output 1";

/// The head's fixed part: the section count and padding; then [`ENTRY`] bytes per section.
const HEAD: usize = 16;
const ENTRY: usize = 48;
/// Where the first section starts: the header, padded to [`ALIGN`].
const FIRST: usize = HEADER_LEN.next_multiple_of(ALIGN);

/// One part of a kept state, in the order written. A field of [`State`] is its own section,
/// so a state that does not round-trip is named by the field that differs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Section {
    Image,
    Symbols,
    Filed,
    /// The writer's `.rela.dyn` slots and symbol table places ([`crate::elf`]'s `keep_tables`).
    Rela,
    State,
    Specials,
    Spaces,
    Names,
    Inputs,
    Resolved,
    SlotSyms,
    Pairs,
    Parts,
    Debug,
    Exports,
    Encoded,
    Lists,
    DebugDelta,
    Makes,
    Classes,
    Hot,
    What,
    Sort,
    PieceLists,
    Loader,
    Users,
    Fixups,
    Syms,
    WasmNames,
    WasmDeclarers,
    Rebase,
    Binds,
    Defs,
    Cold,
    RefItems,
    Seen,
    Functions,
    Lines,
    Ids,
    Cells,
    SlotNext,
    Removals,
    Texts,
}

const COUNT: usize = std::mem::variant_count::<Section>();

impl Section {
    /// Every section, each at the index it is numbered: a section added to the enum and not
    /// here fails the check below.
    pub(crate) const ALL: [Section; COUNT] = [
        Section::Image,
        Section::Symbols,
        Section::Filed,
        Section::Rela,
        Section::State,
        Section::Specials,
        Section::Spaces,
        Section::Names,
        Section::Inputs,
        Section::Resolved,
        Section::SlotSyms,
        Section::Pairs,
        Section::Parts,
        Section::Debug,
        Section::Exports,
        Section::Encoded,
        Section::Lists,
        Section::DebugDelta,
        Section::Makes,
        Section::Classes,
        Section::Hot,
        Section::What,
        Section::Sort,
        Section::PieceLists,
        Section::Loader,
        Section::Users,
        Section::Fixups,
        Section::Syms,
        Section::WasmNames,
        Section::WasmDeclarers,
        Section::Rebase,
        Section::Binds,
        Section::Defs,
        Section::Cold,
        Section::RefItems,
        Section::Seen,
        Section::Functions,
        Section::Lines,
        Section::Ids,
        Section::Cells,
        Section::SlotNext,
        Section::Removals,
        Section::Texts,
    ];

    /// The sections that are [`State`]'s fields.
    pub(crate) const STATE: &[Section] = {
        let all: &'static [Section; COUNT] = &Section::ALL;
        all.split_at(4).1
    };

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Section::Image => "image",
            Section::Symbols => "symbols",
            Section::Filed => "filed",
            Section::Rela => "rela",
            Section::State => "state",
            Section::Specials => "specials",
            Section::Spaces => "spaces",
            Section::Names => "names",
            Section::Inputs => "inputs",
            Section::Resolved => "resolved",
            Section::SlotSyms => "slot_syms",
            Section::Pairs => "pairs",
            Section::Parts => "parts",
            Section::Debug => "debug",
            Section::Exports => "exports",
            Section::Encoded => "encoded",
            Section::Lists => "lists",
            Section::DebugDelta => "debug_delta",
            Section::Makes => "makes",
            Section::Classes => "classes",
            Section::Hot => "hot",
            Section::What => "what",
            Section::Sort => "sort",
            Section::PieceLists => "piece_lists",
            Section::Loader => "loader",
            Section::Users => "users",
            Section::Fixups => "fixups",
            Section::Syms => "syms",
            Section::WasmNames => "wasm_names",
            Section::WasmDeclarers => "wasm_declarers",
            Section::Rebase => "rebase",
            Section::Binds => "binds",
            Section::Defs => "defs",
            Section::Cold => "cold",
            Section::RefItems => "ref_items",
            Section::Seen => "seen",
            Section::Functions => "functions",
            Section::Lines => "lines",
            Section::Ids => "ids",
            Section::Cells => "cells",
            Section::SlotNext => "slot_next",
            Section::Removals => "removals",
            Section::Texts => "texts",
        }
    }
}

const _: () = {
    let mut at = 0;
    while at < COUNT {
        assert!(Section::ALL[at] as usize == at, "Section::ALL holds each section at its number");
        at += 1;
    }
    assert!(matches!(Section::STATE[0], Section::State));
};

/// Why a kept state was not made or not opened.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KeepError {
    /// The program: the image's file name as its caller names it.
    pub image: String,
    pub why: KeepRefusal,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KeepRefusal {
    /// `keep` of a state this layout does not hold yet: what it is.
    Unkept(Unkept),
    /// The bytes are not as long as their header says, or too short to have one.
    Truncated { have: u64, header: Option<u64> },
    /// Of another layout tag: another blaze's, or not a kept link state.
    Layout { found: ContentId },
    /// A section (or `"header"`, `"head"`) whose bytes are not the ones its digest names.
    Corrupt { section: &'static str },
    /// A section whose bytes are as written but do not decode: `what` says why.
    Invalid { section: &'static str, what: String },
    /// Opened for another output than the one it was linked for.
    Output,
    /// Making the image to open into failed: the linker's error.
    Link(String),
}

impl fmt::Display for KeepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let image = &self.image;
        match &self.why {
            KeepRefusal::Unkept(what) => write!(f, "{image}: its link state is not kept: {what}"),
            KeepRefusal::Truncated { have, header: None } => {
                write!(f, "{image}: a kept link state of {have} bytes, shorter than its header")
            }
            KeepRefusal::Truncated { have, header: Some(len) } => write!(
                f,
                "{image}: a kept link state of {have} bytes whose header says {len}: cut or grown, link it again"
            ),
            KeepRefusal::Layout { found } => write!(
                f,
                "{image}: a kept link state of layout {found}, where this blaze reads {}: link it again",
                LAYOUT.0
            ),
            KeepRefusal::Corrupt { section } => write!(
                f,
                "{image}: the kept link state's section `{section}` is not the bytes its digest names"
            ),
            KeepRefusal::Invalid { section, what } => {
                write!(f, "{image}: the kept link state's section `{section}` does not decode: {what}")
            }
            KeepRefusal::Output => write!(
                f,
                "{image}: the kept link state is of another output (format, target, kind or settings): link it again"
            ),
            KeepRefusal::Link(what) => write!(f, "{image}: opening its kept link state: {what}"),
        }
    }
}

impl std::error::Error for KeepError {}

/// What of an image a kept state does not hold yet ([`KeepRefusal::Unkept`]), found before
/// anything is written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unkept {
    /// A commit is in flight.
    Pending,
    /// A lazy image: its session is this process's.
    Lazy,
    /// The lazy placer gave counter pairs no commit took.
    CounterPairs,
    /// An image of a format whose state is not kept: only ELF's and Mach-O's are.
    Format(Format),
    /// A wasm import.
    WasmImport,
    /// A region the linker made whose name a kept state has none for.
    Synthetic(&'static str),
    /// A debug section whose name a kept state has none for.
    Debug(&'static str),
    /// A special, by index, at an address the writer places that no name defines.
    UnnamedSpecial(usize),
}

impl fmt::Display for Unkept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unkept::Pending => write!(f, "a commit is in flight: keep the image before or after it"),
            Unkept::Lazy => write!(f, "a lazy image: its session is this process's"),
            Unkept::CounterPairs => write!(f, "the lazy placer gave counter pairs no commit took"),
            Unkept::Format(format) => write!(f, "a {format:?} image: only ELF and Mach-O states are kept"),
            Unkept::WasmImport => write!(f, "a wasm import: only ELF and Mach-O states are kept"),
            Unkept::Synthetic(name) => write!(f, "the synthetic region `{name}`, which a kept state has no name for"),
            Unkept::Debug(name) => write!(f, "the debug section `{name}`, which a kept state has no name for"),
            Unkept::UnnamedSpecial(index) => {
                write!(f, "special {index}, an address the writer places, is defined by no name")
            }
        }
    }
}

/// Why a section did not decode.
#[derive(Debug)]
pub(crate) enum Bad {
    Read(cursor::Error),
    What(&'static str),
    Text(String),
}

impl From<cursor::Error> for Bad {
    fn from(error: cursor::Error) -> Bad {
        Bad::Read(error)
    }
}

impl fmt::Display for Bad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Bad::Read(error) => write!(f, "{error}"),
            Bad::What(what) => f.write_str(what),
            Bad::Text(what) => f.write_str(what),
        }
    }
}

/// The bytes being kept: the whole buffer, so an offset in it is a buffer offset.
pub(crate) struct Out<A: Allocator> {
    bytes: Vec<u8, A>,
}

impl<A: Allocator> Out<A> {
    #[inline]
    pub(crate) fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    #[inline]
    pub(crate) fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    pub(crate) fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    pub(crate) fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    pub(crate) fn i32(&mut self, value: i32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    pub(crate) fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    pub(crate) fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    /// A count or a length, as a `u64`.
    #[inline]
    pub(crate) fn len(&mut self, len: usize) {
        self.u64(len as u64);
    }

    /// `bytes` as they are, no length.
    #[inline]
    pub(crate) fn raw(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// A length, then `bytes`.
    pub(crate) fn bytes(&mut self, bytes: &[u8]) {
        self.len(bytes.len());
        self.raw(bytes);
    }

    pub(crate) fn text(&mut self, text: &str) {
        self.bytes(text.as_bytes());
    }

    pub(crate) fn zeros(&mut self, len: usize) {
        self.bytes.resize(self.bytes.len() + len, 0);
    }

    /// Zeros to the next 16-byte boundary of the buffer.
    pub(crate) fn align(&mut self) {
        let at = self.bytes.len();
        self.zeros(at.next_multiple_of(ALIGN) - at);
    }

    pub(crate) fn at(&self) -> usize {
        self.bytes.len()
    }
}

/// One section being opened: its bytes, read front to back, and the mapping they are in, so a
/// region's bytes are read where they lie.
pub(crate) struct In<'a> {
    read: Reader<'a>,
    map: &'a Mapping,
    /// The section's offset in the mapping.
    start: usize,
}

impl<'a> In<'a> {
    #[inline]
    pub(crate) fn u8(&mut self) -> Result<u8, Bad> {
        Ok(self.read.u8()?)
    }

    #[inline]
    pub(crate) fn u16(&mut self) -> Result<u16, Bad> {
        Ok(self.read.u16()?)
    }

    #[inline]
    pub(crate) fn u32(&mut self) -> Result<u32, Bad> {
        Ok(self.read.u32()?)
    }

    #[inline]
    pub(crate) fn u64(&mut self) -> Result<u64, Bad> {
        Ok(self.read.u64()?)
    }

    #[inline]
    pub(crate) fn i32(&mut self) -> Result<i32, Bad> {
        Ok(self.read.i32()?)
    }

    #[inline]
    pub(crate) fn i64(&mut self) -> Result<i64, Bad> {
        Ok(self.read.i64()?)
    }

    pub(crate) fn bool(&mut self) -> Result<bool, Bad> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Bad::What("a flag that is neither 0 nor 1")),
        }
    }

    /// A count of items each at least `each` bytes, checked against the bytes left before
    /// anything is made for them.
    pub(crate) fn count(&mut self, each: usize) -> Result<usize, Bad> {
        Ok(self.read.count_u64(each)?)
    }

    /// A length as [`Out::len`] wrote it, for a value that is not a count of items here.
    pub(crate) fn len(&mut self) -> Result<usize, Bad> {
        usize::try_from(self.u64()?).map_err(|_| Bad::What("a length past the address space"))
    }

    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], Bad> {
        Ok(self.read.bytes_u64()?)
    }

    pub(crate) fn text(&mut self) -> Result<&'a str, Bad> {
        Ok(self.read.str_u64()?)
    }

    pub(crate) fn align(&mut self) -> Result<(), Bad> {
        let at = self.start + self.read.at();
        self.read.skip(at.next_multiple_of(ALIGN) - at)?;
        Ok(())
    }

    /// The next `len` bytes, where they lie: the mapping and their offset in it.
    pub(crate) fn in_place(&mut self, len: usize) -> Result<(Mapping, usize), Bad> {
        let at = self.start + self.read.at();
        self.read.skip(len)?;
        Ok((self.map.clone(), at))
    }

    fn finish(&self) -> Result<(), Bad> {
        Ok(self.read.finish()?)
    }
}

/// A value written into a kept state and read back from it: little-endian, field by field,
/// each enum as a one-byte tag and its fields.
pub(crate) trait Kept: Sized {
    fn put<A: Allocator>(&self, out: &mut Out<A>);
    fn take(input: &mut In<'_>) -> Result<Self, Bad>;
}

macro_rules! kept_scalar {
    ($($ty:ty => $method:ident),* $(,)?) => {$(
        impl Kept for $ty {
            #[inline]
            fn put<A: Allocator>(&self, out: &mut Out<A>) {
                out.$method(*self);
            }

            #[inline]
            fn take(input: &mut In<'_>) -> Result<$ty, Bad> {
                input.$method()
            }
        }
    )*};
}

kept_scalar!(u8 => u8, u16 => u16, u32 => u32, u64 => u64, i32 => i32, i64 => i64, bool => bool);

/// [`Kept`] of a struct, its fields in the order named: every field must be named, since the
/// value is built back with a struct expression.
macro_rules! kept_struct {
    ($ty:ident { $($field:ident),* $(,)? }) => {
        impl $crate::keep::Kept for $ty {
            fn put<A: std::alloc::Allocator>(&self, out: &mut $crate::keep::Out<A>) {
                let $ty { $($field),* } = self;
                $( $crate::keep::Kept::put($field, out); )*
            }

            fn take(input: &mut $crate::keep::In<'_>) -> Result<$ty, $crate::keep::Bad> {
                Ok($ty { $( $field: $crate::keep::Kept::take(input)? ),* })
            }
        }
    };
}

/// [`Kept`] of an enum: each variant's tag and its fields. The match that writes is
/// exhaustive, so a variant added and not listed fails to compile.
macro_rules! kept_enum {
    ($ty:ident {
        $( $tag:literal => $variant:ident $( ( $($tuple:ident),* ) )? $( { $($field:ident),* } )? ),* $(,)?
    }) => {
        impl $crate::keep::Kept for $ty {
            fn put<A: std::alloc::Allocator>(&self, out: &mut $crate::keep::Out<A>) {
                match self {
                    $( $ty::$variant $( ( $($tuple),* ) )? $( { $($field),* } )? => {
                        out.u8($tag);
                        $( $( $crate::keep::Kept::put($tuple, out); )* )?
                        $( $( $crate::keep::Kept::put($field, out); )* )?
                    } )*
                }
            }

            fn take(input: &mut $crate::keep::In<'_>) -> Result<$ty, $crate::keep::Bad> {
                Ok(match input.u8()? {
                    $( $tag => $ty::$variant
                        $( ( $( { let $tuple = $crate::keep::Kept::take(input)?; $tuple } ),* ) )?
                        $( { $( $field: $crate::keep::Kept::take(input)? ),* } )?, )*
                    _ => return Err($crate::keep::Bad::What(concat!("a tag that is no ", stringify!($ty)))),
                })
            }
        }
    };
}

pub(crate) use {kept_enum, kept_struct};

impl<T: Kept + Copy + Default, const N: usize> Kept for [T; N] {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        for it in self {
            it.put(out);
        }
    }

    fn take(input: &mut In<'_>) -> Result<[T; N], Bad> {
        let mut out = [T::default(); N];
        for it in &mut out {
            *it = T::take(input)?;
        }
        Ok(out)
    }
}

impl<T: Kept> Kept for Option<T> {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        match self {
            None => out.u8(0),
            Some(it) => {
                out.u8(1);
                it.put(out);
            }
        }
    }

    fn take(input: &mut In<'_>) -> Result<Option<T>, Bad> {
        match input.u8()? {
            0 => Ok(None),
            1 => Ok(Some(T::take(input)?)),
            _ => Err(Bad::What("an option's tag that is neither 0 nor 1")),
        }
    }
}

impl<X: Kept, Y: Kept> Kept for (X, Y) {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        self.0.put(out);
        self.1.put(out);
    }

    fn take(input: &mut In<'_>) -> Result<(X, Y), Bad> {
        Ok((X::take(input)?, Y::take(input)?))
    }
}

impl<X: Kept, Y: Kept, Z: Kept> Kept for (X, Y, Z) {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        self.0.put(out);
        self.1.put(out);
        self.2.put(out);
    }

    fn take(input: &mut In<'_>) -> Result<(X, Y, Z), Bad> {
        Ok((X::take(input)?, Y::take(input)?, Z::take(input)?))
    }
}

/// A list: its count, then its items.
impl<T: Kept> Kept for Vec<T> {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        out.len(self.len());
        for it in self {
            it.put(out);
        }
    }

    fn take(input: &mut In<'_>) -> Result<Vec<T>, Bad> {
        let count = input.count(1)?;
        let mut out = Vec::with_capacity_in(count, Global);
        for _ in 0..count {
            out.push(T::take(input)?);
        }
        Ok(out)
    }
}

impl<T: Kept> Kept for Arc<Vec<T>> {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        (**self).put(out);
    }

    fn take(input: &mut In<'_>) -> Result<Arc<Vec<T>>, Bad> {
        Ok(Arc::new(Vec::take(input)?))
    }
}

impl Kept for String {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        out.text(self);
    }

    fn take(input: &mut In<'_>) -> Result<String, Bad> {
        Ok(input.text()?.to_owned())
    }
}

impl Kept for Arc<str> {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        out.text(self);
    }

    fn take(input: &mut In<'_>) -> Result<Arc<str>, Bad> {
        Ok(Arc::from(input.text()?))
    }
}

/// A name the linker made, kept as its text and read back as the one `&'static str` it is
/// ([`static_name`]).
impl Kept for &'static str {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        out.text(self);
    }

    fn take(input: &mut In<'_>) -> Result<&'static str, Bad> {
        static_name(input.text()?)
            .ok_or(Bad::What("a section name of the linker's that this blaze does not make"))
    }
}

/// The names the linker makes as `&'static str`s that a kept state holds: the synthetic
/// regions' ([`Class::Synthetic`]) of ELF and Mach-O, and the debug sections'
/// ([`crate::write::Lists::debug`]).
const STATIC_NAMES: [&str; 15] = [
    crate::elf::INTERP,
    crate::elf::BUILD_ID,
    crate::elf::DYNSYM,
    crate::elf::DYNSTR,
    crate::elf::GNU_HASH,
    crate::elf::VERSYM,
    crate::elf::VERNEED,
    crate::elf::RELA_DYN,
    crate::elf::DYNAMIC,
    crate::elf::EH_FRAME_HDR,
    crate::macho::UNWIND_INFO,
    ".debug_abbrev",
    ".debug_info",
    ".debug_line",
    ".debug_str",
];

pub(crate) fn static_name(name: &str) -> Option<&'static str> {
    STATIC_NAMES.iter().copied().find(|it| *it == name)
}

impl Kept for ContentId {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        out.raw(&self.0);
    }

    fn take(input: &mut In<'_>) -> Result<ContentId, Bad> {
        Ok(ContentId(<[u8; 32]>::take(input)?))
    }
}

impl Kept for InstanceKey {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        self.0.put(out);
    }

    fn take(input: &mut In<'_>) -> Result<InstanceKey, Bad> {
        Ok(InstanceKey(ContentId::take(input)?))
    }
}

macro_rules! kept_word {
    ($($ty:ident),*) => {$(
        impl Kept for $ty {
            #[inline]
            fn put<A: Allocator>(&self, out: &mut Out<A>) {
                out.u32(self.0);
            }

            #[inline]
            fn take(input: &mut In<'_>) -> Result<$ty, Bad> {
                Ok($ty(input.u32()?))
            }
        }
    )*};
}

kept_word!(Sym, NameId, IdNo);

impl Kept for crate::state::ClassId {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        out.u16(self.0);
    }

    fn take(input: &mut In<'_>) -> Result<crate::state::ClassId, Bad> {
        Ok(crate::state::ClassId(input.u16()?))
    }
}

impl Kept for SectionName {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        out.text(self.as_str());
    }

    fn take(input: &mut In<'_>) -> Result<SectionName, Bad> {
        Ok(SectionName::new(input.text()?))
    }
}

impl Kept for Package {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        out.text(self.as_str());
    }

    fn take(input: &mut In<'_>) -> Result<Package, Bad> {
        Ok(Package::new(input.text()?))
    }
}

/// A function's lines' span: its anchor's content and index, never an address.
impl Kept for Line {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        out.u32(self.at);
        self.span.anchor.put(out);
        out.u32(self.span.index);
    }

    fn take(input: &mut In<'_>) -> Result<Line, Bad> {
        let at = input.u32()?;
        let anchor = ContentId::take(input)?;
        Ok(Line { at, span: SpanRef { anchor, index: input.u32()? } })
    }
}

kept_struct!(Obj { native, member });
kept_struct!(ListRef { chunk, start, len });
kept_struct!(Fixup { at, apply, target, addend, minus });
kept_struct!(Prot { read, write, exec });
kept_struct!(Region { class, addr, capacity, size, prot, align });
kept_struct!(Tls { addr, init, size, align });
kept_struct!(Export { name, addr, size, kind, weak });
kept_struct!(Library { name, as_needed });
kept_struct!(Exported { name, symbol, data, weak, own });
kept_struct!(GotEntry { addr, holds });
kept_struct!(Bind { addr, import, addend });
kept_struct!(Function { addr, size, fde, windows, name });
kept_struct!(WindowsUnwind { info, handler, lsda });
kept_struct!(Made { backend, origin });

impl Kept for premap::Place {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        let premap::Place { base, page, tls_bias, tls_from, end, beside } = *self;
        (base, page, tls_bias).put(out);
        (tls_from, end, beside).put(out);
    }

    fn take(input: &mut In<'_>) -> Result<premap::Place, Bad> {
        let (base, page, tls_bias) = Kept::take(input)?;
        let (tls_from, end, beside) = Kept::take(input)?;
        Ok(premap::Place { base, page, tls_bias, tls_from, end, beside })
    }
}

kept_enum!(Apply {
    0 => Abs64,
    1 => Abs32,
    2 => Abs32S,
    3 => Rel64,
    4 => Rel32,
    5 => Delta64,
    6 => Delta32,
    7 => ImageRel32,
    8 => SecRel32,
    9 => Section16,
    10 => X86Branch32,
    11 => X86GotPcRel32,
    12 => X86TpOff32,
    13 => X86GotTpOff32,
    14 => X86TlsGd32,
    15 => DtpOff32,
    16 => DtpOff64,
    17 => A64Call26,
    18 => A64Jump26,
    19 => A64Page21,
    20 => A64Adr21,
    21 => A64Lo12 { shift },
    22 => A64CondBr19,
    23 => A64TstBr14,
    24 => A64MovW { group },
    25 => A64GotPage21,
    26 => A64GotLo12,
    27 => GotRel32,
    28 => A64TpHi12,
    29 => A64TpLo12,
    30 => A64TlsIePage21,
    31 => A64TlsIeLo12,
    32 => A64TlsDescPage21,
    33 => A64TlsDescLo12,
    34 => A64TlsDescAdd12,
    35 => A64TlsDescCall,
    36 => A64TlsDescIe(step),
    37 => A64TlsDescLe(step),
    38 => TlvPage21,
    39 => TlvLo12,
    40 => TlvRel32,
    41 => TlsOffset,
    42 => TpOff64,
    43 => Native(kind),
});

kept_enum!(TlsDescStep { 0 => Page, 1 => Load, 2 => Add, 3 => Call });
kept_enum!(GotKind { 0 => Address, 1 => TpOff, 2 => TlsGd, 3 => TlsDesc });
kept_enum!(Reserve { 0 => Arena, 1 => Cells, 2 => Fixed });
kept_enum!(SymbolKind { 0 => Function, 1 => Data, 2 => Tls, 3 => Section, 4 => Other });
kept_enum!(NativeKind { 0 => Object, 1 => Archive { whole }, 2 => Shared { as_needed } });
kept_enum!(Backend { 0 => Cranelift { speed }, 1 => Llvm { opt }, 2 => Stencil });
kept_enum!(Origin { 0 => Member(package), 1 => Dependency(package), 2 => BuildTime(package), 3 => Std });

kept_enum!(Holds {
    0 => Local(value),
    1 => Import(index),
    2 => TpOff(offset),
    3 => ImportTpOff(index),
    4 => TlsGd { import, offset },
    5 => TlsDesc { import, offset },
});

kept_enum!(Class {
    0 => Text,
    1 => Patch,
    2 => Stubs,
    3 => Plt,
    4 => Rodata,
    5 => RelRo,
    6 => EhFrame,
    7 => Data,
    8 => Table,
    9 => Got,
    10 => InitArray,
    11 => FiniArray,
    12 => PreinitArray,
    13 => Reserve(reserve),
    14 => PatchData,
    15 => Named { name, write, exec },
    16 => ThreadVars,
    17 => Tdata,
    18 => Tbss,
    19 => Bss,
    20 => Synthetic(name),
});

/// An import: its wasm part is refused before anything is written ([`Image::keep`]).
impl Kept for crate::model::Import {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        let crate::model::Import { name, library, version, kind, weak, wasm } = self;
        assert!(wasm.is_none(), "a wasm import reached keep: `Image::keep` refuses it first");
        name.put(out);
        library.put(out);
        version.put(out);
        kind.put(out);
        weak.put(out);
    }

    fn take(input: &mut In<'_>) -> Result<crate::model::Import, Bad> {
        Ok(crate::model::Import {
            name: Kept::take(input)?,
            library: Kept::take(input)?,
            version: Kept::take(input)?,
            kind: Kept::take(input)?,
            weak: Kept::take(input)?,
            wasm: None,
        })
    }
}

/// Of a debug delta, its keys and anchors sorted: they are in no particular order, which a
/// kept state must not hold (rule 25).
impl Kept for crate::beside::Delta {
    fn put<A: Allocator>(&self, out: &mut Out<A>) {
        let crate::beside::Delta { base, full, keys, anchors } = self;
        (*base, *full).put(out);
        let mut keys = keys.clone();
        keys.sort_unstable();
        keys.put(out);
        let mut anchors = anchors.clone();
        anchors.sort_unstable();
        anchors.put(out);
    }

    fn take(input: &mut In<'_>) -> Result<crate::beside::Delta, Bad> {
        let (base, full) = Kept::take(input)?;
        Ok(crate::beside::Delta { base, full, keys: Kept::take(input)?, anchors: Kept::take(input)? })
    }
}

/// What the image section holds besides the state.
struct ImageHead {
    output: ContentId,
    tags: u32,
    generations: u64,
    place: Option<premap::Place>,
}

kept_struct!(ImageHead { output, tags, generations, place });

/// The output an image is linked for, as one id: its `Debug` form hashed, the form
/// [`State::lineage`] is made from.
fn output_id(output: &Output) -> ContentId {
    ContentId::of(OUTPUT_DOMAIN, format!("{output:?}").as_bytes())
}

/// A kept state whose every byte is the one its digests name: what [`verify`] gives and
/// [`Image::open`] takes, so the bytes are hashed once however often they are opened.
pub struct Verified {
    image: String,
    map: Mapping,
    /// Each section's offset and length, by [`Section::ALL`].
    sections: [(usize, usize); COUNT],
}

impl Verified {
    pub(crate) fn input(&self, section: Section) -> In<'_> {
        let (at, len) = self.sections[section as usize];
        In { read: Reader::new(&self.map.bytes()[at..at + len]), map: &self.map, start: at }
    }

    /// The section's bytes, for a test that names the first section two states differ in.
    #[cfg(test)]
    pub(crate) fn section(&self, section: Section) -> &[u8] {
        let (at, len) = self.sections[section as usize];
        &self.map.bytes()[at..at + len]
    }

    /// The section's offset in the kept bytes and its length, for a test that flips a byte
    /// of one section.
    #[cfg(test)]
    pub(crate) fn range(&self, section: Section) -> (usize, usize) {
        self.sections[section as usize]
    }

    /// The program it is of, as [`verify`] was told.
    pub fn image(&self) -> &str {
        &self.image
    }
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

fn section_digest(bytes: &[u8]) -> ContentId {
    ContentId::of(SECTION_DOMAIN, bytes)
}

fn head_digest(bytes: &[u8], root: usize, table_end: usize) -> ContentId {
    let mut hasher = Hasher::new(HEAD_DOMAIN);
    hasher.update(&bytes[..FIRST]).update(&bytes[root..table_end]);
    hasher.finish()
}

/// Checks a kept state of the program `image` once: its length, its layout tag, the head's
/// digest and then every section's, in one pass over its bytes. A state that fails is refused
/// naming the program and the first part that is not as written: never read as empty.
pub fn verify(image: &str, kept: Mapping) -> Result<Verified, KeepError> {
    let fail = |why| KeepError { image: image.to_owned(), why };
    let bytes = kept.bytes();
    let have = bytes.len() as u64;
    if bytes.len() < HEADER_LEN {
        return Err(fail(KeepRefusal::Truncated { have, header: None }));
    }
    let len = read_u64(bytes, offset_of!(Header, len));
    if len != have {
        return Err(fail(KeepRefusal::Truncated { have, header: Some(len) }));
    }
    let header = blaze_format::header(bytes).map_err(|error| {
        fail(KeepRefusal::Invalid { section: "header", what: error.to_string() })
    })?;
    if header.schema != LAYOUT {
        return Err(fail(KeepRefusal::Layout { found: header.schema.0 }));
    }
    let corrupt = |section| fail(KeepRefusal::Corrupt { section });
    let root = usize::try_from(header.root).map_err(|_| corrupt("head"))?;
    let table_end = root.checked_add(HEAD + ENTRY * COUNT).ok_or_else(|| corrupt("head"))?;
    if root < FIRST || !root.is_multiple_of(ALIGN) || table_end.checked_add(32) != Some(bytes.len())
    {
        return Err(corrupt("head"));
    }
    if head_digest(bytes, root, table_end).0[..] != bytes[table_end..] {
        return Err(corrupt("head"));
    }
    if u32::from_le_bytes(bytes[root..root + 4].try_into().expect("four bytes")) as usize != COUNT {
        return Err(corrupt("head"));
    }
    let mut sections = [(0, 0); COUNT];
    let mut next = FIRST;
    for (index, section) in Section::ALL.iter().enumerate() {
        let entry = root + HEAD + ENTRY * index;
        let (at, len) = (read_u64(bytes, entry), read_u64(bytes, entry + 8));
        let at = usize::try_from(at).map_err(|_| corrupt("head"))?;
        let len = usize::try_from(len).map_err(|_| corrupt("head"))?;
        let end = at.checked_add(len).map(|it| it.next_multiple_of(ALIGN));
        if at != next || end.is_none_or(|it| it > root) {
            return Err(corrupt("head"));
        }
        let end = end.expect("checked above");
        if section_digest(&bytes[at..end]).0[..] != bytes[entry + 16..entry + ENTRY] {
            return Err(corrupt(section.name()));
        }
        sections[index] = (at, len);
        next = end;
    }
    if next != root {
        return Err(corrupt("head"));
    }
    Ok(Verified { image: image.to_owned(), map: kept, sections })
}

impl Image {
    /// The image's state as bytes ([`crate::keep`]): what [`Image::open`] makes the same image
    /// from, in this process or another, so that staging a change and committing it there
    /// writes the same file as here. Made in `alloc`. Refused, naming the image, for a state
    /// the layout does not hold yet (a commit in flight, a lazy image, a PE or wasm image).
    pub fn keep<A: Allocator>(&self, alloc: A) -> Result<Vec<u8, A>, KeepError> {
        let unkept = |what: Unkept| KeepError {
            image: self.to.as_str().to_owned(),
            why: KeepRefusal::Unkept(what),
        };
        self.can_keep().map_err(unkept)?;
        let mut out = Out { bytes: Vec::new_in(alloc) };
        out.zeros(FIRST);
        let mut table = [(0usize, 0usize, ContentId::default()); COUNT];
        #[cfg(test)]
        let empty = State::new();
        for section in Section::ALL {
            let at = out.at();
            match section {
                Section::Image => ImageHead {
                    output: output_id(&self.output),
                    tags: self.tags,
                    generations: self.generations,
                    place: self.place,
                }
                .put(&mut out),
                Section::Symbols => self.symbols.keep(&mut out),
                Section::Filed => self.filed.keep(&self.state, &mut out),
                Section::Rela => {
                    let of = self.state.synthesis;
                    #[cfg(test)]
                    let of = if self.plant_keep_empty == Some(section.name()) { 0 } else { of };
                    match &self.writer {
                        Formats::Elf(elf) => elf.keep_tables(of, &mut out),
                        _ => out.bool(false),
                    }
                }
                _ => {
                    let state = &self.state;
                    #[cfg(test)]
                    let state = match self.plant_keep_empty == Some(section.name()) {
                        true => &empty,
                        false => state,
                    };
                    state.keep_section(section, &self.symbols, &mut out).map_err(unkept)?;
                }
            }
            let len = out.at() - at;
            out.align();
            table[section as usize] = (at, len, section_digest(&out.bytes[at..]));
        }
        let root = out.at();
        out.u32(COUNT as u32);
        out.zeros(HEAD - 4);
        for (at, len, digest) in table {
            out.len(at);
            out.len(len);
            digest.put(&mut out);
        }
        let table_end = out.at();
        let len = (table_end + 32) as u64;
        let bytes = &mut out.bytes;
        bytes[..8].copy_from_slice(&MAGIC);
        let schema = offset_of!(Header, schema);
        bytes[schema..schema + 32].copy_from_slice(&LAYOUT.0.0);
        let at = offset_of!(Header, len);
        bytes[at..at + 8].copy_from_slice(&len.to_le_bytes());
        let at = offset_of!(Header, root);
        bytes[at..at + 8].copy_from_slice(&(root as u64).to_le_bytes());
        let head = head_digest(&out.bytes, root, table_end);
        head.put(&mut out);
        Ok(out.bytes)
    }

    /// What `keep` refuses, before anything is written.
    fn can_keep(&self) -> Result<(), Unkept> {
        if self.pending.is_some() {
            return Err(Unkept::Pending);
        }
        if self.lazy.is_some() {
            return Err(Unkept::Lazy);
        }
        if !self.placed_pairs.is_empty() {
            return Err(Unkept::CounterPairs);
        }
        if matches!(self.output.format, Format::Pe | Format::Wasm) {
            return Err(Unkept::Format(self.output.format));
        }
        if !self.state.wasm_imports.is_empty()
            || self.state.resolved.imports.iter().any(|it| it.wasm.is_some())
        {
            return Err(Unkept::WasmImport);
        }
        Ok(())
    }

    /// The image `kept` holds, made again to link `to` for `output`: staging a change and
    /// committing it gives the file and the state the image that kept it would. Refused,
    /// naming the program and the reason, for a state of another output or one that does not
    /// decode; never linked again in its place (rule 3).
    pub fn open(to: FileName, output: Output, kept: &Verified) -> Result<Image, KeepError> {
        let program = to.as_str().to_owned();
        let fail = |why| KeepError { image: program.clone(), why };
        let invalid = |section: Section, error: Bad| {
            fail(KeepRefusal::Invalid { section: section.name(), what: error.to_string() })
        };
        let head = read_section(kept, Section::Image, ImageHead::take)
            .map_err(|error| invalid(Section::Image, error))?;
        if head.output != output_id(&output) {
            return Err(fail(KeepRefusal::Output));
        }
        let mut image = Image::placed(to, output, head.place)
            .map_err(|error| fail(KeepRefusal::Link(error.to_string())))?;
        image.tags = head.tags;
        image.generations = head.generations;
        image.symbols = read_section(kept, Section::Symbols, Symbols::open)
            .map_err(|error| invalid(Section::Symbols, error))?;
        image.filed = read_section(kept, Section::Filed, Filed::open)
            .map_err(|error| invalid(Section::Filed, error))?;
        let opening = crate::state::Opening { symbols: &image.symbols, writer: &image.writer };
        let mut state = State::new();
        for &section in Section::STATE {
            let mut input = kept.input(section);
            state
                .open_section(section, &mut input, &opening)
                .and_then(|()| input.finish())
                .map_err(|error| invalid(section, error))?;
        }
        // The writer's `.rela.dyn` slots, as of a synthesis number of this process's, which the
        // opened state takes too: the first link after the open patches the table by slot as
        // the image that kept it would, rather than making it whole in address order.
        let synthesis = crate::write::next_synthesis();
        let mut input = kept.input(Section::Rela);
        let held = match &image.writer {
            Formats::Elf(elf) => elf.open_tables(&mut input, synthesis, &state),
            _ => input.bool().and_then(|held| match held {
                false => Ok(false),
                true => Err(Bad::What("a `.rela.dyn` table kept for an image not ELF")),
            }),
        }
        .and_then(|held| input.finish().map(|()| held))
        .map_err(|error| invalid(Section::Rela, error))?;
        if held {
            state.synthesis = synthesis;
        }
        image.state = state;
        Ok(image)
    }
}

/// `section` read whole by `take`: bytes left after it are an error too.
fn read_section<T>(
    kept: &Verified,
    section: Section,
    take: impl FnOnce(&mut In<'_>) -> Result<T, Bad>,
) -> Result<T, Bad> {
    let mut input = kept.input(section);
    let value = take(&mut input)?;
    input.finish()?;
    Ok(value)
}

/// The first section in which two kept states differ, by name: what the round trip's oracle
/// reports.
#[cfg(test)]
pub(crate) fn first_difference(a: &Verified, b: &Verified) -> Option<&'static str> {
    Section::ALL.iter().find(|it| a.section(**it) != b.section(**it)).map(|it| it.name())
}
