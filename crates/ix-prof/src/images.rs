//! The code images loaded in this process, so a raw address kept with a sample (or a panic's
//! stack) can be named again later, by another process, from the file it came from: each
//! image's path, its build id read from the loaded image in memory (ELF's GNU build id note,
//! Mach-O's `LC_UUID`), where its code is ([`Image::base`], [`Image::size`]) and how far it was
//! slid from its file's addresses ([`Image::slide`]), and the BLAKE3 of its file ([`blake3`]).
//!
//! To name an address `pc` later: find the image with `base <= pc < base + size` (of the same
//! process and boot), check that a file's build id or BLAKE3 is the image's, and look `pc -
//! slide` up in that file's symbols and DWARF (a return address at `pc - slide - 1`).
//!
//! - Linux: `dl_iterate_phdr`: each object's name and load bias, its executable `PT_LOAD`
//!   segments, its `PT_NOTE`s read in place. The executable's file is `/proc/self/exe`, which
//!   is the inode running even after a rebuild replaced the path.
//! - macOS: dyld's image list: each image's header, its `__TEXT` segment and `LC_UUID` read in
//!   place, and its slide. A system library in the dyld shared cache has no file on disk.

use std::{collections::BTreeMap, sync::OnceLock};

use blaze_core::lock::Lock;

/// An image loaded in this process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    /// The file it was loaded from, as the loader names it (the executable's path for it).
    pub path: String,
    /// Its GNU build id or Mach-O UUID, in lowercase hex, read from the image in memory.
    pub build_id: Option<String>,
    /// The address its code starts at: its lowest executable segment (ELF), `__TEXT` (Mach-O).
    pub base: u64,
    /// Bytes of code from `base`.
    pub size: u64,
    /// How far it was loaded from its file's addresses: an address less this is the file's.
    pub slide: u64,
    /// The executable itself, not a library.
    pub main: bool,
}

impl Image {
    pub fn contains(&self, pc: u64) -> bool {
        pc >= self.base && pc - self.base < self.size
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    // alloc: an image's id, once an image
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a `String` does not fail.
        _ = write!(out, "{byte:02x}");
    }
    out
}

/// The images loaded now, by base. A library loaded or unloaded while this runs may be missed.
pub fn loaded() -> Vec<Image> {
    let mut out = platform::loaded();
    out.sort_unstable_by_key(|it| it.base);
    out
}

/// The machine's boot this process runs in: Linux's `boot_id`, macOS's `kern.bootsessionuuid`;
/// `None` where neither can be read. With a process id it names one process of the machine's
/// life.
pub fn boot() -> Option<&'static str> {
    static BOOT: OnceLock<Option<String>> = OnceLock::new();
    BOOT.get_or_init(platform::boot).as_deref()
}

/// The BLAKE3 of `image`'s file, in hex, hashed once a process an image: or why it can't be,
/// as the image's `unhashed` says it: no file, the file unreadable, or a file at its path that
/// is not the image loaded (its build id differs: replaced since).
pub fn blake3(image: &Image) -> Result<String, String> {
    // alloc: once a process
    static HASHED: Lock<BTreeMap<(u64, String), Result<String, String>>> =
        Lock::new("ixprof.images.hashed", BTreeMap::new());
    let key = (image.base, image.path.clone());
    if let Some(it) = HASHED.lock().get(&key) {
        return it.clone();
    }
    let hashed = platform::hash(image);
    HASHED.lock().insert(key, hashed.clone());
    hashed
}

/// Hashes the file at `path` (opened as `open`), checking its build id against `image`'s when
/// the image has one. The file is mapped, hashed and unmapped: never read into the heap.
#[cfg(unix)]
fn hash_file(image: &Image, open: &std::path::Path) -> Result<String, String> {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open(open)
        .map_err(|error| format!("opening {}: {error}", open.display()))?;
    let len = file
        .metadata()
        .map_err(|error| format!("reading the size of {}: {error}", open.display()))?
        .len();
    let len = usize::try_from(len)
        .map_err(|_| format!("{} is past the address space", open.display()))?;
    if len == 0 {
        return Err(format!("{} is empty", open.display()));
    }
    // SAFETY: a new read-only private mapping of `len` bytes of a file opened above, unmapped
    // below before anything it lends out outlives it.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return Err(format!("mapping {}: {}", open.display(), std::io::Error::last_os_error()));
    }
    // SAFETY: the mapping is `len` readable bytes until the `munmap` below, and `bytes` is not
    // used past it. A file written while mapped could change under the hash; an executable
    // being run is not written in place (Linux refuses, `ETXTBSY`).
    let bytes = unsafe { std::slice::from_raw_parts(at.cast::<u8>(), len) };
    let checked = match &image.build_id {
        Some(want) => match file_build_id(bytes) {
            Some(got) if got == *want => Ok(()),
            Some(got) => Err(format!(
                "the file at {} is not the image loaded: its build id is {got}, the image's {want} (replaced since it was loaded)",
                open.display()
            )),
            None => Err(format!(
                "the file at {} has no build id, the image loaded has {want}",
                open.display()
            )),
        },
        None => Ok(()),
    };
    let hash = checked.map(|()| blake3::hash(bytes).to_hex().to_string());
    // SAFETY: the mapping made above, of `len`, unmapped once; `bytes` is dead.
    unsafe { libc::munmap(at, len) };
    hash
}

/// The build id or UUID of an object file's bytes, in hex.
#[cfg(unix)]
fn file_build_id(bytes: &[u8]) -> Option<String> {
    use object::Object;
    let object = object::File::parse(bytes).ok()?;
    match object.build_id() {
        Ok(Some(id)) => Some(hex(id)),
        _ => object.mach_uuid().ok().flatten().map(|it| hex(&it)),
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{Image, hex};

    const PT_LOAD: u32 = 1;
    const PT_NOTE: u32 = 4;
    const PF_X: u32 = 1;
    const NT_GNU_BUILD_ID: u32 = 3;

    pub(super) fn loaded() -> Vec<Image> {
        unsafe extern "C" fn each(
            info: *mut libc::dl_phdr_info,
            _size: libc::size_t,
            out: *mut libc::c_void,
        ) -> libc::c_int {
            // SAFETY: the loader gives a valid entry for the length of the call, and `out` is
            // the `Vec<Image>` `loaded` passed, alive and not otherwise borrowed for the call.
            let (info, out) = unsafe { (&*info, &mut *out.cast::<Vec<Image>>()) };
            if let Some(image) = image(info, out.is_empty()) {
                out.push(image);
            }
            0
        }
        // alloc: a listing of images, once a flush with samples
        let mut out: Vec<Image> = Vec::new();
        // SAFETY: `each` writes only through the pointer it is given, which is `out`'s, alive
        // for the call; it allocates (which glibc's loader allows in the callback) and calls
        // no loader function.
        unsafe { libc::dl_iterate_phdr(Some(each), std::ptr::from_mut(&mut out).cast()) };
        out
    }

    /// The image `info` describes; `first` for the first the loader lists, the executable.
    fn image(info: &libc::dl_phdr_info, first: bool) -> Option<Image> {
        let bias = info.dlpi_addr;
        let phdrs = if info.dlpi_phdr.is_null() {
            &[][..]
        } else {
            // SAFETY: the loader's program headers of the object, `dlpi_phnum` of them, mapped
            // as long as the object is loaded, which it is for the call.
            unsafe { std::slice::from_raw_parts(info.dlpi_phdr, usize::from(info.dlpi_phnum)) }
        };
        let (mut low, mut high) = (u64::MAX, 0u64);
        // alloc: an image's segments, once an image a listing
        let mut loads: Vec<(u64, u64)> = Vec::new();
        for it in phdrs.iter().filter(|it| it.p_type == PT_LOAD) {
            let start = bias.wrapping_add(it.p_vaddr);
            loads.push((start, start.saturating_add(it.p_memsz)));
            if it.p_flags & PF_X != 0 {
                low = low.min(start);
                high = high.max(start.saturating_add(it.p_memsz));
            }
        }
        if low >= high {
            return None;
        }
        let build_id = phdrs.iter().filter(|it| it.p_type == PT_NOTE).find_map(|it| {
            let start = bias.wrapping_add(it.p_vaddr);
            let end = start.checked_add(it.p_memsz)?;
            // Only a note inside a loaded segment is mapped to read.
            loads.iter().any(|&(from, to)| from <= start && end <= to).then_some(())?;
            // SAFETY: `[start, end)` lies inside a `PT_LOAD` segment of the object, mapped
            // readable while it is loaded, which it is for the call.
            let notes =
                unsafe { std::slice::from_raw_parts(start as *const u8, it.p_memsz as usize) };
            gnu_build_id(notes, if it.p_align == 8 { 8 } else { 4 })
        });
        // SAFETY: the loader's NUL-terminated name of the object, or null.
        let name = (!info.dlpi_name.is_null())
            .then(|| unsafe { std::ffi::CStr::from_ptr(info.dlpi_name) });
        let name = name.map(|it| it.to_string_lossy().into_owned()).filter(|it| !it.is_empty());
        let main = first && name.is_none();
        let path = match name {
            Some(it) => it,
            None => std::env::current_exe()
                .map(|it| it.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        Some(Image { path, build_id, base: low, size: high - low, slide: bias, main })
    }

    /// The GNU build id among ELF notes `notes`, each aligned to `align`.
    pub(super) fn gnu_build_id(notes: &[u8], align: usize) -> Option<String> {
        let word = |at: usize| -> Option<u32> {
            Some(u32::from_ne_bytes(notes.get(at..at + 4)?.try_into().ok()?))
        };
        let pad = |n: usize| n.div_ceil(align) * align;
        let mut at = 0;
        while at + 12 <= notes.len() {
            let (name_len, desc_len, kind) =
                (word(at)? as usize, word(at + 4)? as usize, word(at + 8)?);
            let name_at = at + 12;
            let desc_at = name_at + pad(name_len);
            let name = notes.get(name_at..name_at + name_len)?;
            let desc = notes.get(desc_at..desc_at + desc_len)?;
            if kind == NT_GNU_BUILD_ID && name == b"GNU\0" {
                return Some(hex(desc));
            }
            at = desc_at + pad(desc_len);
        }
        None
    }

    pub(super) fn boot() -> Option<String> {
        let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        Some(id.trim().to_owned()).filter(|it| !it.is_empty())
    }

    pub(super) fn hash(image: &Image) -> Result<String, String> {
        if image.path.is_empty() || !image.path.starts_with('/') {
            return Err(format!(
                "`{}` names no file (the kernel's vDSO, or a loader with no path)",
                image.path
            ));
        }
        // The executable's own inode, whatever its path holds now.
        let open = if image.main {
            std::path::Path::new("/proc/self/exe")
        } else {
            std::path::Path::new(&image.path)
        };
        super::hash_file(image, open)
    }
}

#[cfg(target_vendor = "apple")]
mod platform {
    use super::{Image, hex};

    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_SEGMENT_64: u32 = 0x19;
    const LC_UUID: u32 = 0x1b;

    pub(super) fn loaded() -> Vec<Image> {
        // SAFETY: no precondition.
        let count = unsafe { libc::_dyld_image_count() };
        // alloc: a listing of images, once a flush with samples
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            // SAFETY: an index below the count; a library unloaded meanwhile gives null.
            let header = unsafe { libc::_dyld_get_image_header(i) };
            if header.is_null() {
                continue;
            }
            // SAFETY: as above.
            let slide = unsafe { libc::_dyld_get_image_vmaddr_slide(i) } as u64;
            // SAFETY: as above; a NUL-terminated path dyld keeps while the image is loaded.
            let name = unsafe { libc::_dyld_get_image_name(i) };
            // SAFETY: as above, checked for null.
            let path = (!name.is_null())
                .then(|| unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy().into_owned());
            // SAFETY: `header` is the loaded image's Mach-O header, mapped while it is loaded.
            if let Some((base, size, uuid)) = unsafe { read(header.cast()) } {
                out.push(Image {
                    path: path.unwrap_or_default(),
                    build_id: uuid.map(|it| hex(&it)),
                    base: base.wrapping_add(slide),
                    size,
                    slide,
                    main: i == 0,
                });
            }
        }
        out
    }

    /// Its `__TEXT` segment's file address and size, and its UUID.
    ///
    /// # Safety
    ///
    /// `header` is the start of a loaded 64-bit Mach-O image: its header and load commands
    /// (`sizeofcmds` bytes after the 32-byte header) are mapped readable.
    unsafe fn read(header: *const u8) -> Option<(u64, u64, Option<[u8; 16]>)> {
        // SAFETY: the header's words, which the caller says are mapped.
        let word = |at: usize| unsafe { header.add(at).cast::<u32>().read_unaligned() };
        if word(0) != MH_MAGIC_64 {
            return None;
        }
        let (count, size) = (word(16) as usize, word(20) as usize);
        let mut at = 32;
        let end = 32 + size;
        let (mut text, mut uuid) = (None, None);
        for _ in 0..count {
            if at + 8 > end {
                break;
            }
            let (cmd, len) = (word(at), word(at + 4) as usize);
            if len < 8 || at + len > end {
                break;
            }
            if cmd == LC_SEGMENT_64 && len >= 48 {
                // SAFETY: inside the load commands, which the caller says are mapped.
                let name = unsafe { std::slice::from_raw_parts(header.add(at + 8), 16) };
                if name.starts_with(b"__TEXT\0") {
                    // SAFETY: as above, `vmaddr` and `vmsize` at 24 and 32 of the command.
                    let (addr, len) = unsafe {
                        (
                            header.add(at + 24).cast::<u64>().read_unaligned(),
                            header.add(at + 32).cast::<u64>().read_unaligned(),
                        )
                    };
                    text = Some((addr, len));
                }
            } else if cmd == LC_UUID && len >= 24 {
                let mut id = [0u8; 16];
                // SAFETY: as above, the 16 bytes after the command's two words.
                unsafe { std::ptr::copy_nonoverlapping(header.add(at + 8), id.as_mut_ptr(), 16) };
                uuid = Some(id);
            }
            at += len;
        }
        text.map(|(addr, len)| (addr, len, uuid))
    }

    pub(super) fn boot() -> Option<String> {
        let mut buf = [0u8; 64];
        let mut len = buf.len();
        // SAFETY: a NUL-terminated name, a buffer of `len` bytes to fill and its length to
        // update; no new value.
        let got = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if got != 0 {
            return None;
        }
        let text = std::str::from_utf8(&buf[..len.min(buf.len())]).ok()?;
        Some(text.trim_end_matches('\0').trim().to_owned()).filter(|it| !it.is_empty())
    }

    pub(super) fn hash(image: &Image) -> Result<String, String> {
        let path = std::path::Path::new(&image.path);
        if !path.exists() {
            let cached = image.path.starts_with("/System/") || image.path.starts_with("/usr/lib/");
            return Err(match cached {
                true => format!("{} is in the dyld shared cache, with no file on disk", image.path),
                false => format!("no file is at {} now", image.path),
            });
        }
        super::hash_file(image, path)
    }
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
mod platform {
    use super::Image;

    pub(super) fn loaded() -> Vec<Image> {
        // alloc: none
        Vec::new()
    }

    pub(super) fn boot() -> Option<String> {
        None
    }

    pub(super) fn hash(image: &Image) -> Result<String, String> {
        Err(format!("{}: ix-prof hashes no image on this system", image.path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[inline(never)]
    fn here() -> u64 {
        std::hint::black_box(here as fn() -> u64 as usize as u64)
    }

    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn the_executable_is_an_image_holding_its_own_code_with_a_build_id_and_a_hash() {
        let pc = here();
        let images = loaded();
        let own = images
            .iter()
            .find(|it| it.contains(pc))
            .unwrap_or_else(|| panic!("no image holds {pc:#x}: {images:#?}"));
        assert!(own.main, "{own:?}");
        assert!(own.build_id.is_some(), "the executable has a build id or UUID: {own:?}");
        let hash = blake3(own).unwrap_or_else(|why| panic!("{why}"));
        assert_eq!(hash.len(), 64);
        assert!(boot().is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_gnu_build_id_note_is_read_past_other_notes() {
        let mut notes = Vec::new();
        for (name, kind, desc) in
            [(&b"Go\0\0"[..], 4u32, &[1u8, 2, 3][..]), (&b"GNU\0"[..], 3, &[0xab, 0xcd][..])]
        {
            notes.extend_from_slice(&(name.len() as u32).to_ne_bytes());
            notes.extend_from_slice(&(desc.len() as u32).to_ne_bytes());
            notes.extend_from_slice(&kind.to_ne_bytes());
            notes.extend_from_slice(name);
            notes.extend_from_slice(desc);
            notes.resize(notes.len().div_ceil(4) * 4, 0);
        }
        assert_eq!(platform::gnu_build_id(&notes, 4).as_deref(), Some("abcd"));
    }
}
