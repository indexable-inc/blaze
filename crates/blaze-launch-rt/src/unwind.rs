//! A placed function's unwind entry: its record's call frame information made a CIE and an
//! FDE at the function's address, and told to the system's unwinder.
//!
//! The bytes are `.eh_frame`'s, with every address absolute (`DW_EH_PE_absptr`): the
//! function, its length, its language-specific data area and its personality routine, which
//! is the routine's entry thunk, so it too is compiled when the unwinder first calls it. A
//! panic then unwinds through code the launcher placed as through linked code, a lazily
//! bound body and a replaced one alike: an entry is registered before a word names its body
//! and taken back only when the body is freed, which is after no thread can be in it.
//!
//! How the unwinder is told (`__register_frame`, `__deregister_frame`): Apple's libunwind
//! takes the address of one FDE; libgcc's and LLVM's on ELF take a run of entries that a
//! zero length ends. Both symbols are in the library the launcher is linked against
//! (libSystem; libgcc_s), and `tests::every_macos_symbol_resolves` looks them up.

use blaze_launch::record::Unwind;

unsafe extern "C" {
    fn __register_frame(entry: *const u8);
    fn __deregister_frame(entry: *const u8);
}

fn uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        out.push(if value == 0 { byte } else { byte | 0x80 });
        if value == 0 {
            return;
        }
    }
}

fn sleb(out: &mut Vec<u8>, mut value: i64) {
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
fn finish(out: &mut Vec<u8>, start: usize) {
    while (out.len() - start) % 8 != 0 {
        out.push(0);
    }
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

/// `DW_EH_PE_absptr`: an address as it is, 8 bytes.
const ABSPTR: u8 = 0x00;

/// Clears `out` and writes the entry of a function at `start`, `len` bytes long: a CIE, its
/// FDE, and the zero length that ends a run. Gives the FDE's offset in `out`. `personality`
/// and `lsda` are the addresses of the record's personality routine and data area.
pub fn entry(
    unwind: &Unwind<'_>,
    start: u64,
    len: u64,
    personality: Option<u64>,
    lsda: Option<u64>,
    out: &mut Vec<u8>,
) -> Result<usize, String> {
    let Ok(return_address) = u8::try_from(unwind.return_address) else {
        // alloc: an error
        return Err(format!(
            "return address register {} is past the 255 a version 1 CIE holds",
            unwind.return_address
        ));
    };
    if lsda.is_some() && personality.is_none() {
        // alloc: an error
        return Err("a data area for a personality routine, and no routine".to_owned());
    }
    out.clear();
    // The CIE: its length (filled by `finish`), id 0, version 1.
    out.extend_from_slice(&[0; 8]);
    out.push(1);
    out.extend_from_slice(if personality.is_some() { b"zPLR\0" } else { b"zR\0" });
    uleb(out, u64::from(unwind.code_align));
    sleb(out, i64::from(unwind.data_align));
    out.push(return_address);
    match personality {
        Some(routine) => {
            // `P` (its encoding, the address), `L`, `R`.
            uleb(out, 11);
            out.push(ABSPTR);
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
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    match personality {
        Some(_) => {
            uleb(out, 8);
            out.extend_from_slice(&lsda.unwrap_or(0).to_le_bytes());
        }
        None => uleb(out, 0),
    }
    out.extend_from_slice(unwind.fde);
    finish(out, fde);
    out.extend_from_slice(&[0; 4]);
    Ok(fde)
}

/// What [`register`] takes of an entry at `at` whose FDE is `fde` bytes in.
fn handle(at: usize, fde: usize) -> *const u8 {
    // Apple's libunwind is told of one FDE; the ELF unwinders of a run of entries.
    #[cfg(target_os = "macos")]
    let entry = at + fde;
    #[cfg(not(target_os = "macos"))]
    let entry = {
        let _ = fde;
        at
    };
    entry as *const u8
}

/// Tells the unwinder of the entry at address `at` ([`entry`]'s bytes, copied there), whose
/// FDE is `fde` bytes in.
///
/// # Safety
///
/// `at` is the address of the bytes [`entry`] wrote, in memory that stays as it is until
/// [`deregister`] is called for the same entry; the function it describes is placed at the
/// address the entry says.
pub unsafe fn register(at: usize, fde: usize) {
    // SAFETY: the caller's contract: a whole, ended run of entries that outlives its
    // registration.
    unsafe { __register_frame(handle(at, fde)) }
}

/// Takes back what [`register`] told.
///
/// # Safety
///
/// `at` and `fde` are those of an entry [`register`] was called with, once, and no thread
/// is, or will be, in the function it describes.
pub unsafe fn deregister(at: usize, fde: usize) {
    // SAFETY: the caller's contract: registered, and deregistered once.
    unsafe { __deregister_frame(handle(at, fde)) }
}
