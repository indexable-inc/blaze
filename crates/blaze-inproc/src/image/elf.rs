//! ELF64 little-endian executables of this machine: the program headers.

#![forbid(unsafe_code)]

use super::{Found, Segment, range, u16_at, u32_at, u64_at};
use crate::Error;

/// `e_machine` of this machine: `EM_AARCH64` on AArch64, `EM_X86_64` elsewhere.
pub(super) const MACHINE: u16 = if cfg!(target_arch = "aarch64") { 183 } else { 62 };

const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const PHENT: u64 = 56;
const PN_XNUM: u16 = 0xffff;

fn bad(what: &str) -> Error {
    Error::new(format!("the executable is not an ELF image a run maps: {what}"))
}

/// The ELF header's own bytes, then the program headers: what [`read`] needs.
pub(super) fn headers_len(prefix: &[u8]) -> Option<u64> {
    let phoff = u64_at(prefix, 32)?;
    let count = u64::from(u16_at(prefix, 56)?);
    phoff.checked_add(count.checked_mul(PHENT)?).map(|it| it.max(64))
}

pub(super) fn read(bytes: &[u8]) -> Result<Found, Error> {
    if bytes.get(..4) != Some(b"\x7fELF") {
        return Err(bad("no ELF magic"));
    }
    if bytes.get(4..7) != Some(&[2, 1, 1]) {
        return Err(bad("not 64-bit little-endian ELF of version 1"));
    }
    let kind = u16_at(bytes, 16).ok_or_else(|| bad("a short header"))?;
    if kind != ET_DYN && kind != ET_EXEC {
        return Err(bad(&format!("e_type {kind}, not an executable")));
    }
    let machine = u16_at(bytes, 18).ok_or_else(|| bad("a short header"))?;
    if machine != MACHINE {
        return Err(bad(&format!("e_machine {machine}, where this machine is {MACHINE}")));
    }
    let phoff = u64_at(bytes, 32).ok_or_else(|| bad("a short header"))?;
    let entsize = u16_at(bytes, 54).ok_or_else(|| bad("a short header"))?;
    let count = u16_at(bytes, 56).ok_or_else(|| bad("a short header"))?;
    if u64::from(entsize) != PHENT {
        return Err(bad(&format!("program headers of {entsize} bytes")));
    }
    if count == PN_XNUM {
        return Err(bad("more program headers than e_phnum holds"));
    }
    let table = range(phoff, u64::from(count) * PHENT, "the program headers")?;
    if table.end > bytes.len() as u64 {
        return Err(bad(&format!("program headers at {:#x}..{:#x} past the {:#x} bytes read", table.start, table.end, bytes.len())));
    }
    #[allow(clippy::reversed_empty_ranges, reason = "the span starts empty, widened to each segment's start and end")]
    let mut found =
        Found { header: 0, span: u64::MAX..0, code: Vec::new(), reserved: Vec::new(), other: Vec::new() };
    let mut header = None;
    for index in 0..u64::from(count) {
        let at = phoff + index * PHENT;
        let field = |offset: u64| u64_at(bytes, at + offset).ok_or_else(|| bad("a short program header"));
        if u32_at(bytes, at) != Some(PT_LOAD) {
            continue;
        }
        let flags = u32_at(bytes, at + 4).ok_or_else(|| bad("a short program header"))?;
        let (offset, vaddr, filesz, memsz) = (field(8)?, field(16)?, field(32)?, field(40)?);
        let memory = range(vaddr, memsz, "a PT_LOAD")?;
        range(offset, filesz, "a PT_LOAD's file bytes")?;
        if memsz == 0 {
            continue;
        }
        found.span.start = found.span.start.min(memory.start);
        found.span.end = found.span.end.max(memory.end);
        if offset == 0 && filesz != 0 && header.is_none() {
            header = Some(vaddr);
        }
        if flags & PF_X != 0 {
            if flags & PF_W != 0 {
                return Err(bad(&format!("the PT_LOAD at {vaddr:#x} is writable and executable")));
            }
            if filesz != memsz {
                return Err(bad(&format!(
                    "the code PT_LOAD at {vaddr:#x} has {filesz:#x} file bytes of {memsz:#x}: code is all file bytes"
                )));
            }
            found.code.push(Segment { offset, linked: vaddr, len: filesz });
        } else if flags & (PF_R | PF_W | PF_X) == 0 {
            if filesz != 0 {
                return Err(bad(&format!("the PT_LOAD of no access at {vaddr:#x} has file bytes")));
            }
            found.reserved.push(memory);
        } else {
            found.other.push(memory);
        }
    }
    found.header = header.ok_or_else(|| bad("no PT_LOAD maps file offset 0, the headers"))?;
    if found.span.start > found.span.end {
        return Err(bad("no PT_LOAD"));
    }
    Ok(found)
}
