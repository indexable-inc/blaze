//! 64-bit Mach-O executables for arm64: the segments of the load commands.

#![forbid(unsafe_code)]

use super::{Found, Segment, range, u32_at, u64_at};
use crate::Error;

const MAGIC: u32 = 0xfeed_facf;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const CPU_SUBTYPE_ARM64E: u32 = 2;
const MH_EXECUTE: u32 = 2;
const LC_SEGMENT_64: u32 = 0x19;
const HEADER: u64 = 32;
const SEGMENT: u64 = 72;
const VM_PROT_WRITE: u32 = 2;
const VM_PROT_EXECUTE: u32 = 4;

fn bad(what: &str) -> Error {
    Error::new(format!("the executable is not a Mach-O image a run maps: {what}"))
}

/// The header and the load commands.
pub(super) fn headers_len(prefix: &[u8]) -> Option<u64> {
    Some(HEADER + u64::from(u32_at(prefix, 20)?))
}

pub(super) fn read(bytes: &[u8]) -> Result<Found, Error> {
    let word = |at: u64| u32_at(bytes, at).ok_or_else(|| bad("a short header"));
    if word(0)? != MAGIC {
        return Err(bad("no 64-bit Mach-O magic"));
    }
    if word(4)? != CPU_TYPE_ARM64 || word(8)? & 0xff == CPU_SUBTYPE_ARM64E {
        return Err(bad(&format!("CPU {:#x} subtype {:#x}, not arm64", word(4)?, word(8)?)));
    }
    if word(12)? != MH_EXECUTE {
        return Err(bad(&format!("file type {}, not MH_EXECUTE", word(12)?)));
    }
    let count = word(16)?;
    let commands = range(HEADER, u64::from(word(20)?), "the load commands")?;
    if commands.end > bytes.len() as u64 {
        return Err(bad(&format!("load commands to {:#x}, past the {:#x} bytes read", commands.end, bytes.len())));
    }
    #[allow(clippy::reversed_empty_ranges, reason = "the span starts empty, widened to each segment's start and end")]
    let mut found =
        Found { header: 0, span: u64::MAX..0, code: Vec::new(), reserved: Vec::new(), other: Vec::new() };
    let mut header = None;
    let mut at = HEADER;
    for _ in 0..count {
        let cmd = u32_at(bytes, at).ok_or_else(|| bad("a short load command"))?;
        let size = u64::from(u32_at(bytes, at + 4).ok_or_else(|| bad("a short load command"))?);
        if size < 8 || !size.is_multiple_of(8) || at.checked_add(size).is_none_or(|end| end > commands.end) {
            return Err(bad(&format!("a load command of {size} bytes at {at:#x}")));
        }
        if cmd == LC_SEGMENT_64 {
            if size < SEGMENT {
                return Err(bad(&format!("an LC_SEGMENT_64 of {size} bytes")));
            }
            let field = |offset: u64| u64_at(bytes, at + offset).ok_or_else(|| bad("a short segment"));
            let (vmaddr, vmsize, fileoff, filesize) = (field(24)?, field(32)?, field(40)?, field(48)?);
            let initprot = u32_at(bytes, at + 60).ok_or_else(|| bad("a short segment"))?;
            let memory = range(vmaddr, vmsize, "a segment")?;
            range(fileoff, filesize, "a segment's file bytes")?;
            let pagezero = vmaddr == 0 && initprot == 0 && filesize == 0;
            if !pagezero && vmsize != 0 {
                found.span.start = found.span.start.min(memory.start);
                found.span.end = found.span.end.max(memory.end);
                if fileoff == 0 && filesize != 0 && header.is_none() {
                    header = Some(vmaddr);
                }
                if initprot & VM_PROT_EXECUTE != 0 {
                    if initprot & VM_PROT_WRITE != 0 {
                        return Err(bad(&format!("the segment at {vmaddr:#x} is writable and executable")));
                    }
                    if filesize != vmsize {
                        return Err(bad(&format!(
                            "the code segment at {vmaddr:#x} has {filesize:#x} file bytes of {vmsize:#x}: code is all \
                             file bytes"
                        )));
                    }
                    found.code.push(Segment { offset: fileoff, linked: vmaddr, len: filesize });
                } else if initprot == 0 {
                    if filesize != 0 {
                        return Err(bad(&format!("the segment of no access at {vmaddr:#x} has file bytes")));
                    }
                    found.reserved.push(memory);
                } else {
                    found.other.push(memory);
                }
            }
        }
        at += size;
    }
    found.header = header.ok_or_else(|| bad("no segment maps file offset 0, the header"))?;
    if found.span.start > found.span.end {
        return Err(bad("no segment"));
    }
    Ok(found)
}
