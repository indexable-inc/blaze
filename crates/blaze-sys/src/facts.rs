//! The facts of the process the kernel states once at exec: page size, clock ticks a second,
//! hardware capability words, the vDSO's address, the program headers and the random bytes.
//! One value, [`Aux`], made once at a driver's entry and handed down (rule nothing-ambient):
//! no function in blaze-sys looks any of them up.
//!
//! **The decision (2026-11-20).** Each kind of process has exactly one source, chosen by who
//! starts it. No constructor tries another; a failure is a typed [`AuxError`] naming what.
//!
//! | process | constructor | source |
//! |---|---|---|
//! | entered at blaze's own `_start` (Linux; the end state, sys.md "Runtime") | [`Aux::from_entry`] | the start block: past `argc`, `argv` and `envp`, the vector; no call at all |
//! | a child blaze starts itself (launcher, compile-time children, confined helpers; every unix) | [`Aux::from_parent`] | the parent's start message, encoded by [`Aux::to_bytes`] |
//! | the top-level blaze process while std enters it (Linux, the transition), kernel 6.4 and later | [`Aux::from_prctl`] | one `prctl(PR_GET_AUXV)` into a stack buffer; no procfs |
//! | the same process on an older kernel | [`Aux::from_proc`] | one `openat` and read of `/proc/self/auxv` into a stack buffer |
//! | any process on macOS | [`Aux::from_system`] | `sysconf(_SC_PAGESIZE)`; `CLK_TCK`, 100 |
//!
//! The top-level process's two rows are a selection by a probed fact, not a fallback (D185,
//! end-state.md): [`Aux::for_top_level`] takes `KernelFacts::get_auxv` (lane K1's probe, made
//! before any of these calls and printed with the kernel's facts) and calls exactly one of
//! them. Neither constructor tries the other; each error names its source.
//!
//! What lost: a 6.4 kernel floor (`prctl` alone); glibc's `getauxval` (a C-library symbol,
//! which blaze-sys on Linux does not reference); `/proc/self/auxv` in every process (it fails
//! under Landlock without `/proc`, once a process is not dumpable, and where procfs is not
//! mounted: a child started by blaze never reads it).
//!
//! **What a child is told.** [`Aux::from_parent`] carries only the values (page size, clock
//! ticks, the capability words): never an address. After `execve` the child's vDSO, program
//! headers and random bytes are at new addresses, so a parent's would be wrong. A child made
//! this way has [`Aux::vdso`] `None`, and its clock reads take the system call, the stated
//! path, until it enters at `_start` and reads its own vector by [`Aux::from_entry`].

#[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")))]
use crate::linux::auxv::{
    AT_CLKTCK, AT_HWCAP, AT_HWCAP2, AT_PAGESZ, AT_PHDR, AT_PHNUM, AT_RANDOM, AT_SYSINFO_EHDR, Auxv, AuxvError,
};
/// The encoding's version: the first byte of [`Aux::to_bytes`]'s output.
const VERSION: u8 = 1;

/// The bytes [`Aux::to_bytes`] writes: the version, then the page size, the clock ticks,
/// `hwcap` and `hwcap2` as little-endian `u64`s.
pub const ENCODED: usize = 1 + 4 * 8;

/// The facts of this process. Fields are read through getters only; only the constructors
/// here make one, so an address in it ([`Aux::vdso`]) is the kernel's for this process.
/// replaces libc::getauxval, libc::sysconf(libc::_SC_PAGESIZE) and libc::sysconf(libc::_SC_CLK_TCK)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Aux {
    /// `AT_PAGESZ`: bytes a page, a power of two.
    page_size: usize,
    /// `AT_CLKTCK`: the ticks a second `/proc`'s times count.
    clock_ticks: u64,
    /// `AT_HWCAP`; 0 where the kernel gives none, and on macOS.
    hwcap: u64,
    /// `AT_HWCAP2`; 0 likewise.
    hwcap2: u64,
    /// `AT_SYSINFO_EHDR`: the vDSO's ELF header in this process; `None` for a kernel without
    /// one, a child told by its parent, and macOS.
    vdso: Option<usize>,
    /// `AT_PHDR` and `AT_PHNUM`: this program's program headers; `None` where not this
    /// process's own vector.
    phdr: Option<(usize, usize)>,
    /// The 16 bytes at `AT_RANDOM`, copied when made; `None` likewise.
    random: Option<[u8; 16]>,
}

/// Why an [`Aux`] could not be made: what, and from which source.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuxError {
    /// `/proc/self/auxv` could not be opened or read: the errno (`EACCES` for a process that
    /// is not dumpable, `ENOENT` without procfs).
    Proc(crate::Errno),
    /// `prctl(PR_GET_AUXV)` refused: the errno ([`crate::Errno::Inval`] on a kernel before
    /// 6.4, which `KernelFacts::get_auxv` says before the call is made).
    Prctl(crate::Errno),
    /// The vector has more pairs than the bounded table holds, or the file more bytes than its
    /// room (`linux::auxv::CAP`).
    TooLong,
    /// The vector is not whole pairs before an `AT_NULL` end.
    Malformed,
    /// The vector lacks a key every Linux kernel writes: its name.
    Missing {
        /// `"AT_PAGESZ"` or `"AT_CLKTCK"`.
        key: &'static str,
    },
    /// A parent's encoding is not [`ENCODED`] bytes long.
    Length {
        /// The length given.
        found: usize,
    },
    /// A parent's encoding has another version than this build's.
    Version {
        /// The version byte given.
        found: u8,
    },
    /// A value is not a page size or tick rate: the field.
    Invalid {
        /// `"page_size"` or `"clock_ticks"`.
        field: &'static str,
    },
}

impl core::fmt::Display for AuxError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AuxError::Proc(errno) => write!(f, "the process's facts: /proc/self/auxv: {errno:?}"),
            AuxError::Prctl(errno) => write!(f, "the process's facts: prctl(PR_GET_AUXV): {errno:?}"),
            AuxError::TooLong => f.write_str("the process's facts: the auxiliary vector is longer than its bounded table"),
            AuxError::Malformed => f.write_str("the process's facts: the auxiliary vector has no AT_NULL end"),
            AuxError::Missing { key } => write!(f, "the process's facts: the auxiliary vector has no {key}"),
            AuxError::Length { found } => write!(f, "the process's facts: the parent's message is {found} bytes, not {ENCODED}"),
            AuxError::Version { found } => write!(f, "the process's facts: the parent's message is version {found}, not {VERSION}"),
            AuxError::Invalid { field } => write!(f, "the process's facts: {field} is not a valid value"),
        }
    }
}

impl core::error::Error for AuxError {}

#[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")))]
impl From<AuxvError> for AuxError {
    fn from(error: AuxvError) -> AuxError {
        match error {
            AuxvError::TooLong => AuxError::TooLong,
            AuxvError::Malformed => AuxError::Malformed,
        }
    }
}

/// [`AuxError::Invalid`] unless `page_size` is a power of two and `clock_ticks` is not 0.
#[blaze::noalloc]
#[blaze::nopanic]
const fn checked(page_size: u64, clock_ticks: u64) -> Result<(usize, u64), AuxError> {
    if !page_size.is_power_of_two() || page_size > usize::MAX as u64 {
        return Err(AuxError::Invalid { field: "page_size" });
    }
    if clock_ticks == 0 {
        return Err(AuxError::Invalid { field: "clock_ticks" });
    }
    Ok((page_size as usize, clock_ticks))
}

impl Aux {
    /// The facts in this process's own vector `table`: [`AuxError::Missing`] without
    /// `AT_PAGESZ` or `AT_CLKTCK`; `hwcap` and `hwcap2` 0 where absent (the kernel states
    /// none); the 16 `AT_RANDOM` bytes copied.
    ///
    /// # Safety
    ///
    /// `table` is this process's own vector, as the kernel wrote it at exec: its addresses
    /// (`AT_RANDOM`'s 16 bytes, `AT_SYSINFO_EHDR`'s image) are this process's mappings.
    #[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[blaze::noalloc]
    #[blaze::nopanic]
    unsafe fn from_own_table(table: &Auxv) -> Result<Aux, AuxError> {
        let page_size = table.get(AT_PAGESZ).ok_or(AuxError::Missing { key: "AT_PAGESZ" })?;
        let clock_ticks = table.get(AT_CLKTCK).ok_or(AuxError::Missing { key: "AT_CLKTCK" })?;
        let (page_size, clock_ticks) = checked(page_size, clock_ticks)?;
        let address = |key| table.get(key).filter(|it| *it != 0).and_then(|it| usize::try_from(it).ok());
        let phdr = address(AT_PHDR).zip(table.get(AT_PHNUM).and_then(|it| usize::try_from(it).ok()));
        let random = address(AT_RANDOM).map(|at| {
            // SAFETY: the caller's contract: `AT_RANDOM` is the address of the 16 bytes the
            // kernel wrote above this process's start block, readable for its life; a byte
            // array has no alignment.
            unsafe { (at as *const [u8; 16]).read() }
        });
        Ok(Aux {
            page_size,
            clock_ticks,
            hwcap: table.get(AT_HWCAP).unwrap_or(0),
            hwcap2: table.get(AT_HWCAP2).unwrap_or(0),
            vdso: address(AT_SYSINFO_EHDR),
            phdr,
            random,
        })
    }

    /// The facts of a process entered at blaze's own `_start`, from its start block: `stack`
    /// is the stack pointer the kernel entered with, at `argc`. Walks `argv` and `envp` to
    /// their nulls, then parses the vector; makes no call. The end state's constructor
    /// (sys.md "Runtime"): today no entry calls it. [`AuxError::TooLong`] for an `envp` past
    /// `ENV_CAP` entries.
    ///
    /// # Safety
    ///
    /// `stack` is the initial stack pointer of this process as the kernel laid it out at
    /// exec (`argc`, `argv[]`, null, `envp[]`, null, the vector), aligned to 8, and none of it
    /// has been written since.
    #[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub unsafe fn from_entry(stack: *const u64) -> Result<Aux, AuxError> {
        /// The most environment entries walked: far past the kernel's `ARG_MAX` of strings
        /// that fit (each at least two bytes in 2 MiB by default).
        const ENV_CAP: usize = 1 << 22;
        // SAFETY: the caller's contract: `stack` points at `argc`.
        let argc = unsafe { stack.read() };
        let Ok(argc) = usize::try_from(argc) else { return Err(AuxError::Malformed) };
        // `argv[0..argc]`, then its null: `envp` begins two words past `argc`'s last.
        let Some(mut at) = argc.checked_add(2) else { return Err(AuxError::Malformed) };
        for _ in 0..=ENV_CAP {
            // SAFETY: the kernel's layout: the words from `argc` to the vector's end are the
            // start block, readable; the loop stops at `envp`'s null.
            let word = unsafe { stack.add(at).read() };
            at += 1;
            if word == 0 {
                // SAFETY: `at` is the vector's first word, by the layout above; the caller's
                // contract makes it this process's own and unchanged.
                let table = unsafe { Auxv::from_vector(stack.add(at)) }?;
                // SAFETY: the table is this process's own vector, as the contract says.
                return unsafe { Aux::from_own_table(&table) };
            }
        }
        Err(AuxError::TooLong)
    }

    /// The facts of the top-level blaze process while std enters it (Linux, the transition):
    /// one `openat` of `/proc/self/auxv` and its reads to the end into a stack buffer of
    /// `linux::auxv::ROOM` bytes. [`AuxError::Proc`] with the errno where the file cannot be
    /// opened or read, [`AuxError::TooLong`] where it fills the buffer. No other source is
    /// tried. Only that process calls it: a child blaze starts is told by [`Aux::from_parent`].
    #[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn from_proc() -> Result<Aux, AuxError> {
        use crate::linux::auxv::ROOM;
        use crate::linux::{CWD, OpenFlags, openat, read};
        let file = openat(CWD, crate::Path::new(c"/proc/self/auxv"), OpenFlags::READ.with(OpenFlags::CLOSE_ON_EXEC), 0)
            .map_err(AuxError::Proc)?;
        let mut room = [0u8; ROOM];
        let mut filled = 0;
        // The reads of one file to its end, not attempts: each fills at least one byte or
        // ends the file, so `ROOM` reads bound the loop.
        for _ in 0..=ROOM {
            let rest = room.get_mut(filled..).ok_or(AuxError::TooLong)?;
            if rest.is_empty() {
                return Err(AuxError::TooLong);
            }
            match read(file.fd(), rest).map_err(AuxError::Proc)? {
                0 => {
                    let table = Auxv::from_bytes(room.get(..filled).ok_or(AuxError::TooLong)?)?;
                    // SAFETY: `/proc/self/auxv` is this process's own vector as the kernel
                    // saved it at exec.
                    return unsafe { Aux::from_own_table(&table) };
                }
                got => filled += got,
            }
        }
        Err(AuxError::TooLong)
    }

    /// The facts of the top-level blaze process while std enters it, on a kernel that has
    /// `PR_GET_AUXV` (6.4 and later): one `prctl(PR_GET_AUXV)` into a stack buffer of
    /// `linux::auxv::ROOM` bytes; no file, so it works without procfs, under Landlock and when
    /// not dumpable. [`AuxError::Prctl`] with the errno where the kernel refuses,
    /// [`AuxError::TooLong`] where its saved vector is larger than the buffer. No other source
    /// is tried: [`Aux::for_top_level`] calls it only where `KernelFacts::get_auxv` is true.
    #[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn from_prctl() -> Result<Aux, AuxError> {
        use crate::linux::auxv::{ROOM, prctl_get_auxv};
        let mut room = [0u8; ROOM];
        let size = prctl_get_auxv(&mut room).map_err(AuxError::Prctl)?;
        // The call's value is the saved vector's whole size: past `ROOM`, the copy was cut.
        let table = Auxv::from_bytes(room.get(..size).ok_or(AuxError::TooLong)?)?;
        // SAFETY: the kernel's saved copy of this process's own vector from exec.
        unsafe { Aux::from_own_table(&table) }
    }

    /// The facts of the top-level blaze process while std enters it (Linux, the transition),
    /// by the source the kernel's probed fact selects (D185): `get_auxv` is
    /// `KernelFacts::get_auxv` (lane K1's probe, `prctl(PR_GET_AUXV, null, 0, 0, 0)` answering
    /// a length or `EINVAL`, made once before this and printed with the kernel's facts).
    /// `true` calls [`Aux::from_prctl`] only, `false` [`Aux::from_proc`] only: a selection
    /// made before any call, never a second try after a failure, whose error names its source.
    #[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn for_top_level(get_auxv: bool) -> Result<Aux, AuxError> {
        if get_auxv { Aux::from_prctl() } else { Aux::from_proc() }
    }

    /// The facts of a macOS process: the page size by `sysconf(_SC_PAGESIZE)` (libSystem,
    /// declared in `posix::thread`); the clock ticks the fixed `CLK_TCK` of macOS, 100
    /// (sys/time.h: `__DARWIN_CLK_TCK`, what `sysconf(_SC_CLK_TCK)` answers); no capability
    /// words (macOS states the CPU's features by sysctl: `cpu.rs`), no vDSO, no vector.
    /// [`AuxError::Invalid`] where `sysconf` answers no power of two.
    #[cfg(target_os = "macos")]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn from_system() -> Result<Aux, AuxError> {
        /// `CLK_TCK` (sys/time.h, macOS): 100.
        const CLK_TCK: u64 = 100;
        use crate::posix::thread::{_SC_PAGESIZE, sysconf};
        let page_size = u64::try_from(sysconf(_SC_PAGESIZE)).map_err(|_| AuxError::Invalid { field: "page_size" })?;
        let (page_size, clock_ticks) = checked(page_size, CLK_TCK)?;
        Ok(Aux { page_size, clock_ticks, hwcap: 0, hwcap2: 0, vdso: None, phdr: None, random: None })
    }

    /// The facts a parent told this child, in [`Aux::to_bytes`]'s encoding: the values only;
    /// no address (after `execve` the child's vDSO, program headers and random bytes are its
    /// own, at new addresses), so [`Aux::vdso`] is `None` and the child's clock reads are the
    /// system call. [`AuxError::Length`], [`AuxError::Version`] or [`AuxError::Invalid`] for
    /// a message that is not this build's.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn from_parent(bytes: &[u8]) -> Result<Aux, AuxError> {
        let Some(message) = bytes.first_chunk::<ENCODED>().filter(|_| bytes.len() == ENCODED) else {
            return Err(AuxError::Length { found: bytes.len() });
        };
        let [version, words @ ..] = message;
        if *version != VERSION {
            return Err(AuxError::Version { found: *version });
        }
        let (words, _) = words.as_chunks::<8>();
        let [page_size, clock_ticks, hwcap, hwcap2] = words else { return Err(AuxError::Length { found: bytes.len() }) };
        let (page_size, clock_ticks) = checked(u64::from_le_bytes(*page_size), u64::from_le_bytes(*clock_ticks))?;
        Ok(Aux {
            page_size,
            clock_ticks,
            hwcap: u64::from_le_bytes(*hwcap),
            hwcap2: u64::from_le_bytes(*hwcap2),
            vdso: None,
            phdr: None,
            random: None,
        })
    }

    /// The values a child is told ([`Aux::from_parent`] reads them): the version byte, then
    /// the page size, the clock ticks, `hwcap` and `hwcap2` as little-endian `u64`s. No
    /// address is written.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn to_bytes(&self, out: &mut [u8; ENCODED]) {
        let words = [self.page_size as u64, self.clock_ticks, self.hwcap, self.hwcap2];
        let [version, rest @ ..] = out;
        *version = VERSION;
        let (slots, _) = rest.as_chunks_mut::<8>();
        for (slot, word) in slots.iter_mut().zip(words) {
            *slot = word.to_le_bytes();
        }
    }

    /// The size of a page in bytes, a power of two.
    /// replaces libc::sysconf(libc::_SC_PAGESIZE) and libc::getauxval(libc::AT_PAGESZ)
    /// (needs the runtime's `Aux` handed to the site; not for the port's first pass: leave the
    /// site and list it with the parameter it should take)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn page_size(&self) -> usize {
        self.page_size
    }

    /// The clock ticks a second that `/proc/<pid>/stat`'s times count (`USER_HZ`).
    /// replaces libc::sysconf(libc::_SC_CLK_TCK) and libc::getauxval(libc::AT_CLKTCK)
    /// (needs the runtime's `Aux` handed to the site; not for the port's first pass: leave the
    /// site and list it with the parameter it should take)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn clock_ticks(&self) -> u64 {
        self.clock_ticks
    }

    /// `AT_HWCAP`: the kernel's hardware capability word; 0 on macOS.
    /// replaces libc::getauxval(libc::AT_HWCAP)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn hwcap(&self) -> u64 {
        self.hwcap
    }

    /// `AT_HWCAP2`: the second capability word; 0 on macOS.
    /// replaces libc::getauxval(libc::AT_HWCAP2)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn hwcap2(&self) -> u64 {
        self.hwcap2
    }

    /// `AT_SYSINFO_EHDR`: the address of this process's vDSO image, which
    /// `linux::vdso::Vdso::find` parses; `None` for a kernel without one, a child told by its
    /// parent, and macOS.
    /// replaces libc::getauxval(libc::AT_SYSINFO_EHDR)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn vdso(&self) -> Option<usize> {
        self.vdso
    }

    /// `AT_PHDR` and `AT_PHNUM`: the address and count of this program's program headers;
    /// `None` where not read from this process's own vector.
    /// replaces libc::getauxval(libc::AT_PHDR) and libc::getauxval(libc::AT_PHNUM)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn program_headers(&self) -> Option<(usize, usize)> {
        self.phdr
    }

    /// The 16 random bytes the kernel wrote at `AT_RANDOM`, copied; `None` likewise.
    /// replaces libc::getauxval(libc::AT_RANDOM) (and the read of its bytes)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn random(&self) -> Option<[u8; 16]> {
        self.random
    }
}

/// Each constructor against libc's `sysconf` and `getauxval`, and the parent's encoding round
/// trip. Dev-only: the libc comparisons are deleted with the `libc` dev-dependency.
#[cfg(test)]
mod tests {
    use super::*;

    /// This process's facts by the constructor its platform has.
    fn own() -> Aux {
        #[cfg(target_os = "linux")]
        return Aux::from_proc().unwrap();
        #[cfg(target_os = "macos")]
        return Aux::from_system().unwrap();
    }

    #[test]
    fn the_page_size_and_ticks_are_sysconfs() {
        let aux = own();
        // SAFETY: `sysconf` takes a number.
        let (page, ticks) = unsafe { (libc::sysconf(libc::_SC_PAGESIZE), libc::sysconf(libc::_SC_CLK_TCK)) };
        assert_eq!(aux.page_size(), page as usize);
        assert_eq!(aux.clock_ticks(), ticks as u64);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_proc_files_facts_are_getauxvals() {
        let aux = Aux::from_proc().unwrap();
        // SAFETY: `getauxval` takes a number.
        let get = |key: libc::c_ulong| unsafe { libc::getauxval(key) } as usize;
        assert_eq!(aux.vdso(), Some(get(libc::AT_SYSINFO_EHDR)).filter(|it| *it != 0));
        assert_eq!(aux.program_headers(), Some((get(libc::AT_PHDR), get(libc::AT_PHNUM))));
        // SAFETY: `AT_RANDOM`'s 16 bytes, as the kernel wrote them.
        let random = unsafe { (get(libc::AT_RANDOM) as *const [u8; 16]).read() };
        assert_eq!(aux.random(), Some(random));
        assert_eq!(aux.hwcap2(), get(libc::AT_HWCAP2) as u64);
        // glibc on x86-64 answers `AT_HWCAP` with its own `HWCAP_X86_64`, not the kernel's word.
        #[cfg(target_arch = "aarch64")]
        assert_eq!(aux.hwcap(), get(libc::AT_HWCAP) as u64);
    }

    /// Where the kernel has `PR_GET_AUXV` both top-level sources give the same facts; where it
    /// has not, `from_prctl` names its refusal.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_two_top_level_sources_agree() {
        let has = crate::linux::auxv::prctl_get_auxv(&mut []).is_ok();
        if has {
            assert_eq!(Aux::from_prctl().unwrap(), Aux::from_proc().unwrap());
            assert_eq!(Aux::for_top_level(true).unwrap(), Aux::from_proc().unwrap());
        } else {
            assert_eq!(Aux::from_prctl(), Err(AuxError::Prctl(crate::Errno::Inval)));
        }
        assert_eq!(Aux::for_top_level(false).unwrap(), Aux::from_proc().unwrap());
    }

    #[test]
    fn a_child_is_told_the_values_and_no_address() {
        let aux = own();
        let mut message = [0u8; ENCODED];
        aux.to_bytes(&mut message);
        let told = Aux::from_parent(&message).unwrap();
        assert_eq!(
            (told.page_size(), told.clock_ticks(), told.hwcap(), told.hwcap2()),
            (aux.page_size(), aux.clock_ticks(), aux.hwcap(), aux.hwcap2())
        );
        assert_eq!((told.vdso(), told.program_headers(), told.random()), (None, None, None));
        assert_eq!(Aux::from_parent(&message[..ENCODED - 1]), Err(AuxError::Length { found: ENCODED - 1 }));
        let mut longer = [0u8; ENCODED + 1];
        longer[..ENCODED].copy_from_slice(&message);
        assert_eq!(Aux::from_parent(&longer), Err(AuxError::Length { found: ENCODED + 1 }));
        let mut other = message;
        other[0] = VERSION + 1;
        assert_eq!(Aux::from_parent(&other), Err(AuxError::Version { found: VERSION + 1 }));
        let mut no_page = message;
        no_page[1..9].copy_from_slice(&3000u64.to_le_bytes());
        assert_eq!(Aux::from_parent(&no_page), Err(AuxError::Invalid { field: "page_size" }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_entry_form_walks_the_start_block() {
        let random = [7u8; 16];
        // argc 2, argv[0..2], null, envp[0..1], null, the vector.
        let block: [u64; 15] = [
            2,
            0x1000,
            0x2000,
            0,
            0x3000,
            0,
            AT_PAGESZ,
            4096,
            AT_CLKTCK,
            100,
            AT_RANDOM,
            random.as_ptr() as u64,
            crate::linux::auxv::AT_NULL,
            0,
            0,
        ];
        // SAFETY: `block` is laid out as the kernel's start block, aligned and readable, and
        // its `AT_RANDOM` points at 16 live bytes.
        let aux = unsafe { Aux::from_entry(block.as_ptr()) }.unwrap();
        assert_eq!((aux.page_size(), aux.clock_ticks(), aux.random()), (4096, 100, Some(random)));
        assert_eq!((aux.vdso(), aux.hwcap()), (None, 0));
    }
}
