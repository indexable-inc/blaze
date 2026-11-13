//! The names of placed code, as a backtrace of the program finds them (Linux).
//!
//! The program's own standard library prints a backtrace by asking `dl_iterate_phdr` for the
//! images of the process, once, and reading each image's file for the symbol an address is
//! in (its `backtrace` crate: `.symtab`, then DWARF). Code the launcher placed is in no
//! image, so every frame of the program was `<unknown>`. Here the launcher is one more image:
//!
//! - The launcher keeps a table of what it placed ([`add`]: an offset in its code mapping, a
//!   length, a name; a push under a lock, since a start places hundreds).
//! - [`iterate`] is the `dl_iterate_phdr` the program is bound to (`records::own`): the
//!   system's, then one more entry, whose one segment is the launcher's whole code mapping
//!   and whose name is a file of this process, `/proc/self/fd/N`: a memfd written then and
//!   there, an ELF file of nothing but a symbol table, one function symbol a placed body.
//!
//! What this does not give, each said in launcher.md: a file and a line a frame (that is
//! DWARF's line table, not made); a frame of a body placed after the process printed its
//! first backtrace (the standard library asks for the images once and keeps the symbols it
//! parsed); macOS (its standard library asks dyld's image list and reads Mach-O).
//!
//! A body is never taken out: the area hands no offset out twice, so a freed body's range
//! names nothing else. A file is made again only when the table grew since the last was,
//! and the last is left open (a reader may have its path and not yet have opened it): a
//! program that asks for its images again and again leaves one descriptor a growth.

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::Mutex;

const SYMBOL: usize = 24;
const NAMES: &[u8] = b"\0.symtab\0.strtab\0.shstrtab\0\0\0\0\0\0";

#[cfg(target_arch = "aarch64")]
const MACHINE: u16 = 183;
#[cfg(target_arch = "x86_64")]
const MACHINE: u16 = 62;

struct Table {
    /// The code mapping: its first byte and its length.
    base: usize,
    len: usize,
    /// Each placed body: its offset in the mapping, its length, its name.
    bodies: Vec<(u64, u64, Box<str>)>,
    /// The file of the bodies there were when it was made: its descriptor, and how many.
    file: Option<(i32, usize)>,
}

/// One table a process: a process has one launcher. Locked for a push or for the making of
/// a file, by the control thread and by a program thread that asks for the images.
static TABLE: Mutex<Option<Table>> = Mutex::new(None);

/// One more placed body: `len` bytes at `offset` of the code mapping (`mapping` bytes at
/// `base`), called `name`.
pub fn add(base: usize, mapping: usize, offset: u64, len: u64, name: &str) {
    // A thread that panicked under the lock left the table whole: a push is one call.
    let mut table = TABLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // alloc: the launcher's table of names, for the life of the process
    let table = table.get_or_insert_with(|| Table { base, len: mapping, bodies: Vec::new(), file: None });
    table.bodies.push((offset, len, name.into()));
}

/// One section header.
fn section(name: u32, kind: u32, offset: u64, size: u64, link: u32, info: u32, entry: u64) -> [u8; 64] {
    let mut out = [0u8; 64];
    out[0..4].copy_from_slice(&name.to_le_bytes());
    out[4..8].copy_from_slice(&kind.to_le_bytes());
    out[24..32].copy_from_slice(&offset.to_le_bytes());
    out[32..40].copy_from_slice(&size.to_le_bytes());
    out[40..44].copy_from_slice(&link.to_le_bytes());
    out[44..48].copy_from_slice(&info.to_le_bytes());
    out[48..56].copy_from_slice(&1u64.to_le_bytes());
    out[56..64].copy_from_slice(&entry.to_le_bytes());
    out
}

/// The ELF file of `bodies`: a header, four section headers (null, `.symtab`, `.strtab`,
/// `.shstrtab`), then the three sections' bytes.
pub fn file(bodies: &[(u64, u64, Box<str>)]) -> Vec<u8> {
    let symbols = 64 + 4 * 64 + NAMES.len();
    let names = symbols + (bodies.len() + 1) * SYMBOL;
    let mut strings = vec![0u8]; // alloc: the file's bytes, once a file
    let mut table = vec![0u8; SYMBOL]; // alloc: as above; the first symbol is null
    for (offset, len, name) in bodies {
        let mut symbol = [0u8; SYMBOL];
        symbol[0..4].copy_from_slice(&(strings.len() as u32).to_le_bytes());
        symbol[4] = 0x12; // a global function
        symbol[6..8].copy_from_slice(&0xfff1u16.to_le_bytes()); // defined: SHN_ABS
        symbol[8..16].copy_from_slice(&offset.to_le_bytes());
        symbol[16..24].copy_from_slice(&len.to_le_bytes());
        table.extend_from_slice(&symbol);
        strings.extend_from_slice(name.as_bytes());
        strings.push(0);
    }
    let mut out = vec![0u8; 64]; // alloc: as above
    out[..8].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
    out[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
    out[18..20].copy_from_slice(&MACHINE.to_le_bytes());
    out[20..24].copy_from_slice(&1u32.to_le_bytes());
    out[40..48].copy_from_slice(&64u64.to_le_bytes()); // the section headers
    out[52..54].copy_from_slice(&64u16.to_le_bytes());
    out[54..56].copy_from_slice(&56u16.to_le_bytes());
    out[58..60].copy_from_slice(&64u16.to_le_bytes());
    out[60..62].copy_from_slice(&4u16.to_le_bytes());
    out[62..64].copy_from_slice(&3u16.to_le_bytes());
    out.extend_from_slice(&[0; 64]);
    out.extend_from_slice(&section(1, 2, symbols as u64, table.len() as u64, 2, 1, SYMBOL as u64));
    out.extend_from_slice(&section(9, 3, names as u64, strings.len() as u64, 0, 0, 0));
    out.extend_from_slice(&section(17, 3, (64 + 4 * 64) as u64, 27, 0, 0, 0));
    out.extend_from_slice(NAMES);
    out.extend_from_slice(&table);
    out.extend_from_slice(&strings);
    out
}

/// A memfd that holds `bytes`: its descriptor, left open for the life of the process.
fn kept(bytes: &[u8]) -> Option<i32> {
    // SAFETY: a name that is a C string; the result is checked.
    let fd = unsafe { libc::memfd_create(c"blaze-launch-symbols".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: a descriptor just made, owned by nothing else.
    let file = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut done = 0;
    while done < bytes.len() {
        // SAFETY: a write of a live slice to a descriptor this function owns.
        let wrote = unsafe { libc::write(file.as_raw_fd(), bytes[done..].as_ptr().cast(), bytes.len() - done) };
        if wrote <= 0 {
            return None;
        }
        done += wrote as usize;
    }
    Some(file.into_raw_fd())
}

/// `dl_iterate_phdr` as the program is bound to it: every image the system reports, then the
/// launcher's code mapping as one more, unless `callback` ended the walk before it. Where
/// the symbol file cannot be made (no descriptor is left), the walk ends as the system's did:
/// the program's frames are then `<unknown>`, as they are of any image whose file is gone.
///
/// # Safety
///
/// As `dl_iterate_phdr`: `callback` may be called with `data`.
pub unsafe extern "C" fn iterate(
    callback: Option<unsafe extern "C" fn(*mut libc::dl_phdr_info, libc::size_t, *mut libc::c_void) -> libc::c_int>,
    data: *mut libc::c_void,
) -> libc::c_int {
    // SAFETY: the caller's contract, handed on.
    let ended = unsafe { libc::dl_iterate_phdr(callback, data) };
    let (0, Some(callback)) = (ended, callback) else { return ended };
    let (base, len, fd) = {
        let mut table = TABLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(table) = table.as_mut() else { return 0 };
        let fd = match table.file {
            Some((fd, of)) if of == table.bodies.len() => fd,
            _ => {
                let Some(fd) = kept(&file(&table.bodies)) else { return 0 };
                table.file = Some((fd, table.bodies.len()));
                fd
            }
        };
        (table.base, table.len, fd)
    };
    // alloc: the image's name, for the length of the callback (which copies it)
    let Ok(name) = CString::new(format!("/proc/self/fd/{fd}")) else { return 0 };
    // SAFETY: zeroes are a valid `Elf64_Phdr` and a valid `dl_phdr_info`.
    let (mut segment, mut info): (libc::Elf64_Phdr, libc::dl_phdr_info) = unsafe { core::mem::zeroed() };
    segment.p_type = libc::PT_LOAD;
    segment.p_flags = libc::PF_R | libc::PF_X;
    segment.p_memsz = len as u64;
    segment.p_filesz = len as u64;
    info.dlpi_addr = base as libc::Elf64_Addr;
    info.dlpi_name = name.as_ptr();
    info.dlpi_phdr = &segment;
    info.dlpi_phnum = 1;
    // SAFETY: the caller's contract; `info` and what it points to live across the call.
    unsafe { callback(&mut info, core::mem::size_of::<libc::dl_phdr_info>(), data) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(bytes: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
    }

    /// The file is an ELF file whose `.symtab` holds each body with its name: read back as a
    /// reader of ELF does, from the section headers.
    #[test]
    fn the_symbol_file_names_each_body_at_its_offset() {
        let bodies = [(0x40u64, 0x20u64, Box::<str>::from("app::first")), (0x80, 0x10, "app::second".into())];
        let bytes = file(&bodies);
        assert_eq!(&bytes[..4], b"\x7fELF");
        let sections = word(&bytes, 40) as usize;
        let header = |index: usize| &bytes[sections + index * 64..sections + (index + 1) * 64];
        let (symtab, strtab) = (header(1), header(2));
        assert_eq!(u32::from_le_bytes(symtab[4..8].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(symtab[40..44].try_into().unwrap()), 2, "its strings are section 2");
        let (at, size) = (word(symtab, 24) as usize, word(symtab, 32) as usize);
        let strings = &bytes[word(strtab, 24) as usize..][..word(strtab, 32) as usize];
        assert_eq!(size, 3 * SYMBOL);
        assert_eq!(at + size, word(strtab, 24) as usize);
        assert_eq!(bytes.len(), at + size + strings.len());
        let mut read = Vec::new();
        for symbol in bytes[at..at + size].chunks(SYMBOL).skip(1) {
            let name = u32::from_le_bytes(symbol[0..4].try_into().unwrap()) as usize;
            let end = name + strings[name..].iter().position(|it| *it == 0).unwrap();
            read.push((word(symbol, 8), word(symbol, 16), std::str::from_utf8(&strings[name..end]).unwrap()));
        }
        assert_eq!(read, [(0x40, 0x20, "app::first"), (0x80, 0x10, "app::second")]);
    }
}
