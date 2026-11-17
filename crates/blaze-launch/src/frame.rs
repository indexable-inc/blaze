//! A placed function's unwind entry as bytes: its record's call frame information
//! ([`Unwind`]) made a CIE and an FDE at the function's address, as `.eh_frame` has them.
//!
//! Every address in it is absolute (`DW_EH_PE_absptr`): the function, its language-specific
//! data area and its personality routine, which is the routine's entry thunk. So the bytes
//! may lie anywhere, and where each address is in them is told ([`Entry`]): an image of
//! placed code ([`crate::image`]) holds the entry with the addresses of one process and says
//! which words to move for another.
//!
//! Nothing here knows an unwinder: the driver tells the system's of the entry
//! (`blaze-launch-rt`'s `unwind`).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::Allocator;

use crate::record::Unwind;

/// Where the parts of an entry [`entry`] wrote are, as offsets in its bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    /// The FDE: what Apple's unwinder is told of; an ELF unwinder is told of the whole.
    pub fde: usize,
    /// The 8 bytes that hold the function's address.
    pub start: usize,
    /// The 8 bytes that hold the personality routine's address, when the entry has one.
    pub personality: Option<usize>,
    /// The 8 bytes that hold the data area's address, when the entry has one.
    pub lsda: Option<usize>,
}

fn uleb<A: Allocator>(out: &mut Vec<u8, A>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        out.push(if value == 0 { byte } else { byte | 0x80 });
        if value == 0 {
            return;
        }
    }
}

fn sleb<A: Allocator>(out: &mut Vec<u8, A>, mut value: i64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
        out.push(if done { byte } else { byte | 0x80 });
        if done {
            return;
        }
    }
}

/// Ends the record that began at `start` of `out`: pads it with `DW_CFA_nop` to a multiple
/// of 8 bytes and writes its length.
fn finish<A: Allocator>(out: &mut Vec<u8, A>, start: usize) {
    while (out.len() - start) % 8 != 0 {
        out.push(0);
    }
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

/// `DW_EH_PE_absptr`: an address as it is, 8 bytes.
const ABSPTR: u8 = 0x00;

/// Clears `out` and writes the entry of a function at `start`, `len` bytes long: a CIE, its
/// FDE, and the zero length that ends a run. `personality` and `lsda` are the addresses of
/// the record's personality routine and data area. Gives where each part is.
pub fn entry<A: Allocator>(
    unwind: &Unwind<'_>,
    start: u64,
    len: u64,
    personality: Option<u64>,
    lsda: Option<u64>,
    out: &mut Vec<u8, A>,
) -> Result<Entry, String> {
    let Ok(return_address) = u8::try_from(unwind.return_address) else {
        // alloc: an error
        return Err(format!(
            "return address register {} is past the 255 a version 1 CIE holds",
            unwind.return_address
        ));
    };
    if lsda.is_some() && personality.is_none() {
        // alloc: an error
        return Err(String::from("a data area for a personality routine, and no routine"));
    }
    out.clear();
    // The CIE: its length (filled by `finish`), id 0, version 1.
    out.extend_from_slice(&[0; 8]);
    out.push(1);
    out.extend_from_slice(if personality.is_some() { b"zPLR\0" } else { b"zR\0" });
    uleb(out, u64::from(unwind.code_align));
    sleb(out, i64::from(unwind.data_align));
    out.push(return_address);
    let mut personality_at = None;
    match personality {
        Some(routine) => {
            // `P` (its encoding, the address), `L`, `R`.
            uleb(out, 11);
            out.push(ABSPTR);
            personality_at = Some(out.len());
            out.extend_from_slice(&routine.to_le_bytes());
            out.extend_from_slice(&[ABSPTR, ABSPTR]);
        }
        None => {
            uleb(out, 1);
            out.push(ABSPTR);
        }
    }
    out.extend_from_slice(unwind.cie);
    finish(out, 0);
    // The FDE: its length, then how far back its CIE begins, counted from this field.
    let fde = out.len();
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&((fde + 4) as u32).to_le_bytes());
    let start_at = out.len();
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    let mut lsda_at = None;
    match personality {
        Some(_) => {
            uleb(out, 8);
            // A routine with no data area has a zero there, which is no address.
            if lsda.is_some() {
                lsda_at = Some(out.len());
            }
            out.extend_from_slice(&lsda.unwrap_or(0).to_le_bytes());
        }
        None => uleb(out, 0),
    }
    out.extend_from_slice(unwind.fde);
    finish(out, fde);
    out.extend_from_slice(&[0; 4]);
    Ok(Entry { fde, start: start_at, personality: personality_at, lsda: lsda_at })
}
