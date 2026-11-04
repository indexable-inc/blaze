//! PE32+ executables for x86-64: the optional header and the section table.

#![forbid(unsafe_code)]

use super::{Found, Segment, range, u16_at, u32_at, u64_at};
use crate::Error;

const MACHINE_AMD64: u16 = 0x8664;
const EXECUTABLE_IMAGE: u16 = 0x0002;
const DLL: u16 = 0x2000;
const PE32_PLUS: u16 = 0x20b;
const SECTION: u64 = 40;
const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const SCN_MEM_READ: u32 = 0x4000_0000;
const SCN_MEM_WRITE: u32 = 0x8000_0000;

fn bad(what: &str) -> Error {
    Error::new(format!("the executable is not a PE image a run maps: {what}"))
}

/// `SizeOfHeaders`: the headers and the section table.
pub(super) fn headers_len(prefix: &[u8]) -> Option<u64> {
    let lfanew = u64::from(u32_at(prefix, 0x3c)?);
    Some(u64::from(u32_at(prefix, lfanew.checked_add(24 + 60)?)?))
}

pub(super) fn read(bytes: &[u8]) -> Result<Found, Error> {
    if bytes.get(..2) != Some(b"MZ") {
        return Err(bad("no MZ header"));
    }
    let lfanew = u64::from(u32_at(bytes, 0x3c).ok_or_else(|| bad("a short DOS header"))?);
    if u32_at(bytes, lfanew) != Some(u32::from_le_bytes(*b"PE\0\0")) {
        return Err(bad("no PE signature"));
    }
    let coff = lfanew + 4;
    let half = |at: u64| u16_at(bytes, at).ok_or_else(|| bad("a short header"));
    if half(coff)? != MACHINE_AMD64 {
        return Err(bad(&format!("machine {:#06x}, not x86-64", half(coff)?)));
    }
    let count = u64::from(half(coff + 2)?);
    let optional_len = u64::from(half(coff + 16)?);
    let characteristics = half(coff + 18)?;
    if characteristics & EXECUTABLE_IMAGE == 0 || characteristics & DLL != 0 {
        return Err(bad(&format!("characteristics {characteristics:#06x}, not an executable")));
    }
    let optional = coff + 20;
    if half(optional)? != PE32_PLUS {
        return Err(bad("not PE32+"));
    }
    let base = u64_at(bytes, optional + 24).ok_or_else(|| bad("a short optional header"))?;
    let size = u64::from(u32_at(bytes, optional + 56).ok_or_else(|| bad("a short optional header"))?);
    let table = range(optional + optional_len, count * SECTION, "the section table")?;
    if table.end > bytes.len() as u64 {
        return Err(bad(&format!("a section table to {:#x}, past the {:#x} bytes read", table.end, bytes.len())));
    }
    let headers = u64::from(u32_at(bytes, optional + 60).ok_or_else(|| bad("a short optional header"))?);
    let span = range(base, size, "the image")?;
    let mut found = Found { header: base, span, code: Vec::new(), reserved: Vec::new(), other: Vec::new() };
    found.other.push(range(base, headers.max(1), "the headers")?);
    let mut code: Option<(u64, u64)> = None;
    for index in 0..count {
        let at = table.start + index * SECTION;
        let field = |offset: u64| u32_at(bytes, at + offset).map(u64::from).ok_or_else(|| bad("a short section header"));
        let (virtual_size, address, raw_size, raw_at) = (field(8)?, field(12)?, field(16)?, field(20)?);
        let flags = u32_at(bytes, at + 36).ok_or_else(|| bad("a short section header"))?;
        let linked = base.checked_add(address).ok_or_else(|| bad("a section past the address space"))?;
        let memory = range(linked, virtual_size.max(raw_size), "a section")?;
        if flags & SCN_MEM_EXECUTE != 0 {
            if flags & SCN_MEM_WRITE != 0 {
                return Err(bad(&format!("the section at RVA {address:#x} is writable and executable")));
            }
            if raw_at != address {
                return Err(bad(&format!(
                    "the code section at RVA {address:#x} has its raw data at {raw_at:#x}: a run needs them equal"
                )));
            }
            if virtual_size > raw_size {
                return Err(bad(&format!(
                    "the code section at RVA {address:#x} is {virtual_size:#x} bytes with {raw_size:#x} of raw data: code \
                     is all raw data"
                )));
            }
            let end = memory.end - base;
            code = Some(code.map_or((address, end), |(start, until)| (start.min(address), until.max(end))));
        } else if flags & (SCN_MEM_READ | SCN_MEM_WRITE | SCN_MEM_EXECUTE) == 0 && raw_size == 0 && virtual_size != 0 {
            found.reserved.push(memory);
        } else if !memory.is_empty() {
            found.other.push(memory);
        }
    }
    if let Some((start, end)) = code {
        found.code.push(Segment { offset: start, linked: base + start, len: end - start });
    }
    Ok(found)
}
