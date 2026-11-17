//! The running image's linker id, read in place from its mapped header: Mach-O's `LC_UUID` on
//! macOS, the GNU build-id note on Linux. No file is opened and nothing is hashed: the linker
//! wrote the id from the image's content, and the loader mapped it with the image.
//!
//! - macOS: `_dyld_get_image_header(0)` (mach-o/dyld.h; public dyld API, Darwin-only, rule 45):
//!   image 0 is the main executable, its header and load commands mapped for the process's
//!   life. Chosen over reading the binary's file: no open, no path, the image actually running.
//! - Linux: `dl_iterate_phdr` (glibc, link.h): its first callback is the main program, whose
//!   `PT_NOTE` segments are mapped with it. A binary linked without `--build-id` has none.

use crate::host::BuildId;

/// A little-endian-or-native `u32` at `at`, when `bytes` holds one there.
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let word = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_ne_bytes([word[0], word[1], word[2], word[3]]))
}

/// The id in a run of Mach-O load commands: `LC_UUID`'s 16 bytes.
fn uuid_in(commands: &[u8], ncmds: u32) -> Option<BuildId> {
    const LC_UUID: u32 = 0x1b;
    let mut at = 0usize;
    for _ in 0..ncmds {
        let cmd = u32_at(commands, at)?;
        let size = u32_at(commands, at.checked_add(4)?)? as usize;
        if cmd == LC_UUID {
            // `struct uuid_command`: cmd, cmdsize, then uuid[16].
            return BuildId::of(commands.get(at.checked_add(8)?..at.checked_add(24)?)?);
        }
        if size == 0 {
            return None;
        }
        at = at.checked_add(size)?;
    }
    None
}

/// The id in a run of ELF notes: `NT_GNU_BUILD_ID`'s descriptor, named `GNU`.
fn note_in(notes: &[u8]) -> Option<BuildId> {
    const NT_GNU_BUILD_ID: u32 = 3;
    let mut at = 0usize;
    while let (Some(namesz), Some(descsz), Some(kind)) = (u32_at(notes, at), u32_at(notes, at + 4), u32_at(notes, at + 8)) {
        let name = at.checked_add(12)?;
        let desc = name.checked_add((namesz as usize).next_multiple_of(4))?;
        if kind == NT_GNU_BUILD_ID && notes.get(name..name.checked_add(namesz as usize)?) == Some(b"GNU\0".as_slice()) {
            return BuildId::of(notes.get(desc..desc.checked_add(descsz as usize)?)?);
        }
        at = desc.checked_add((descsz as usize).next_multiple_of(4))?;
    }
    None
}

/// A little-endian `u16` at `at`, when `bytes` holds one there.
fn le_u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let word = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([word[0], word[1]]))
}

/// A little-endian `u64` at `at`, when `bytes` holds one there, as an offset or a length.
fn le_usize_at(bytes: &[u8], at: usize) -> Option<usize> {
    let word = bytes.get(at..at.checked_add(8)?)?;
    usize::try_from(u64::from_le_bytes([word[0], word[1], word[2], word[3], word[4], word[5], word[6], word[7]])).ok()
}

/// The linker id of ANOTHER program, from the first bytes of its file (the roster reads 64 KiB,
/// blaze-cli roster.rs): the format chosen by its magic, then the same readers as the running
/// image's. A 64-bit little-endian Mach-O (`MH_MAGIC_64`) gives its `LC_UUID`; a 64-bit
/// little-endian ELF gives the `NT_GNU_BUILD_ID` note of its first `PT_NOTE` segment that holds
/// one. Everything else is `None`, by form: a universal (fat) Mach-O, a 32-bit or big-endian
/// image, a script, an id lying past `file`'s end. The two readers take the host's byte order,
/// so a big-endian host reads no id at all.
///
/// TO VERIFY BY A TEST (`own_file_carries_the_running_image_id`): the linkers blaze is built
/// with put the load commands (Mach-O) and the build-id note (ELF, `.note.gnu.build-id`, in the
/// first page) inside a file's first 64 KiB.
#[blaze::nonblocking]
pub fn build_id_in(file: &[u8]) -> Option<BuildId> {
    const MH_MAGIC_64: [u8; 4] = 0xfeed_facf_u32.to_le_bytes();
    const ELF_MAGIC: [u8; 4] = *b"\x7fELF";
    /// `struct mach_header_64`'s size: the load commands follow it.
    const MACH_HEADER_64: usize = 32;
    /// `Elf64_Phdr`'s size.
    const ELF64_PHDR: usize = 56;
    const PT_NOTE: u32 = 4;
    if cfg!(target_endian = "big") {
        return None;
    }
    let magic = file.get(..4)?;
    if magic == MH_MAGIC_64 {
        let ncmds = u32_at(file, 16)?;
        let sizeofcmds = u32_at(file, 20)? as usize;
        let commands = file.get(MACH_HEADER_64..MACH_HEADER_64.checked_add(sizeofcmds)?)?;
        return uuid_in(commands, ncmds);
    }
    if magic != ELF_MAGIC {
        return None;
    }
    // e_ident: EI_CLASS 2 (64-bit), EI_DATA 1 (little-endian).
    if file.get(4..6)? != [2, 1] {
        return None;
    }
    let phoff = le_usize_at(file, 0x20)?;
    let phentsize = usize::from(le_u16_at(file, 0x36)?);
    let phnum = usize::from(le_u16_at(file, 0x38)?);
    if phentsize < ELF64_PHDR {
        return None;
    }
    for index in 0..phnum {
        let header = phoff.checked_add(index.checked_mul(phentsize)?)?;
        if u32_at(file, header)? != PT_NOTE {
            continue;
        }
        let offset = le_usize_at(file, header.checked_add(8)?)?;
        let size = le_usize_at(file, header.checked_add(32)?)?;
        let Some(notes) = file.get(offset..offset.checked_add(size)?) else { continue };
        if let Some(id) = note_in(notes) {
            return Some(id);
        }
    }
    None
}

/// The running program's own file, as the kernel names it, written into `out`: its length,
/// or `None` when the host cannot say or `out` is too short. macOS: `_NSGetExecutablePath`
/// (mach-o/dyld.h; the path the program was started by, symlinks left as they are). Linux:
/// `readlink("/proc/self/exe")` (a file replaced since the start reads with " (deleted)"
/// appended: the roster then finds no file there, which is the truth).
#[cfg(target_os = "macos")]
pub(crate) fn exe_path(out: &mut [u8]) -> Option<usize> {
    use core::ffi::{c_char, c_int};
    unsafe extern "C" {
        fn _NSGetExecutablePath(buf: *mut c_char, bufsize: *mut u32) -> c_int;
    }
    let mut size = u32::try_from(out.len()).ok()?;
    // SAFETY: `out` is writable for `size` bytes; dyld writes at most that many and, on success
    // (0), a NUL-terminated path; on -1 it writes nothing and sets the size it needs.
    let status = unsafe { _NSGetExecutablePath(out.as_mut_ptr().cast(), &raw mut size) };
    if status != 0 {
        return None;
    }
    out.iter().position(|byte| *byte == 0)
}

#[cfg(target_os = "linux")]
pub(crate) fn exe_path(out: &mut [u8]) -> Option<usize> {
    use core::ffi::c_char;
    unsafe extern "C" {
        fn readlink(path: *const c_char, buf: *mut c_char, len: usize) -> isize;
    }
    // SAFETY: the path is a NUL-terminated literal; `out` is writable for `out.len()` bytes and
    // readlink writes at most that many (no NUL), returning the count or -1.
    let written = unsafe { readlink(c"/proc/self/exe".as_ptr(), out.as_mut_ptr().cast(), out.len()) };
    let written = usize::try_from(written).ok()?;
    // A full buffer may be a cut path: no answer rather than a wrong one.
    (written < out.len()).then_some(written)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn exe_path(_out: &mut [u8]) -> Option<usize> {
    None
}

#[cfg(target_os = "macos")]
pub(crate) fn build_id() -> Option<BuildId> {
    /// `struct mach_header_64` (mach-o/loader.h): its load commands follow it.
    #[repr(C)]
    struct MachHeader64 {
        magic: u32,
        cputype: i32,
        cpusubtype: i32,
        filetype: u32,
        ncmds: u32,
        sizeofcmds: u32,
        flags: u32,
        reserved: u32,
    }
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    unsafe extern "C" {
        fn _dyld_get_image_header(image_index: u32) -> *const MachHeader64;
    }
    // SAFETY: a dyld call with no preconditions; index 0 names the main executable.
    let header = unsafe { _dyld_get_image_header(0) };
    if header.is_null() {
        return None;
    }
    // SAFETY: dyld returned the main image's mapped header, aligned and live for the process.
    let head = unsafe { &*header };
    if head.magic != MH_MAGIC_64 {
        return None;
    }
    let start = header.cast::<u8>().wrapping_add(size_of::<MachHeader64>());
    // SAFETY: the load commands follow the header in the same read-only mapping, `sizeofcmds`
    // bytes (mach-o/loader.h), live for the process.
    let commands = unsafe { core::slice::from_raw_parts(start, head.sizeofcmds as usize) };
    uuid_in(commands, head.ncmds)
}

#[cfg(target_os = "linux")]
pub(crate) fn build_id() -> Option<BuildId> {
    use core::ffi::{c_int, c_void};

    /// `struct dl_phdr_info` (link.h), its first four fields.
    #[repr(C)]
    struct DlPhdrInfo {
        dlpi_addr: usize,
        dlpi_name: *const u8,
        dlpi_phdr: *const Elf64Phdr,
        dlpi_phnum: u16,
    }
    /// `Elf64_Phdr` (elf.h).
    #[repr(C)]
    struct Elf64Phdr {
        p_type: u32,
        p_flags: u32,
        p_offset: u64,
        p_vaddr: u64,
        p_paddr: u64,
        p_filesz: u64,
        p_memsz: u64,
        p_align: u64,
    }
    const PT_NOTE: u32 = 4;
    unsafe extern "C" {
        fn dl_iterate_phdr(
            callback: unsafe extern "C" fn(info: *mut DlPhdrInfo, size: usize, data: *mut c_void) -> c_int,
            data: *mut c_void,
        ) -> c_int;
    }
    unsafe extern "C" fn first(info: *mut DlPhdrInfo, _size: usize, data: *mut c_void) -> c_int {
        // SAFETY: `data` is the `Option<BuildId>` below, alive across the call.
        let out = unsafe { &mut *data.cast::<Option<BuildId>>() };
        // SAFETY: glibc hands a valid `dl_phdr_info` for the callback's duration.
        let info = unsafe { &*info };
        // SAFETY: `dlpi_phdr` points at `dlpi_phnum` program headers, mapped with the image.
        let headers = unsafe { core::slice::from_raw_parts(info.dlpi_phdr, usize::from(info.dlpi_phnum)) };
        for header in headers.iter().filter(|h| h.p_type == PT_NOTE) {
            let start = (info.dlpi_addr as *const u8).wrapping_add(header.p_vaddr as usize);
            // SAFETY: a PT_NOTE segment is loaded at `dlpi_addr + p_vaddr`, `p_memsz` bytes.
            let notes = unsafe { core::slice::from_raw_parts(start, header.p_memsz as usize) };
            if let Some(id) = note_in(notes) {
                *out = Some(id);
                return 1;
            }
        }
        // The main program came first and had no build id: stop; `None` stands.
        1
    }
    let mut out: Option<BuildId> = None;
    // SAFETY: `first` has the callback's C signature, and `out` outlives the call.
    unsafe { dl_iterate_phdr(first, (&raw mut out).cast()) };
    out
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn build_id() -> Option<BuildId> {
    None
}

#[cfg(test)]
mod tests {
    /// The linker stamped this test binary, and the reader finds it at this OS's length: 16
    /// bytes (a UUID) on macOS, 20 (`--build-id=sha1`, the GNU default) on Linux.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_running_image_has_its_linker_id() {
        let id = super::build_id().expect("the linker wrote no id into this test binary");
        let want = if cfg!(target_os = "macos") { 16 } else { 20 };
        assert_eq!(id.as_bytes().len(), want);
        assert!(id.as_bytes().iter().any(|b| *b != 0), "an empty id");
    }

    /// Each reader on bytes laid out as its format says, on every host.
    #[test]
    fn each_reader_finds_its_id_in_its_format() {
        let mut commands = alloc::vec::Vec::new();
        for word in [0x19u32, 16, 0, 0, 0x1b, 24] {
            commands.extend_from_slice(&word.to_ne_bytes());
        }
        commands.extend_from_slice(&[7; 16]);
        assert_eq!(super::uuid_in(&commands, 2).map(|it| it.as_bytes().to_vec()), Some(alloc::vec![7; 16]));
        assert_eq!(super::uuid_in(&commands[..20], 2), None, "a cut command is no id");
        let mut notes = alloc::vec::Vec::new();
        for word in [4u32, 20, 3] {
            notes.extend_from_slice(&word.to_ne_bytes());
        }
        notes.extend_from_slice(b"GNU\0");
        notes.extend_from_slice(&[9; 20]);
        assert_eq!(super::note_in(&notes).map(|it| it.as_bytes().to_vec()), Some(alloc::vec![9; 20]));
    }

    /// A file's id by its magic: a Mach-O header then `LC_UUID`; an ELF header, one program
    /// header naming a `PT_NOTE` at offset 120, the GNU note there. Passes when both give their
    /// ids, a cut ELF note and a fat Mach-O magic give none.
    #[test]
    fn a_file_gives_its_id_by_its_magic() {
        let mut macho = alloc::vec::Vec::new();
        // magic, cputype, cpusubtype, filetype, ncmds 1, sizeofcmds 24, flags, reserved.
        for word in [0xfeed_facfu32, 0x0100_000c, 0, 2, 1, 24, 0, 0, 0x1b, 24] {
            macho.extend_from_slice(&word.to_le_bytes());
        }
        macho.extend_from_slice(&[7; 16]);
        assert_eq!(super::build_id_in(&macho).map(|it| it.as_bytes().to_vec()), Some(alloc::vec![7; 16]));
        let mut fat = macho.clone();
        fat[..4].copy_from_slice(&0xcafe_babeu32.to_be_bytes());
        assert_eq!(super::build_id_in(&fat), None, "a universal binary is not read");

        let mut elf = alloc::vec![0u8; 64];
        elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[0x20..0x28].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        elf[0x36..0x38].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        elf[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        let mut phdr = alloc::vec![0u8; 56];
        phdr[..4].copy_from_slice(&4u32.to_le_bytes()); // PT_NOTE
        phdr[8..16].copy_from_slice(&120u64.to_le_bytes()); // p_offset
        phdr[32..40].copy_from_slice(&36u64.to_le_bytes()); // p_filesz
        elf.extend_from_slice(&phdr);
        for word in [4u32, 20, 3] {
            elf.extend_from_slice(&word.to_le_bytes());
        }
        elf.extend_from_slice(b"GNU\0");
        elf.extend_from_slice(&[9; 20]);
        assert_eq!(super::build_id_in(&elf).map(|it| it.as_bytes().to_vec()), Some(alloc::vec![9; 20]));
        assert_eq!(super::build_id_in(&elf[..150]), None, "a note past the bytes read is no id");
    }

    /// TO VERIFY BY A TEST: this test binary's own file, found by `exe_path` and read for its
    /// first 64 KiB, carries the id the loader mapped (`build_id`). Passes when they are equal.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn own_file_carries_the_running_image_id() {
        use std::io::Read as _;
        let mut path = [0u8; 4096];
        let len = super::exe_path(&mut path).expect("the kernel names this test binary's file");
        let path = core::str::from_utf8(&path[..len]).expect("a UTF-8 path");
        let mut head = alloc::vec::Vec::new();
        std::fs::File::open(path)
            .expect("this test binary's file opens")
            .take(64 << 10)
            .read_to_end(&mut head)
            .expect("its first 64 KiB read");
        assert_eq!(super::build_id_in(&head), super::build_id(), "the file's id and the running image's");
        assert!(super::build_id_in(&head).is_some(), "the id lies in the first 64 KiB");
    }
}
