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
#[cfg_attr(not(target_os = "macos"), expect(dead_code, reason = "macOS's reader; tested on every host"))]
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
#[cfg_attr(not(target_os = "linux"), expect(dead_code, reason = "Linux's reader; tested on every host"))]
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
}
