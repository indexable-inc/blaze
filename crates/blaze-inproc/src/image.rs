//! What a run needs of its executable's headers: where its code is (each code segment's file
//! offset, linked address and length), which address ranges it reserves for the arena, cells
//! and fixed maps, and where its headers are linked, from which a process's slide follows.
//!
//! The daemon reads them from the image part of the code memory (the file's bytes,
//! [`crate::CodeMemory::lay_out`]); the start hook reads the same headers from its own image in
//! memory ([`crate::runtime`]) and checks the daemon's overlays against them. Both read
//! untrusted bytes: every offset and length is checked, nothing here panics on any bytes.
//!
//! A run overlays whole pages, so a code segment must start on a page in the file and in
//! memory and share no page with anything else; a reservation is page-aligned and holds
//! nothing. The formats name them:
//!
//! - ELF: a `PT_LOAD` with `PF_X` is code (never `PF_W`, `p_filesz == p_memsz`); a `PT_LOAD`
//!   with no flags and no file bytes is a reservation. The header is where the `PT_LOAD` of
//!   file offset 0 is linked.
//! - Mach-O (arm64 `MH_EXECUTE`): an `LC_SEGMENT_64` whose `initprot` has execute is code
//!   (never write, `filesize == vmsize`); one of `initprot` 0 and no file bytes is a
//!   reservation, except `__PAGEZERO` at address 0. The header is `__TEXT`'s address.
//! - PE32+ (x86-64 executable): the sections with `IMAGE_SCN_MEM_EXECUTE` are one code segment
//!   from the first to the end of the last (never writable, file offset equal to address,
//!   `VirtualSize` within the raw data); a section with no access and no raw data is a
//!   reservation. The header is `ImageBase`. Views on Windows are 64 KiB granules, so the code
//!   starts on one: the first code section at a 64 KiB RVA, the first section after the code
//!   on the next.

#![forbid(unsafe_code)]

mod elf;
mod macho;
mod pe;

#[cfg(test)]
pub(crate) mod tests;

use std::ops::Range;

use crate::{Error, Format};

/// A code segment: `len` bytes from file offset `offset`, linked at `linked`; both
/// page-aligned, `len` rounded up to whole pages.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Segment {
    pub offset: u64,
    pub linked: u64,
    pub len: u64,
}

/// The executable as a run maps it, in linked addresses.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Layout {
    pub format: Format,
    /// Where the headers are linked: ELF the address of file offset 0, Mach-O `__TEXT`'s,
    /// PE `ImageBase`. A process's slide is where it has them minus this.
    pub header: u64,
    /// Everything the image's segments or sections take, `__PAGEZERO` aside.
    pub span: Range<u64>,
    /// By address.
    pub code: Vec<Segment>,
    /// By address, each whole pages.
    pub reserved: Vec<Range<u64>>,
}

/// What a format's reader finds, before the checks every format shares.
struct Found {
    header: u64,
    span: Range<u64>,
    /// Lengths not yet rounded.
    code: Vec<Segment>,
    reserved: Vec<Range<u64>>,
    /// Every other range the image takes.
    other: Vec<Range<u64>>,
}

/// The layout of the executable whose headers are `headers` (from its first byte: at least
/// through ELF's program headers, Mach-O's load commands, PE's section table), for pages of
/// `page` bytes.
pub fn layout(format: Format, headers: &[u8], page: u64) -> Result<Layout, Error> {
    if !page.is_power_of_two() {
        return Err(Error::new(format!("a page of {page:#x} bytes")));
    }
    let found = match format {
        Format::Elf => elf::read(headers)?,
        Format::MachO => macho::read(headers)?,
        Format::Pe => pe::read(headers)?,
    };
    check(format, found, page)
}

/// How many bytes of headers [`layout`] needs, from the first `prefix` bytes of the image in
/// memory (at least 64 for ELF and PE, 32 for Mach-O); `None` when they don't say.
pub fn headers_len(format: Format, prefix: &[u8]) -> Option<u64> {
    match format {
        Format::Elf => elf::headers_len(prefix),
        Format::MachO => macho::headers_len(prefix),
        Format::Pe => pe::headers_len(prefix),
    }
}

fn overlaps(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

fn check(format: Format, found: Found, page: u64) -> Result<Layout, Error> {
    let Found { header, span, mut code, mut reserved, other } = found;
    code.sort_by_key(|it| it.linked);
    reserved.sort_by_key(|it| it.start);
    let mut ranges: Vec<Range<u64>> = Vec::with_capacity(code.len());
    for segment in &mut code {
        if segment.len == 0 {
            return Err(Error::new(format!("the code segment at {:#x} is empty", segment.linked)));
        }
        if !segment.offset.is_multiple_of(page) || !segment.linked.is_multiple_of(page) {
            return Err(Error::new(format!(
                "the code segment at {:#x} (file offset {:#x}) does not start on a page of {page:#x}: a run \
                 overlays whole pages, so the link must align code to them",
                segment.linked, segment.offset
            )));
        }
        let len = segment
            .len
            .checked_next_multiple_of(page)
            .filter(|len| segment.linked.checked_add(*len).is_some() && segment.offset.checked_add(*len).is_some())
            .ok_or_else(|| Error::new(format!("the code segment at {:#x} overflows", segment.linked)))?;
        segment.len = len;
        ranges.push(segment.linked..segment.linked + len);
    }
    for (index, range) in ranges.iter().enumerate() {
        let shared = ranges[index + 1..]
            .iter()
            .chain(&other)
            .chain(&reserved)
            .find(|it| overlaps(it, range));
        if let Some(it) = shared {
            return Err(Error::new(format!(
                "the code segment at {:#x}..{:#x} shares a page with {:#x}..{:#x}: a run overlays whole pages of code, \
                 so nothing else may be on them",
                range.start, range.end, it.start, it.end
            )));
        }
    }
    for (index, range) in reserved.iter().enumerate() {
        if range.is_empty() || !range.start.is_multiple_of(page) || !range.end.is_multiple_of(page) {
            return Err(Error::new(format!(
                "the reservation {:#x}..{:#x} is not whole pages of {page:#x}",
                range.start, range.end
            )));
        }
        if let Some(it) = reserved[index + 1..].iter().chain(&other).find(|it| overlaps(it, range)) {
            return Err(Error::new(format!(
                "the reservation {:#x}..{:#x} overlaps {:#x}..{:#x}",
                range.start, range.end, it.start, it.end
            )));
        }
    }
    let inside = |it: &Range<u64>| it.start >= span.start && it.end <= span.end;
    if !ranges.iter().chain(&reserved).all(inside) || !(span.start..span.end.max(span.start)).contains(&header) {
        return Err(Error::new(format!(
            "the image's span {:#x}..{:#x} does not hold its headers, code and reservations",
            span.start, span.end
        )));
    }
    Ok(Layout { format, header, span, code, reserved })
}

// The images' offsets are the file's `u64`s: past `usize` is a short read, as past the end.
pub(crate) fn u16_at(bytes: &[u8], at: u64) -> Option<u16> {
    blaze_bytes::le::get_u16(bytes, usize::try_from(at).ok()?)
}

pub(crate) fn u32_at(bytes: &[u8], at: u64) -> Option<u32> {
    blaze_bytes::le::get_u32(bytes, usize::try_from(at).ok()?)
}

pub(crate) fn u64_at(bytes: &[u8], at: u64) -> Option<u64> {
    blaze_bytes::le::get_u64(bytes, usize::try_from(at).ok()?)
}

/// `start..start + len`, or an error naming `what` when it overflows.
pub(crate) fn range(start: u64, len: u64, what: &str) -> Result<Range<u64>, Error> {
    let end = start.checked_add(len).ok_or_else(|| Error::new(format!("{what} at {start:#x} of {len:#x} bytes overflows")))?;
    Ok(start..end)
}
