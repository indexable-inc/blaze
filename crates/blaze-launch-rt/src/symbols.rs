//! The names of placed code, as a backtrace of the program finds them (Linux).
//!
//! The program's own standard library prints a backtrace by asking `dl_iterate_phdr` for the
//! images of the process, once, and reading each image's file for the symbol an address is
//! in (its `backtrace` crate: `.symtab`, then DWARF). Code the launcher placed is in no
//! image, so every frame of the program was `<unknown>`. Here the launcher is one more image:
//!
//! - [`iterate`] is the `dl_iterate_phdr` the program is bound to (`records::own`): the
//!   system's, then one more entry, whose one segment is the launcher's whole code mapping
//!   and whose name is a file of this process, `/proc/self/fd/N`.
//! - That file ([`Symbols`], a memfd) is an ELF file of nothing but a symbol table: one
//!   function symbol a placed body, its value the body's offset in the mapping. A body is
//!   added when it is placed. None is taken out: the area never hands an offset out twice,
//!   so a freed body's range names nothing else.
//!
//! What this does not give, each said in launcher.md: a file and a line a frame (that is
//! DWARF's line table, not made); a frame of a body placed after the process printed its
//! first backtrace (the standard library keeps the symbols it parsed for an image); macOS
//! (its standard library asks dyld's image list and reads Mach-O).
//!
//! The file's layout, fixed so that adding a symbol is four small writes and no move:
//!
//! ```text
//! 0     the ELF header
//! 64    four section headers: null, .symtab, .strtab, .shstrtab
//! 320   .shstrtab's bytes
//! 4096  .symtab: 24 bytes a symbol, the first one null; room for 2^21
//! 64 MiB .strtab
//! ```
//!
//! A reader maps the file while the control thread appends. An append writes the name, the
//! symbol, then `.strtab`'s length, then `.symtab`'s: a reader that sees a symbol sees its
//! name.

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicPtr, Ordering};

const SECTIONS: u64 = 64;
const SHSTRTAB: u64 = 320;
const SYMTAB: u64 = 4096;
const SYMBOL: u64 = 24;
/// The most symbols the table has room for before `.strtab` begins.
const MOST: u64 = 1 << 21;
const STRTAB: u64 = 64 << 20;
const NAMES: &[u8] = b"\0.symtab\0.strtab\0.shstrtab\0";

#[cfg(target_arch = "aarch64")]
const MACHINE: u16 = 183;
#[cfg(target_arch = "x86_64")]
const MACHINE: u16 = 62;

/// The image [`iterate`] adds: the code mapping, and the file that names what is in it.
struct Image {
    base: usize,
    segment: libc::Elf64_Phdr,
    name: CString,
}

/// Set once, by [`Symbols::new`], and never freed: the program may print a backtrace for as
/// long as it runs.
static IMAGE: AtomicPtr<Image> = AtomicPtr::new(core::ptr::null_mut());

/// The symbol file of one launcher. See the module's documentation.
pub struct Symbols {
    file: OwnedFd,
    /// Symbols written, the null one included.
    symbols: u64,
    /// Bytes of `.strtab` written.
    names: u64,
}

fn write(file: &OwnedFd, at: u64, bytes: &[u8]) -> Result<(), String> {
    let mut done = 0;
    while done < bytes.len() {
        // SAFETY: a write of a live slice to a descriptor this value owns, at an offset.
        let wrote = unsafe {
            libc::pwrite(
                file.as_raw_fd(),
                bytes[done..].as_ptr().cast(),
                bytes.len() - done,
                (at + done as u64) as libc::off_t,
            )
        };
        if wrote <= 0 {
            // alloc: an error
            return Err(format!("writing the launcher's symbol file failed: {}", std::io::Error::last_os_error()));
        }
        done += wrote as usize;
    }
    Ok(())
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

impl Symbols {
    /// The symbol file of the launcher whose code mapping is `len` bytes at `base`, empty,
    /// and the image [`iterate`] reports from now on. Once a process.
    pub fn new(base: usize, len: usize) -> Result<Symbols, String> {
        // SAFETY: a name that is a C string; the result is checked. Not close-on-exec: the
        // file is the process's, as an image's is, though nothing reads it after an `exec`.
        let fd = unsafe { libc::memfd_create(c"blaze-launch-symbols".as_ptr(), 0) };
        if fd < 0 {
            // alloc: an error
            return Err(format!("memfd_create for the launcher's symbol file failed: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: a descriptor just made, owned by nothing else.
        let file = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut header = [0u8; 64];
        header[..8].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
        header[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        header[18..20].copy_from_slice(&MACHINE.to_le_bytes());
        header[20..24].copy_from_slice(&1u32.to_le_bytes());
        header[40..48].copy_from_slice(&SECTIONS.to_le_bytes());
        header[52..54].copy_from_slice(&64u16.to_le_bytes());
        header[54..56].copy_from_slice(&56u16.to_le_bytes());
        header[58..60].copy_from_slice(&64u16.to_le_bytes());
        header[60..62].copy_from_slice(&4u16.to_le_bytes());
        header[62..64].copy_from_slice(&3u16.to_le_bytes());
        write(&file, 0, &header)?;
        write(&file, SHSTRTAB, NAMES)?;
        write(&file, SECTIONS + 192, &section(17, 3, SHSTRTAB, NAMES.len() as u64, 0, 0, 0))?;
        // The null symbol and the null name.
        write(&file, SYMTAB, &[0; SYMBOL as usize])?;
        write(&file, STRTAB, &[0])?;
        let symbols = Symbols { file, symbols: 1, names: 1 };
        symbols.lengths()?;
        // SAFETY: zeroes are a valid `Elf64_Phdr`.
        let mut segment: libc::Elf64_Phdr = unsafe { core::mem::zeroed() };
        segment.p_type = libc::PT_LOAD;
        segment.p_flags = libc::PF_R | libc::PF_X;
        segment.p_memsz = len as u64;
        segment.p_filesz = len as u64;
        let name = CString::new(format!("/proc/self/fd/{}", symbols.file.as_raw_fd()))
            .map_err(|_| "the symbol file's path has a NUL in it".to_owned())?;
        // alloc: the image's description, leaked: read for as long as the process runs
        let image = Box::into_raw(Box::new(Image { base, segment, name }));
        if IMAGE.compare_exchange(core::ptr::null_mut(), image, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            // SAFETY: the box made above, which nothing else was given.
            drop(unsafe { Box::from_raw(image) });
            return Err("a second symbol file in one process: a process has one launcher".to_owned());
        }
        Ok(symbols)
    }

    /// Writes the two tables' lengths: `.strtab`'s, then `.symtab`'s.
    fn lengths(&self) -> Result<(), String> {
        write(&self.file, SECTIONS + 128, &section(9, 3, STRTAB, self.names, 0, 0, 0))?;
        write(&self.file, SECTIONS + 64, &section(1, 2, SYMTAB, self.symbols * SYMBOL, 2, 1, SYMBOL))
    }

    /// One more function: `len` bytes at `offset` of the code mapping, called `name`.
    pub fn add(&mut self, offset: u64, len: u64, name: &str) -> Result<(), String> {
        if self.symbols == MOST {
            // alloc: an error
            return Err(format!("the launcher's symbol file is full at {MOST} functions"));
        }
        write(&self.file, STRTAB + self.names, name.as_bytes())?;
        write(&self.file, STRTAB + self.names + name.len() as u64, &[0])?;
        let mut symbol = [0u8; SYMBOL as usize];
        symbol[0..4].copy_from_slice(&(self.names as u32).to_le_bytes());
        symbol[4] = 0x12; // a global function
        symbol[6..8].copy_from_slice(&0xfff1u16.to_le_bytes()); // defined: SHN_ABS
        symbol[8..16].copy_from_slice(&offset.to_le_bytes());
        symbol[16..24].copy_from_slice(&len.to_le_bytes());
        write(&self.file, SYMTAB + self.symbols * SYMBOL, &symbol)?;
        self.names += name.len() as u64 + 1;
        self.symbols += 1;
        self.lengths()
    }
}

/// `dl_iterate_phdr` as the program is bound to it: every image the system reports, then the
/// launcher's code mapping as one more, unless `callback` ended the walk before it.
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
    let image = IMAGE.load(Ordering::SeqCst);
    let (0, Some(callback), false) = (ended, callback, image.is_null()) else { return ended };
    // SAFETY: set once to a leaked box and never freed.
    let image = unsafe { &*image };
    // SAFETY: zeroes are a valid `dl_phdr_info`.
    let mut info: libc::dl_phdr_info = unsafe { core::mem::zeroed() };
    info.dlpi_addr = image.base as libc::Elf64_Addr;
    info.dlpi_name = image.name.as_ptr();
    info.dlpi_phdr = &image.segment;
    info.dlpi_phnum = 1;
    // SAFETY: the caller's contract; `info` and what it points to live across the call.
    unsafe { callback(&mut info, core::mem::size_of::<libc::dl_phdr_info>(), data) }
}
