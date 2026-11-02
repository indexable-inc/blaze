//! A dependency image as the store keeps it: one value, little-endian, fixed-layout records,
//! read in place ([`View`]) once [`View::parse`] has checked every offset, length, name and
//! address against the bytes and the key. The segments' bytes sit at page-aligned offsets,
//! so the memory object every run maps is filled with one copy per segment.
//!
//! ```text
//! 0    magic "BZPREMAP", version u32, format u8, arch u8, 2 zero bytes
//! 16   key (32 bytes)
//! 48   base u64, page u64
//! 64   Rust names defined u64, 8 zero bytes
//! 80   directory: SECTIONS x (offset u64, length u64)
//! 304  the sections, each 64-byte aligned; segment bytes page-aligned, last
//! ```
//!
//! Everything before the segment bytes is the image's metadata ([`View::meta_len`]): an opened
//! image keeps only that once its segments are in its memory object.

use blaze_bytes::le::{u32 as u32_at, u64 as u64_at};
use blaze_codegen::content::{arch_of, arch_tag};
use blaze_codegen::{Arch, InstanceKey};
use blaze_graph::par;
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use blaze_link::Format;

use crate::PremapError;
use crate::key::{ImageKey, format_tag};
use crate::prelink::{Bind, BindKind, InitEntry, Placed, Prot, Tls};
use crate::table::{Def, Entry, Kind, SymbolId, TableView};

pub const MAGIC: [u8; 8] = *b"BZPREMAP";
pub const VERSION: u32 = 4;

const SEGMENTS: usize = 0;
const TABLE: usize = 1;
const BINDS: usize = 2;
const LIBRARIES: usize = 3;
const INIT: usize = 4;
const FINI: usize = 5;
const TLS: usize = 6;
const UNWIND: usize = 7;
const STRINGS: usize = 8;
const THREAD_VARS: usize = 9;
const CONTENTS: usize = 10;
const DEBUG: usize = 11;
const STATICS: usize = 12;
const BYTES: usize = 13;
const SECTIONS: usize = 14;

const NAMES: usize = 64;
const DIRECTORY: usize = 80;
const HEADER: usize = DIRECTORY + SECTIONS * 16;

/// addr, size, offset of its bytes, their length, prot, 7 zero bytes.
const SEGMENT: usize = 40;
/// place, addend, name offset u32, name length u32, kind, weak, 2 zero bytes, and the table
/// slot of the image's own definition of the name u32, from 1, 0 for none.
const BIND: usize = 32;
/// name offset u32, name length u32, order u32, as_needed u8, 3 zero bytes.
const LIBRARY: usize = 16;
/// rank u32, order 3 x u32, tie (64), at u64, count u32, 4 zero bytes.
const ENTRY: usize = 96;
const TLS_RECORD: usize = 32;
const UNWIND_RECORD: usize = 16;
const CONTENT: usize = 32;
/// A debug name's digest.
const DEBUG_NAME: usize = 32;
/// item (32), instance (32), definition (32), sorted by item and instance, each once.
const STATIC_RECORD: usize = 96;
/// Segment bytes are copied into the value in pieces of this size, in parallel.
const COPY_PIECE: usize = 1 << 20;

const _: () = assert!(HEADER == 304);
const _: () = assert!(4 + 12 + 64 + 8 + 4 + 4 == ENTRY);

/// What an image is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Meta {
    pub key: ImageKey,
    pub format: Format,
    pub arch: Arch,
    pub base: u64,
    pub page: u64,
}

/// What an image says of its pieces beyond its table: the records its slots' refs index.
pub(crate) struct Pieces<'a> {
    /// A [`table::build`](crate::table::build) table with its refs set.
    pub table: &'a [u8],
    pub contents: &'a [ContentId],
    pub debug: &'a [ContentId],
    /// The image's statics and their definitions, sorted by item and instance, each once.
    pub statics: &'a [((ItemKey, ContentId), ContentId)],
    /// How many of the table's names are Rust names (the build's `names`) the image defines.
    pub names: u64,
}

/// `prelink` and its `pieces`, as one value: the segments' bytes written into it by the
/// prelink, once.
pub(crate) fn encode(meta: &Meta, prelink: &impl Placed, pieces: &Pieces<'_>) -> Result<Vec<u8>, PremapError> {
    let linked = prelink.linked();
    let mut strings = Vec::new(); // alloc: once per image made: its names, of sizes known only as they come
    let mut string = |text: &str| -> Result<(u32, u32), PremapError> {
        let at = u32::try_from(strings.len()).map_err(|_| PremapError::of(format_args!("the image's names pass 4 GiB")))?;
        let len = u32::try_from(text.len()).map_err(|_| PremapError::of(format_args!("a name of 4 GiB")))?;
        strings.extend_from_slice(text.as_bytes());
        Ok((at, len))
    };

    let table = TableView::trusted(pieces.table);
    let mut binds = Vec::with_capacity(linked.binds.len() * BIND);
    for Bind { place, name, addend, kind, weak } in &linked.binds {
        let own = match table.slot_of(&SymbolId::name(name)) {
            Some(at) => u32::try_from(at + 1).map_err(|_| PremapError::of(format_args!("a table of 4 Gi slots")))?,
            None => 0,
        };
        let (at, len) = string(name)?;
        binds.extend_from_slice(&place.to_le_bytes());
        binds.extend_from_slice(&addend.to_le_bytes());
        binds.extend_from_slice(&at.to_le_bytes());
        binds.extend_from_slice(&len.to_le_bytes());
        binds.push(match kind {
            BindKind::Addr => 1,
            BindKind::TpOff => 2,
        });
        binds.push(*weak as u8);
        binds.extend_from_slice(&[0; 2]);
        binds.extend_from_slice(&own.to_le_bytes());
    }
    let mut libraries = Vec::with_capacity(linked.libraries.len() * LIBRARY);
    for library in &linked.libraries {
        let (at, len) = string(&library.name)?;
        libraries.extend_from_slice(&at.to_le_bytes());
        libraries.extend_from_slice(&len.to_le_bytes());
        libraries.extend_from_slice(&library.order.to_le_bytes());
        libraries.push(library.as_needed as u8);
        libraries.extend_from_slice(&[0; 3]);
    }
    let entries_of = |list: &[InitEntry]| {
        let mut out = Vec::with_capacity(list.len() * ENTRY);
        for entry in list {
            out.extend_from_slice(&entry.rank.to_le_bytes());
            for order in entry.order {
                out.extend_from_slice(&order.to_le_bytes());
            }
            out.extend_from_slice(&entry.tie);
            out.extend_from_slice(&entry.at.to_le_bytes());
            out.extend_from_slice(&entry.count.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
        }
        out
    };
    let init = entries_of(&linked.init);
    let fini = entries_of(&linked.fini);
    let mut tls = Vec::with_capacity(TLS_RECORD);
    if let Some(Tls { template, init, size, align }) = linked.tls {
        for it in [template, init, size, align] {
            tls.extend_from_slice(&it.to_le_bytes());
        }
    }
    let ranges = |list: &[(u64, u64)]| {
        let mut out = Vec::with_capacity(list.len() * UNWIND_RECORD);
        for (addr, len) in list {
            out.extend_from_slice(&addr.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
        }
        out
    };
    let unwind = ranges(&linked.unwind);
    let thread_vars = ranges(&linked.thread_vars);
    let mut contents = Vec::with_capacity(pieces.contents.len() * CONTENT);
    for content in pieces.contents {
        contents.extend_from_slice(&content.0);
    }
    let mut debug = Vec::with_capacity(pieces.debug.len() * DEBUG_NAME);
    for digest in pieces.debug {
        debug.extend_from_slice(&digest.0);
    }
    if !pieces.statics.is_sorted_by(|a, b| a.0 < b.0) {
        return Err(PremapError::of(format_args!("the image's statics are not sorted by item and instance, each once")));
    }
    let mut statics = Vec::with_capacity(pieces.statics.len() * STATIC_RECORD);
    for ((item, instance), definition) in pieces.statics {
        statics.extend_from_slice(&item.0.0);
        statics.extend_from_slice(&instance.0);
        statics.extend_from_slice(&definition.0);
    }

    let small: [&[u8]; STATICS + 1] =
        [&[], pieces.table, &binds, &libraries, &init, &fini, &tls, &unwind, &strings, &thread_vars, &contents, &debug, &statics];
    let mut at = HEADER;
    let mut directory = [(0u64, 0u64); SECTIONS];
    // The segment records come first, and their size is known before the bytes' offsets are.
    directory[SEGMENTS] = (at as u64, (linked.segments.len() * SEGMENT) as u64);
    at = align(at + linked.segments.len() * SEGMENT, 64);
    for section in TABLE..=STATICS {
        directory[section] = (at as u64, small[section].len() as u64);
        at = align(at + small[section].len(), 64);
    }
    let page = meta.page as usize;
    let bytes_from = align(at, page);
    let mut placed = Vec::with_capacity(linked.segments.len());
    let mut end = bytes_from;
    for at in 0..linked.segments.len() {
        placed.push(end);
        end = align(end + prelink.len(at) as usize, page);
    }
    directory[BYTES] = (bytes_from as u64, (end - bytes_from) as u64);

    let mut out = Vec::with_capacity(bytes_from);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&[format_tag(meta.format), arch_tag(meta.arch), 0, 0]);
    out.extend_from_slice(&meta.key.0.0);
    out.extend_from_slice(&meta.base.to_le_bytes());
    out.extend_from_slice(&meta.page.to_le_bytes());
    out.extend_from_slice(&pieces.names.to_le_bytes());
    out.extend_from_slice(&[0; 8]);
    for (offset, len) in directory {
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
    }
    debug_assert_eq!(out.len(), HEADER);
    for (at, (segment, offset)) in linked.segments.iter().zip(&placed).enumerate() {
        out.extend_from_slice(&segment.addr.to_le_bytes());
        out.extend_from_slice(&segment.size.to_le_bytes());
        out.extend_from_slice(&(*offset as u64).to_le_bytes());
        out.extend_from_slice(&prelink.len(at).to_le_bytes());
        out.push(prot_tag(segment.prot));
        out.extend_from_slice(&[0; 7]);
    }
    for section in TABLE..=STATICS {
        out.resize(directory[section].0 as usize, 0);
        out.extend_from_slice(small[section]);
    }
    let head = out;
    // Zeroed by the allocator's fresh pages, not a pass of its own: only the bytes copied in are written.
    let mut out = vec![0u8; end]; // alloc: once per image made: its value
    par::for_each(par::zip(par::chunks_mut(&mut out[..head.len()], COPY_PIECE), par::chunks(&head, COPY_PIECE)), |(into, from)| {
        into.copy_from_slice(from);
    });
    prelink.write_into(&mut out, &placed)?;
    Ok(out)
}

fn align(at: usize, to: usize) -> usize {
    at.div_ceil(to) * to
}

/// A segment read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SegmentRef<'a> {
    pub addr: u64,
    pub size: u64,
    pub prot: Prot,
    /// Its bytes, then zeroes to `size`; none when the view holds only the metadata.
    pub bytes: &'a [u8],
    /// Where `bytes` start in the image's value: page-aligned.
    pub offset: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BindRef<'a> {
    pub place: u64,
    pub name: &'a str,
    pub addend: i64,
    pub kind: BindKind,
    pub weak: bool,
    /// The image's own definition of `name`, read from the slot the image recorded for it.
    pub own: Option<Def>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LibraryRef<'a> {
    pub name: &'a str,
    pub order: u32,
    pub as_needed: bool,
}

/// An image, checked, read in place.
#[derive(Clone, Copy)]
pub struct View<'a> {
    bytes: &'a [u8],
    meta: Meta,
    names: u64,
    directory: [(usize, usize); SECTIONS],
    table: TableView<'a>,
}

impl<'a> View<'a> {
    /// Checks `bytes` are an image of `key` and that every reference in them stays inside
    /// them; a store value that is not is refused, naming what is wrong.
    pub fn parse(bytes: &'a [u8], key: ImageKey) -> Result<View<'a>, PremapError> {
        let bad = |what: std::fmt::Arguments<'_>| Err(PremapError::of(format_args!("the dependency image {:?} {what}", key.0)));
        if bytes.len() < HEADER || bytes[..8] != MAGIC {
            return bad(format_args!("is not one: no header"));
        }
        if u32_at(bytes, 8) != VERSION {
            return bad(format_args!("is of version {}, and this blaze reads {VERSION}", u32_at(bytes, 8)));
        }
        let (Some(format), Some(arch)) = (format_of(bytes[12]), arch_of(bytes[13])) else {
            return bad(format_args!("names no format or target there is"));
        };
        if bytes[14..16] != [0, 0] {
            return bad(format_args!("has bytes where its header has none"));
        }
        if bytes[16..48] != key.0.0 {
            return bad(format_args!("is under another key than its own"));
        }
        if bytes[NAMES + 8..DIRECTORY] != [0; 8] {
            return bad(format_args!("has bytes where its header has none"));
        }
        let (base, page) = (u64_at(bytes, 48), u64_at(bytes, 56));
        if !page.is_power_of_two() || page < 4096 || base % page != 0 {
            return bad(format_args!("has a page of {page} and a base of {base:#x}"));
        }
        let mut directory = [(0usize, 0usize); SECTIONS];
        for (section, slot) in directory.iter_mut().enumerate() {
            let offset = u64_at(bytes, DIRECTORY + section * 16);
            let len = u64_at(bytes, DIRECTORY + section * 16 + 8);
            let end = offset.checked_add(len).filter(|end| *end <= bytes.len() as u64);
            if end.is_none() || offset < HEADER as u64 {
                return bad(format_args!("has section {section} outside its bytes"));
            }
            *slot = (offset as usize, len as usize);
        }
        for (section, record) in [
            (SEGMENTS, SEGMENT),
            (BINDS, BIND),
            (LIBRARIES, LIBRARY),
            (INIT, ENTRY),
            (FINI, ENTRY),
            (UNWIND, UNWIND_RECORD),
            (THREAD_VARS, UNWIND_RECORD),
            (CONTENTS, CONTENT),
            (DEBUG, DEBUG_NAME),
            (STATICS, STATIC_RECORD),
        ] {
            if directory[section].1 % record != 0 {
                return bad(format_args!("has section {section} of no whole records"));
            }
        }
        if directory[..BYTES].iter().any(|(offset, len)| offset + len > directory[BYTES].0) {
            return bad(format_args!("has a section after its segment bytes"));
        }
        if directory[TLS].1 != 0 && directory[TLS].1 != TLS_RECORD {
            return bad(format_args!("has a thread-local record of another size"));
        }
        let table = TableView::parse_with(
            &bytes[directory[TABLE].0..][..directory[TABLE].1],
            directory[CONTENTS].1 / CONTENT,
            directory[DEBUG].1 / DEBUG_NAME,
        )?;
        let names = u64_at(bytes, NAMES);
        if names > table.len() as u64 {
            return bad(format_args!("says it defines {names} Rust names, more than its table has"));
        }
        let meta = Meta { key, format, arch, base, page };
        let view = View { bytes, meta, names, directory, table };

        let mut end = base;
        let (from, len) = directory[BYTES];
        for at in 0..directory[SEGMENTS].1 / SEGMENT {
            let record = view.record(SEGMENTS, SEGMENT, at);
            let (addr, size, offset, file) = (u64_at(record, 0), u64_at(record, 8), u64_at(record, 16), u64_at(record, 24));
            if prot_of(record[32]).is_none() || record[33..40] != [0; 7] {
                return bad(format_args!("has segment {at} of no protection there is"));
            }
            if addr % page != 0 || addr < end || size == 0 || file > size {
                return bad(format_args!("has segment {at} at {addr:#x} out of order or of no size"));
            }
            let inside = offset >= from as u64
                && offset % page == 0
                && offset.checked_add(file).is_some_and(|it| it <= (from + len) as u64);
            if !inside {
                return bad(format_args!("has segment {at}'s bytes outside its bytes"));
            }
            end = match addr.checked_add(size) {
                Some(end) => end,
                None => return bad(format_args!("has segment {at} past the end of the address space")),
            };
        }
        let strings = view.section(STRINGS);
        let name = |at: u32, len: u32| -> Option<&str> {
            let range = strings.get(at as usize..(at as usize).checked_add(len as usize)?)?;
            std::str::from_utf8(range).ok()
        };
        let statics = view.section(STATICS);
        if statics.chunks_exact(STATIC_RECORD).zip(statics.chunks_exact(STATIC_RECORD).skip(1)).any(|(a, b)| a[..64] >= b[..64]) {
            return bad(format_args!("has statics not sorted by item and instance, each once"));
        }
        for at in 0..directory[BINDS].1 / BIND {
            let record = view.record(BINDS, BIND, at);
            let place = u64_at(record, 0);
            let Some(text) = name(u32_at(record, 16), u32_at(record, 20)) else {
                return bad(format_args!("has bind {at} that is not one"));
            };
            if !matches!(record[24], 1 | 2) || record[25] > 1 || record[26..28] != [0; 2] {
                return bad(format_args!("has bind {at} that is not one"));
            }
            if table.slot_of(&SymbolId::name(text)).map_or(0, |it| it as u64 + 1) != u64::from(u32_at(record, 28)) {
                return bad(format_args!("has bind {at} naming another slot than its own definition of `{text}`"));
            }
            if place % 8 != 0 || !view.inside(place, 8, true) {
                return bad(format_args!("binds a cell at {place:#x}, outside its writable segments"));
            }
        }
        for at in 0..directory[LIBRARIES].1 / LIBRARY {
            let record = view.record(LIBRARIES, LIBRARY, at);
            if name(u32_at(record, 0), u32_at(record, 4)).is_none() || record[12] > 1 || record[13..16] != [0; 3] {
                return bad(format_args!("has library {at} that is not one"));
            }
        }
        for section in [INIT, FINI] {
            for at in 0..directory[section].1 / ENTRY {
                let record = view.record(section, ENTRY, at);
                let (cell, count) = (u64_at(record, 80), u32_at(record, 88));
                if record[92..96] != [0; 4] || cell % 8 != 0 || !view.inside(cell, u64::from(count) * 8, false) {
                    return bad(format_args!("has an init or fini piece at {cell:#x} outside its segments"));
                }
            }
        }
        if let Some(tls) = view.tls() {
            if tls.init > tls.size || !tls.align.is_power_of_two() || (tls.init > 0 && !view.inside(tls.template, tls.init, false)) {
                return bad(format_args!("has a thread-local template that is not one"));
            }
        }
        for (addr, len) in view.unwind() {
            if !view.inside(addr, len, false) {
                return bad(format_args!("has call frame information at {addr:#x} outside its segments"));
            }
        }
        for (addr, len) in view.thread_vars() {
            if addr % 8 != 0 || len % 24 != 0 || !view.inside(addr, len, true) {
                return bad(format_args!("has thread-local descriptors at {addr:#x} outside its writable segments"));
            }
        }
        Ok(view)
    }

    /// `bytes` [`View::parse`] accepted under `key` before, or their first [`View::meta_len`],
    /// read again with no checks: for the bytes an opened image keeps, which nothing writes.
    /// Bytes that were not checked so may panic on a read, never more.
    pub(crate) fn reopen(bytes: &'a [u8], key: ImageKey) -> View<'a> {
        let format = format_of(bytes[12]).expect("checked when opened");
        let arch = arch_of(bytes[13]).expect("checked when opened");
        let meta = Meta { key, format, arch, base: u64_at(bytes, 48), page: u64_at(bytes, 56) };
        let mut directory = [(0usize, 0usize); SECTIONS];
        for (section, slot) in directory.iter_mut().enumerate() {
            *slot = (
                u64_at(bytes, DIRECTORY + section * 16) as usize,
                u64_at(bytes, DIRECTORY + section * 16 + 8) as usize,
            );
        }
        let table = TableView::trusted(&bytes[directory[TABLE].0..][..directory[TABLE].1]);
        View { bytes, meta, names: u64_at(bytes, NAMES), directory, table }
    }

    pub fn meta(&self) -> Meta {
        self.meta
    }

    /// The image's value, whole, or its metadata when that is all it holds.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// How many bytes come before the segment bytes: all an image needs once it is mapped.
    pub fn meta_len(&self) -> usize {
        self.directory[BYTES].0
    }

    /// Whether the view holds the segment bytes too.
    pub fn whole(&self) -> bool {
        self.bytes.len() >= self.directory[BYTES].0 + self.directory[BYTES].1
    }

    /// How many Rust names (a build's `names`) the image defines.
    pub fn names(&self) -> u64 {
        self.names
    }

    /// How many debug names the image has.
    pub fn debug_names(&self) -> usize {
        self.directory[DEBUG].1 / DEBUG_NAME
    }

    /// The content digest a slot's [`Refs::content`](crate::table::Refs) names.
    pub fn content(&self, at: u32) -> Option<ContentId> {
        let index = (at as usize).checked_sub(1)?;
        Some(ContentId(self.record(CONTENTS, CONTENT, index).try_into().expect("32 bytes")))
    }

    /// The debug name digest a slot's [`Refs::debug`](crate::table::Refs) names.
    pub fn debug_name(&self, at: u32) -> Option<ContentId> {
        let record = self.record(DEBUG, DEBUG_NAME, (at as usize).checked_sub(1)?);
        Some(ContentId(record.try_into().expect("32 bytes")))
    }

    /// How many statics the image has.
    pub fn statics(&self) -> usize {
        self.directory[STATICS].1 / STATIC_RECORD
    }

    /// The image's static at `at` of its sorted list, and its definition.
    pub fn static_record(&self, at: usize) -> ((ItemKey, ContentId), ContentId) {
        let record = self.record(STATICS, STATIC_RECORD, at);
        let id = |from: usize| ContentId(record[from..from + 32].try_into().expect("32 bytes"));
        ((ItemKey(id(0)), id(32)), id(64))
    }

    pub fn table(&self) -> TableView<'a> {
        self.table
    }

    /// One past the image's last byte in the address space.
    pub fn end(&self) -> u64 {
        self.segments().last().map_or(self.meta.base, |it| it.addr + it.size)
    }

    pub fn segments(&self) -> impl Iterator<Item = SegmentRef<'a>> + 'a {
        let (view, whole) = (*self, self.whole());
        (0..self.directory[SEGMENTS].1 / SEGMENT).map(move |at| {
            let record = view.record(SEGMENTS, SEGMENT, at);
            let (offset, len) = (u64_at(record, 16), u64_at(record, 24));
            SegmentRef {
                addr: u64_at(record, 0),
                size: u64_at(record, 8),
                prot: prot_of(record[32]).expect("checked by parse"),
                // The metadata alone holds no segment bytes.
                bytes: match whole {
                    true => view.bytes.get(offset as usize..(offset + len) as usize).expect("checked by parse"),
                    false => &[],
                },
                offset,
            }
        })
    }

    pub fn binds(&self) -> impl Iterator<Item = BindRef<'a>> + 'a {
        let view = *self;
        (0..self.directory[BINDS].1 / BIND).map(move |at| {
            let record = view.record(BINDS, BIND, at);
            BindRef {
                place: u64_at(record, 0),
                addend: u64_at(record, 8) as i64,
                name: view.string(u32_at(record, 16), u32_at(record, 20)),
                kind: if record[24] == 1 { BindKind::Addr } else { BindKind::TpOff },
                weak: record[25] == 1,
                own: (u32_at(record, 28) as usize).checked_sub(1).map(|at| view.table.def_at(at)),
            }
        })
    }

    pub fn libraries(&self) -> impl Iterator<Item = LibraryRef<'a>> + 'a {
        let view = *self;
        (0..self.directory[LIBRARIES].1 / LIBRARY).map(move |at| {
            let record = view.record(LIBRARIES, LIBRARY, at);
            LibraryRef {
                name: view.string(u32_at(record, 0), u32_at(record, 4)),
                order: u32_at(record, 8),
                as_needed: record[12] == 1,
            }
        })
    }

    pub fn init(&self) -> impl Iterator<Item = InitEntry> + 'a {
        self.entries(INIT)
    }

    pub fn fini(&self) -> impl Iterator<Item = InitEntry> + 'a {
        self.entries(FINI)
    }

    pub fn tls(&self) -> Option<Tls> {
        let section = self.section(TLS);
        (!section.is_empty()).then(|| Tls {
            template: u64_at(section, 0),
            init: u64_at(section, 8),
            size: u64_at(section, 16),
            align: u64_at(section, 24),
        })
    }

    pub fn unwind(&self) -> impl Iterator<Item = (u64, u64)> + 'a {
        self.ranges(UNWIND)
    }

    /// Mach-O: the thread-local descriptors (three words each) the run's loader fills.
    pub fn thread_vars(&self) -> impl Iterator<Item = (u64, u64)> + 'a {
        self.ranges(THREAD_VARS)
    }

    fn ranges(&self, section: usize) -> impl Iterator<Item = (u64, u64)> + 'a {
        let view = *self;
        (0..self.directory[section].1 / UNWIND_RECORD).map(move |at| {
            let record = view.record(section, UNWIND_RECORD, at);
            (u64_at(record, 0), u64_at(record, 8))
        })
    }

    fn entries(&self, section: usize) -> impl Iterator<Item = InitEntry> + 'a {
        let view = *self;
        (0..self.directory[section].1 / ENTRY).map(move |at| {
            let record = view.record(section, ENTRY, at);
            InitEntry {
                rank: u32_at(record, 0),
                order: [u32_at(record, 4), u32_at(record, 8), u32_at(record, 12)],
                tie: record[16..80].try_into().expect("64 bytes"),
                at: u64_at(record, 80),
                count: u32_at(record, 88),
            }
        })
    }

    fn section(&self, section: usize) -> &'a [u8] {
        let (offset, len) = self.directory[section];
        &self.bytes[offset..offset + len]
    }

    fn record(&self, section: usize, size: usize, at: usize) -> &'a [u8] {
        &self.section(section)[at * size..(at + 1) * size]
    }

    fn string(&self, at: u32, len: u32) -> &'a str {
        let strings = self.section(STRINGS);
        std::str::from_utf8(&strings[at as usize..at as usize + len as usize]).expect("checked by parse")
    }

    /// Whether `[addr, addr + len)` is inside one segment (a writable one when `writable`).
    fn inside(&self, addr: u64, len: u64, writable: bool) -> bool {
        self.segments().any(|it| {
            addr >= it.addr && addr.checked_add(len).is_some_and(|end| end <= it.addr + it.size) && (!writable || it.prot == Prot::Data)
        })
    }

    /// The symbol `id` is, if the image defines it.
    pub fn get(&self, id: &SymbolId) -> Option<Def> {
        self.table.get(id)
    }

    pub fn entry(&self, id: &SymbolId) -> Option<Entry> {
        self.table.entry(id)
    }
}

/// The dependency image as the workspace image beside it sees it ([`blaze_link::Image::beside`]).
impl blaze_link::Outside for View<'_> {
    fn function(&self, key: &InstanceKey) -> Option<u64> {
        self.get(&SymbolId::function(key)).map(|it| it.value)
    }

    fn static_at(&self, item: &ItemKey, instance: &ContentId) -> Option<blaze_link::premap::StaticAt> {
        self.get(&SymbolId::of_static(item, instance)).map(|it| match it.kind {
            Kind::ThreadLocal => blaze_link::premap::StaticAt::ThreadLocal(it.value),
            Kind::Function | Kind::Data => blaze_link::premap::StaticAt::Addr(it.value),
        })
    }

    fn name(&self, name: &str) -> Option<blaze_link::premap::Def> {
        self.get(&SymbolId::name(name))
    }

    fn reached_directly(&self, name: &str) -> bool {
        self.table.reached_directly(&SymbolId::name(name))
    }

    fn tls_end(&self) -> u64 {
        self.tls().map_or(0, |it| it.size)
    }

    fn end(&self) -> u64 {
        View::end(self)
    }
}

fn prot_tag(prot: Prot) -> u8 {
    match prot {
        Prot::Code => 1,
        Prot::ReadOnly => 2,
        Prot::Data => 3,
    }
}

fn prot_of(tag: u8) -> Option<Prot> {
    match tag {
        1 => Some(Prot::Code),
        2 => Some(Prot::ReadOnly),
        3 => Some(Prot::Data),
        _ => None,
    }
}

fn format_of(tag: u8) -> Option<Format> {
    [Format::Elf, Format::MachO, Format::Pe, Format::Wasm].into_iter().find(|it| format_tag(*it) == tag)
}
