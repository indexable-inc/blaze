//! The vDSO: the small shared object the kernel maps into every process (vdso(7)), whose
//! `clock_gettime` reads the clock from a page the kernel keeps current, without entering the
//! kernel. Its functions are the kernel's code, not the C library's: calling one through a
//! pointer found here adds no undefined symbol to blaze-sys.
//!
//! [`Vdso::find`] takes the image's address from `AT_SYSINFO_EHDR` in this process's own
//! facts ([`Aux::vdso`], handed in), parses its ELF (the header, the one `PT_LOAD`, `PT_DYNAMIC`, the
//! symbol and string tables, `DT_HASH` or `DT_GNU_HASH` for the symbol count, `DT_VERSYM` and
//! `DT_VERDEF` for the version) and resolves the kernel's `clock_gettime` once:
//!
//! | arch | symbol | version |
//! |---|---|---|
//! | x86-64 | `__vdso_clock_gettime` | `LINUX_2.6` |
//! | aarch64 | `__kernel_clock_gettime` | `LINUX_2.6.39` |
//!
//! A driver makes the [`Vdso`] once at its entry and passes it to each clock read
//! (`linux::clock_gettime_vdso`): no static holds it.
//!
//! Every read of the image is bounded: first by the one page the kernel maps at least (for the
//! header and program headers), then by the `PT_LOAD`'s file size, which says how many bytes
//! from the header the kernel mapped. Reads go through `.get()` on a byte slice; nothing indexes
//! unchecked.
//!
//! The assumptions about the image, each stated where it is relied on:
//! - the kernel mapped it at `AT_SYSINFO_EHDR` before the process ran, page-aligned, at
//!   least one page, read-only, for the process's life; nothing in this process unmaps or
//!   remaps it (`munmap`, `mremap`, `arch_prctl(ARCH_MAP_VDSO_*)`: blaze makes none of these);
//! - its contents are the kernel's: well-formed. The parse still checks every offset, so a
//!   malformed image is a [`VdsoError`], never an out-of-bounds read.

use core::ffi::c_int;

use crate::Timespec;
use crate::facts::Aux;

/// The vDSO's `clock_gettime`: `int (*)(clockid_t, struct timespec *)`; 0, or an errno negated
/// (the kernel's own system call's result where the vDSO cannot read the clock itself).
/// `struct timespec` on a 64-bit Linux is two `i64`s: [`Timespec`]'s layout.
/// replaces libc::clock_gettime (glibc's `GLRO(dl_vdso_clock_gettime64)` pointer)
pub type ClockGettime = unsafe extern "C" fn(c_int, *mut Timespec) -> c_int;

/// This process's vDSO, its functions resolved once by [`Vdso::find`].
/// replaces glibc's `setup_vdso_pointers` (dl-vdso-setup.c) and its `GLRO` pointers
#[derive(Clone, Copy, Debug)]
pub struct Vdso {
    /// The kernel's `clock_gettime`; `None` where this kernel maps no vDSO or its vDSO has no
    /// such symbol. Private: only [`Vdso::find`] puts a pointer here, so calling it calls the
    /// kernel's code.
    clock_gettime: Option<ClockGettime>,
}

/// Why the vDSO's image could not be read: the part of it that was wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VdsoError {
    /// The image at `AT_SYSINFO_EHDR` is not the 64-bit ELF of this machine the kernel maps,
    /// or one of its tables lies outside it: `what` names the part.
    Malformed {
        /// The part: "the ELF header", "the program headers", "DT_SYMTAB", ...
        what: &'static str,
    },
}

impl core::fmt::Display for VdsoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            VdsoError::Malformed { what } => write!(f, "the vDSO at AT_SYSINFO_EHDR: {what} is malformed or out of the image"),
        }
    }
}

impl core::error::Error for VdsoError {}

/// The `clock_gettime` symbol and version of this architecture's vDSO (the kernel's
/// arch/x86/entry/vdso/vdso.lds.S and arch/arm64/kernel/vdso/vdso.lds.S).
#[cfg(target_arch = "x86_64")]
const CLOCK_GETTIME: (&[u8], &[u8]) = (b"__vdso_clock_gettime", b"LINUX_2.6");
/// The `clock_gettime` symbol and version of this architecture's vDSO.
#[cfg(target_arch = "aarch64")]
const CLOCK_GETTIME: (&[u8], &[u8]) = (b"__kernel_clock_gettime", b"LINUX_2.6.39");

/// `e_machine` (elf.h): `EM_X86_64`.
#[cfg(target_arch = "x86_64")]
const MACHINE: u16 = 62;
/// `e_machine` (elf.h): `EM_AARCH64`.
#[cfg(target_arch = "aarch64")]
const MACHINE: u16 = 183;

/// `EI_DATA` of this machine's byte order: `ELFDATA2LSB` 1, `ELFDATA2MSB` 2.
const DATA: u8 = if cfg!(target_endian = "little") { 1 } else { 2 };

/// The smallest page on either architecture: the kernel maps the image in whole pages, so its
/// first this many bytes are mapped whatever its size.
const PAGE_MIN: usize = 4096;
/// The largest image this parse takes: the kernel's are 8 KiB to 16 KiB.
const IMAGE_CAP: u64 = 1 << 20;
/// The most program headers read.
const PHDR_CAP: u16 = 64;
/// The most symbols looked at.
const SYMBOL_CAP: u64 = 4096;
/// The most version definitions walked.
const VERDEF_CAP: u32 = 64;

/// `sizeof(Elf64_Ehdr)`.
const EHDR_SIZE: u64 = 64;
/// `sizeof(Elf64_Phdr)`.
const PHDR_SIZE: u64 = 56;
/// `sizeof(Elf64_Dyn)`.
const DYN_SIZE: u64 = 16;
/// `sizeof(Elf64_Sym)`.
const SYM_SIZE: u64 = 24;

/// `p_type`: `PT_LOAD`, `PT_DYNAMIC`.
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
/// `p_flags`: `PF_X`.
const PF_X: u32 = 1;

/// `d_tag`s read (elf.h).
const DT_NULL: u64 = 0;
const DT_HASH: u64 = 4;
const DT_STRTAB: u64 = 5;
const DT_SYMTAB: u64 = 6;
const DT_STRSZ: u64 = 10;
const DT_SYMENT: u64 = 11;
const DT_GNU_HASH: u64 = 0x6fff_fef5;
const DT_VERSYM: u64 = 0x6fff_fff0;
const DT_VERDEF: u64 = 0x6fff_fffc;
const DT_VERDEFNUM: u64 = 0x6fff_fffd;

/// `st_info`'s type `STT_FUNC`, bindings `STB_GLOBAL` and `STB_WEAK`; `SHN_UNDEF`.
const STT_FUNC: u8 = 2;
const STB_GLOBAL: u8 = 1;
const STB_WEAK: u8 = 2;
const SHN_UNDEF: u16 = 0;
/// `vd_flags`' `VER_FLG_BASE`: the file's own name, not a version.
const VER_FLG_BASE: u16 = 1;
/// `DT_VERSYM` entries' hidden bit, masked off before the index is compared.
const VERSYM_HIDDEN: u16 = 0x8000;

impl Vdso {
    /// No vDSO: every clock read is the system call. What a kernel built without one gives
    /// ([`Vdso::find`] where `AT_SYSINFO_EHDR` is absent), and a driver's value before it has
    /// read its vector.
    pub const NONE: Vdso = Vdso { clock_gettime: None };

    /// This process's vDSO, its `clock_gettime` resolved: [`Vdso::NONE`] where `aux` has no
    /// vDSO address (a kernel built without one, or a child whose facts its parent told it:
    /// after `execve` its vDSO is at a new address the parent cannot know, so
    /// `Aux::from_parent` carries none and the clock reads are the system call, the stated
    /// path, until the child enters at `_start`); a `Vdso` without `clock_gettime` where the
    /// image lacks the symbol at its version; [`VdsoError`] where the image is not what the
    /// kernel maps.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn find(aux: &Aux) -> Result<Vdso, VdsoError> {
        let Some(base) = aux.vdso() else { return Ok(Vdso::NONE) };
        if base % PAGE_MIN != 0 {
            return Err(VdsoError::Malformed { what: "the image's address" });
        }
        // SAFETY: `base` is this process's own `AT_SYSINFO_EHDR`: an `Aux` carries a vDSO
        // address only when made from this process's own vector (`from_entry`, `from_prctl`,
        // `from_proc`; its fields are private), where the kernel mapped the vDSO page-aligned,
        // readable, at least one page, for the process's life; nothing here unmaps it. The
        // bytes are not written while read: the image is read-only.
        let head: &'static [u8] = unsafe { core::slice::from_raw_parts(base as *const u8, PAGE_MIN) };
        let load = layout(head)?;
        let Ok(size) = usize::try_from(load.size) else { return Err(VdsoError::Malformed { what: "PT_LOAD's p_filesz" }) };
        // SAFETY: the image's one `PT_LOAD` maps the file from offset 0 for `p_filesz` bytes
        // (checked in `layout`), so `size` bytes from `base` are the kernel's mapping, read-only
        // for the process's life, as above.
        let image: &'static [u8] = unsafe { core::slice::from_raw_parts(base as *const u8, size) };
        let (name, version) = CLOCK_GETTIME;
        let clock_gettime = match symbol(image, &load, name, version)? {
            None => None,
            Some(offset) => {
                let Ok(offset) = usize::try_from(offset) else { return Err(VdsoError::Malformed { what: "the symbol's value" }) };
                let Some(at) = base.checked_add(offset) else { return Err(VdsoError::Malformed { what: "the symbol's value" }) };
                // SAFETY: `at` is inside the image's executable `PT_LOAD` (checked in `symbol`),
                // the defined `STT_FUNC` the kernel exports as its `clock_gettime`, whose ABI is
                // the C one with `clockid_t` and `struct timespec *` (vdso(7)); the code stays
                // mapped for the process's life.
                Some(unsafe { core::mem::transmute::<*const (), ClockGettime>(at as *const ()) })
            }
        };
        Ok(Vdso { clock_gettime })
    }

    /// Whether the kernel's `clock_gettime` was found.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn has_clock_gettime(&self) -> bool {
        self.clock_gettime.is_some()
    }

    /// The kernel's `clock_gettime`, for `wait::clock_gettime_vdso`, which calls it.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub(super) const fn clock_gettime_fn(&self) -> Option<ClockGettime> {
        self.clock_gettime
    }
}

/// What the program headers say of the image: its one `PT_LOAD` and its `PT_DYNAMIC`, as file
/// offsets and sizes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Load {
    /// The `PT_LOAD`'s `p_vaddr`: the link address of offset 0. A `d_ptr` or `st_value` less
    /// this is an offset into the image.
    vaddr: u64,
    /// The `PT_LOAD`'s `p_filesz`: the image's mapped bytes from its header.
    size: u64,
    /// Whether the `PT_LOAD` is executable (`PF_X`).
    exec: bool,
    /// `PT_DYNAMIC`'s `p_offset`.
    dynamic: u64,
    /// `PT_DYNAMIC`'s `p_filesz`.
    dynamic_size: u64,
}

/// `N` bytes at `at + field` in `bytes`, or `what` is malformed (past the bytes, or an offset
/// that overflows).
#[blaze::noalloc]
#[blaze::nopanic]
fn bytes_at<const N: usize>(bytes: &[u8], at: u64, field: u64, what: &'static str) -> Result<[u8; N], VdsoError> {
    at.checked_add(field)
        .and_then(|at| usize::try_from(at).ok())
        .and_then(|at| bytes.get(at..))
        .and_then(<[u8]>::first_chunk::<N>)
        .copied()
        .ok_or(VdsoError::Malformed { what })
}

/// A native-endian `u16` at `at + field`.
#[blaze::noalloc]
#[blaze::nopanic]
fn u16_at(bytes: &[u8], at: u64, field: u64, what: &'static str) -> Result<u16, VdsoError> {
    bytes_at::<2>(bytes, at, field, what).map(u16::from_ne_bytes)
}

/// A native-endian `u32` at `at + field`.
#[blaze::noalloc]
#[blaze::nopanic]
fn u32_at(bytes: &[u8], at: u64, field: u64, what: &'static str) -> Result<u32, VdsoError> {
    bytes_at::<4>(bytes, at, field, what).map(u32::from_ne_bytes)
}

/// A native-endian `u64` at `at + field`.
#[blaze::noalloc]
#[blaze::nopanic]
fn u64_at(bytes: &[u8], at: u64, field: u64, what: &'static str) -> Result<u64, VdsoError> {
    bytes_at::<8>(bytes, at, field, what).map(u64::from_ne_bytes)
}

/// `base + index * size`, or `what` is malformed where it overflows.
#[blaze::noalloc]
#[blaze::nopanic]
fn entry(base: u64, index: u64, size: u64, what: &'static str) -> Result<u64, VdsoError> {
    index.checked_mul(size).and_then(|it| base.checked_add(it)).ok_or(VdsoError::Malformed { what })
}

/// The header and program headers in `head` (the image's first bytes): the one `PT_LOAD`,
/// which must map the file from offset 0, and `PT_DYNAMIC`.
#[blaze::noalloc]
#[blaze::nopanic]
fn layout(head: &[u8]) -> Result<Load, VdsoError> {
    const HEADER: &str = "the ELF header";
    let ident = bytes_at::<16>(head, 0, 0, HEADER)?;
    // `\x7fELF`, `ELFCLASS64`, this machine's byte order.
    if ident.first_chunk::<4>() != Some(b"\x7fELF") || ident.get(4) != Some(&2) || ident.get(5) != Some(&DATA) {
        return Err(VdsoError::Malformed { what: HEADER });
    }
    // `e_machine` at 18, `e_phentsize` at 54.
    if u16_at(head, 0, 18, HEADER)? != MACHINE || u16_at(head, 0, 54, HEADER)? != PHDR_SIZE as u16 {
        return Err(VdsoError::Malformed { what: HEADER });
    }
    // `e_phoff` at 32, `e_phnum` at 56.
    let phoff = u64_at(head, 0, 32, HEADER)?;
    let phnum = u16_at(head, 0, 56, HEADER)?;
    if phnum > PHDR_CAP {
        return Err(VdsoError::Malformed { what: "e_phnum" });
    }
    const PHDRS: &str = "the program headers";
    let (mut load, mut dynamic) = (None, None);
    for index in 0..u64::from(phnum) {
        // `Elf64_Phdr`: `p_type` 0, `p_flags` 4, `p_offset` 8, `p_vaddr` 16, `p_filesz` 32.
        let at = entry(phoff, index, PHDR_SIZE, PHDRS)?;
        let kind = u32_at(head, at, 0, PHDRS)?;
        let flags = u32_at(head, at, 4, PHDRS)?;
        let offset = u64_at(head, at, 8, PHDRS)?;
        let vaddr = u64_at(head, at, 16, PHDRS)?;
        let size = u64_at(head, at, 32, PHDRS)?;
        match kind {
            PT_LOAD if load.is_some() => return Err(VdsoError::Malformed { what: "a second PT_LOAD" }),
            PT_LOAD => load = Some((offset, vaddr, size, flags)),
            PT_DYNAMIC => dynamic = Some((offset, size)),
            _ => {}
        }
    }
    let Some((offset, vaddr, size, flags)) = load else { return Err(VdsoError::Malformed { what: "PT_LOAD" }) };
    if offset != 0 || size < EHDR_SIZE || size > IMAGE_CAP {
        return Err(VdsoError::Malformed { what: "PT_LOAD" });
    }
    let Some((dynamic, dynamic_size)) = dynamic else { return Err(VdsoError::Malformed { what: "PT_DYNAMIC" }) };
    Ok(Load { vaddr, size, exec: flags & PF_X != 0, dynamic, dynamic_size })
}

/// What `PT_DYNAMIC` says, as offsets into the image.
#[derive(Clone, Copy, Default)]
struct Dynamic {
    strtab: Option<u64>,
    strsz: Option<u64>,
    symtab: Option<u64>,
    syment: Option<u64>,
    hash: Option<u64>,
    gnu_hash: Option<u64>,
    versym: Option<u64>,
    verdef: Option<u64>,
    verdefnum: Option<u64>,
}

/// The dynamic entries of `image` to `DT_NULL`, their addresses made offsets.
#[blaze::noalloc]
#[blaze::nopanic]
fn dynamic(image: &[u8], load: &Load) -> Result<Dynamic, VdsoError> {
    const WHAT: &str = "PT_DYNAMIC";
    let mut found = Dynamic::default();
    let offset = |vaddr: u64, what: &'static str| vaddr.checked_sub(load.vaddr).ok_or(VdsoError::Malformed { what });
    for index in 0..load.dynamic_size / DYN_SIZE {
        let at = entry(load.dynamic, index, DYN_SIZE, WHAT)?;
        // `Elf64_Dyn`: `d_tag` 0, `d_val`/`d_ptr` 8.
        let tag = u64_at(image, at, 0, WHAT)?;
        let value = u64_at(image, at, 8, WHAT)?;
        match tag {
            DT_NULL => return Ok(found),
            DT_STRTAB => found.strtab = Some(offset(value, "DT_STRTAB")?),
            DT_STRSZ => found.strsz = Some(value),
            DT_SYMTAB => found.symtab = Some(offset(value, "DT_SYMTAB")?),
            DT_SYMENT => found.syment = Some(value),
            DT_HASH => found.hash = Some(offset(value, "DT_HASH")?),
            DT_GNU_HASH => found.gnu_hash = Some(offset(value, "DT_GNU_HASH")?),
            DT_VERSYM => found.versym = Some(offset(value, "DT_VERSYM")?),
            DT_VERDEF => found.verdef = Some(offset(value, "DT_VERDEF")?),
            DT_VERDEFNUM => found.verdefnum = Some(value),
            _ => {}
        }
    }
    Err(VdsoError::Malformed { what: "PT_DYNAMIC's DT_NULL" })
}

/// How many symbols `DT_SYMTAB` has: `DT_HASH`'s `nchain` where the image has `DT_HASH` (both
/// kernels' vDSOs do: arm64 links `--hash-style=sysv`, x86-64 `--hash-style=both`), else the
/// end of `DT_GNU_HASH`'s last chain. Which one is read is the image's own fact, the same
/// every time, not a second try.
#[blaze::noalloc]
#[blaze::nopanic]
fn symbol_count(image: &[u8], found: &Dynamic) -> Result<u64, VdsoError> {
    let count = match (found.hash, found.gnu_hash) {
        // `nbucket` at 0, `nchain` at 4: one chain entry a symbol.
        (Some(hash), _) => u64::from(u32_at(image, hash, 4, "DT_HASH")?),
        (None, Some(gnu)) => gnu_hash_count(image, gnu)?,
        (None, None) => return Err(VdsoError::Malformed { what: "DT_HASH and DT_GNU_HASH (neither)" }),
    };
    if count > SYMBOL_CAP {
        return Err(VdsoError::Malformed { what: "the symbol count" });
    }
    Ok(count)
}

/// The symbol count `DT_GNU_HASH` implies: past the highest bucket's chain's end (the entry
/// with bit 0 set), or `symoffset` where every bucket is empty.
#[blaze::noalloc]
#[blaze::nopanic]
fn gnu_hash_count(image: &[u8], at: u64) -> Result<u64, VdsoError> {
    const WHAT: &str = "DT_GNU_HASH";
    // `nbuckets` 0, `symoffset` 4, `bloom_size` 8, `bloom_shift` 12.
    let nbuckets = u64::from(u32_at(image, at, 0, WHAT)?);
    let symoffset = u64::from(u32_at(image, at, 4, WHAT)?);
    let bloom_size = u64::from(u32_at(image, at, 8, WHAT)?);
    if nbuckets > SYMBOL_CAP || bloom_size > SYMBOL_CAP {
        return Err(VdsoError::Malformed { what: WHAT });
    }
    // The header is four words; the bloom words are 64-bit on ELF64.
    let header_end = at.checked_add(16).ok_or(VdsoError::Malformed { what: WHAT })?;
    let buckets = entry(header_end, bloom_size, 8, WHAT)?;
    let mut highest = 0;
    for index in 0..nbuckets {
        highest = highest.max(u64::from(u32_at(image, entry(buckets, index, 4, WHAT)?, 0, WHAT)?));
    }
    if highest < symoffset {
        return Ok(symoffset);
    }
    let chains = entry(buckets, nbuckets, 4, WHAT)?;
    // `highest` is a `u32`'s value, so the range does not overflow; `symbol >= symoffset`.
    for symbol in highest..highest + SYMBOL_CAP {
        let link = u32_at(image, entry(chains, symbol - symoffset, 4, WHAT)?, 0, WHAT)?;
        if link & 1 != 0 {
            return Ok(symbol + 1);
        }
    }
    Err(VdsoError::Malformed { what: WHAT })
}

/// Whether the NUL-terminated string at `at` in `strtab` is `want`.
#[blaze::noalloc]
#[blaze::nopanic]
fn name_is(strtab: &[u8], at: u32, want: &[u8]) -> bool {
    let Ok(at) = usize::try_from(at) else { return false };
    let Some(end) = at.checked_add(want.len()) else { return false };
    strtab.get(at..end) == Some(want) && strtab.get(end) == Some(&0)
}

/// Whether version index `index` (from `DT_VERSYM`) is the definition named `want`.
#[blaze::noalloc]
#[blaze::nopanic]
fn version_is(image: &[u8], strtab: &[u8], found: &Dynamic, index: u16, want: &[u8]) -> Result<bool, VdsoError> {
    const WHAT: &str = "DT_VERDEF";
    let Some(mut at) = found.verdef else { return Err(VdsoError::Malformed { what: WHAT }) };
    let most = found.verdefnum.map_or(u64::from(VERDEF_CAP), |it| it.min(u64::from(VERDEF_CAP)));
    for _ in 0..most {
        // `Elf64_Verdef`: `vd_flags` 2, `vd_ndx` 4, `vd_aux` 12, `vd_next` 16.
        let flags = u16_at(image, at, 2, WHAT)?;
        let ndx = u16_at(image, at, 4, WHAT)?;
        let aux = u32_at(image, at, 12, WHAT)?;
        let next = u32_at(image, at, 16, WHAT)?;
        if flags & VER_FLG_BASE == 0 && ndx & !VERSYM_HIDDEN == index {
            // The definition's first `Elf64_Verdaux` (`vda_name` at 0) names it.
            let name = u32_at(image, at, u64::from(aux), "DT_VERDEF's Verdaux")?;
            return Ok(name_is(strtab, name, want));
        }
        if next == 0 {
            return Ok(false);
        }
        at = at.checked_add(u64::from(next)).ok_or(VdsoError::Malformed { what: WHAT })?;
    }
    Ok(false)
}

/// The image offset of the defined function `name` at version `version`, `None` where the
/// image has none. Without `DT_VERSYM` the name alone decides, as the kernel's own
/// tools/testing/selftests/vDSO/parse_vdso.c does.
#[blaze::noalloc]
#[blaze::nopanic]
fn symbol(image: &[u8], load: &Load, name: &[u8], version: &[u8]) -> Result<Option<u64>, VdsoError> {
    let found = dynamic(image, load)?;
    let (Some(symtab), Some(strtab), Some(strsz)) = (found.symtab, found.strtab, found.strsz) else {
        return Err(VdsoError::Malformed { what: "DT_SYMTAB, DT_STRTAB or DT_STRSZ" });
    };
    if found.syment.is_some_and(|it| it != SYM_SIZE) {
        return Err(VdsoError::Malformed { what: "DT_SYMENT" });
    }
    let strtab = usize::try_from(strtab)
        .ok()
        .zip(usize::try_from(strsz).ok())
        .and_then(|(at, size)| image.get(at..at.checked_add(size)?))
        .ok_or(VdsoError::Malformed { what: "DT_STRTAB" })?;
    const SYM: &str = "DT_SYMTAB";
    for index in 0..symbol_count(image, &found)? {
        // `Elf64_Sym`: `st_name` 0, `st_info` 4, `st_shndx` 6, `st_value` 8.
        let at = entry(symtab, index, SYM_SIZE, SYM)?;
        let st_name = u32_at(image, at, 0, SYM)?;
        let [info] = bytes_at::<1>(image, at, 4, SYM)?;
        let shndx = u16_at(image, at, 6, SYM)?;
        let value = u64_at(image, at, 8, SYM)?;
        let (kind, bind) = (info & 0xf, info >> 4);
        if kind != STT_FUNC || !(bind == STB_GLOBAL || bind == STB_WEAK) || shndx == SHN_UNDEF {
            continue;
        }
        if !name_is(strtab, st_name, name) {
            continue;
        }
        if let Some(versym) = found.versym {
            let index = u16_at(image, entry(versym, index, 2, "DT_VERSYM")?, 0, "DT_VERSYM")? & !VERSYM_HIDDEN;
            if !version_is(image, strtab, &found, index, version)? {
                continue;
            }
        }
        let offset = value.checked_sub(load.vaddr).ok_or(VdsoError::Malformed { what: "the symbol's value" })?;
        if offset >= load.size || !load.exec {
            return Err(VdsoError::Malformed { what: "the symbol's value" });
        }
        return Ok(Some(offset));
    }
    Ok(None)
}

/// The resolved `clock_gettime` against the system call, and the parse's bounds on every cut of
/// the real image. Linux; the facts by `Aux::from_proc`, the test process's source.
#[cfg(test)]
mod tests {
    use super::super::wait::{ClockId, clock_gettime, clock_gettime_vdso};
    use super::*;

    /// This test process's facts.
    fn own() -> Aux {
        Aux::from_proc().unwrap()
    }

    /// This process's vDSO image, copied.
    fn image() -> (usize, std::vec::Vec<u8>) {
        let base = own().vdso().unwrap();
        // SAFETY: the kernel's vDSO, as in `Vdso::find`.
        let head = unsafe { core::slice::from_raw_parts(base as *const u8, PAGE_MIN) };
        let load = layout(head).unwrap();
        // SAFETY: the `PT_LOAD`'s bytes, as in `Vdso::find`.
        (base, unsafe { core::slice::from_raw_parts(base as *const u8, load.size as usize) }.to_vec())
    }

    #[test]
    fn the_kernels_clock_gettime_is_found() {
        let vdso = Vdso::find(&own()).unwrap();
        assert!(vdso.has_clock_gettime());
        let (base, image) = image();
        let load = layout(&image).unwrap();
        let (name, version) = CLOCK_GETTIME;
        let offset = symbol(&image, &load, name, version).unwrap().unwrap();
        assert_eq!(vdso.clock_gettime_fn().map(|it| it as usize), Some(base + offset as usize));
        // A version the image does not define is not found.
        assert_eq!(symbol(&image, &load, name, b"LINUX_9.9").unwrap(), None);
        assert_eq!(symbol(&image, &load, b"__vdso_no_such_call", version).unwrap(), None);
    }

    #[test]
    fn the_vdsos_clock_is_the_system_calls() {
        let vdso = Vdso::find(&own()).unwrap();
        for clock in [ClockId::Monotonic, ClockId::Boottime, ClockId::Realtime] {
            let nanos = |at: Timespec| at.sec as i128 * 1_000_000_000 + at.nsec as i128;
            let before = nanos(clock_gettime(clock).unwrap());
            let read = nanos(clock_gettime_vdso(&vdso, clock).unwrap());
            let after = nanos(clock_gettime(clock).unwrap());
            if clock != ClockId::Realtime {
                assert!(before <= read && read <= after, "{clock:?}: {before} <= {read} <= {after}");
            }
            assert!((after - before) < 1_000_000_000, "{clock:?}: the reads are within a second");
            assert!((read - before).abs() < 1_000_000_000, "{clock:?}: the vDSO's read is within a second");
        }
        // Monotonic reads through the vDSO do not go back.
        let mut last = clock_gettime_vdso(&vdso, ClockId::Monotonic).unwrap();
        for _ in 0..10_000 {
            let now = clock_gettime_vdso(&vdso, ClockId::Monotonic).unwrap();
            assert!((now.sec, now.nsec) >= (last.sec, last.nsec));
            last = now;
        }
    }

    #[test]
    fn no_vdso_is_the_system_call() {
        let raw = clock_gettime(ClockId::Monotonic).unwrap();
        let none = clock_gettime_vdso(&Vdso::NONE, ClockId::Monotonic).unwrap();
        assert!((none.sec, none.nsec) >= (raw.sec, raw.nsec));
    }

    #[test]
    fn every_cut_of_the_image_is_refused_not_overread() {
        let (_, image) = image();
        let (name, version) = CLOCK_GETTIME;
        let whole = layout(&image).unwrap();
        for cut in 0..image.len() {
            let part = image.get(..cut).unwrap();
            // A cut image either still parses (the tables lie before the cut) or is refused.
            if let Ok(load) = layout(part) {
                let load = Load { size: (cut as u64).min(load.size), ..load };
                let _ = symbol(part, &load, name, version);
            }
            let _ = symbol(part, &whole, name, version);
        }
        // A wrong magic, class or machine is refused.
        for (at, byte) in [(0, 0u8), (4, 1), (18, 0)] {
            let mut bad = image.clone();
            *bad.get_mut(at).unwrap() = byte;
            assert!(layout(&bad).is_err());
        }
    }
}
