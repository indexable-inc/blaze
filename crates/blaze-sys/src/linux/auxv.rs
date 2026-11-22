//! The auxiliary vector's keys and its one bounded parse: the `(key, value)` words the kernel
//! hands a process at exec (linux/auxvec.h). No C library, so no `getauxval`.
//!
//! Nothing here looks anything up. The facts a program uses (page size, clock ticks, hardware
//! capabilities, the vDSO's address) are a value, [`crate::facts::Aux`], handed in; its
//! constructors read the vector through this parse, from the start block
//! ([`Auxv::from_vector`]) or from `/proc/self/auxv`'s bytes ([`Auxv::from_bytes`]). The
//! crate doc of `facts` says which process takes which source and why.
//!
//! The table is bounded: [`CAP`] pairs, no allocation; a longer vector is
//! [`AuxvError::TooLong`].

/// `AT_NULL`: the vector's end.
pub const AT_NULL: u64 = 0;
/// `AT_PHDR`: the address of the program's own program headers.
/// replaces libc::AT_PHDR
pub const AT_PHDR: u64 = 3;
/// `AT_PHNUM`: how many program headers [`AT_PHDR`] points at.
/// replaces libc::AT_PHNUM
pub const AT_PHNUM: u64 = 5;
/// `AT_PAGESZ`: the size of a page in bytes.
/// replaces libc::AT_PAGESZ
pub const AT_PAGESZ: u64 = 6;
/// `AT_HWCAP`: the CPU's hardware capability bits (asm/hwcap.h on arm64; CPUID 1 `edx` on
/// x86-64).
/// replaces libc::AT_HWCAP
pub const AT_HWCAP: u64 = 16;
/// `AT_CLKTCK`: the clock ticks a second that times in `/proc` count (`USER_HZ`).
/// replaces libc::AT_CLKTCK
pub const AT_CLKTCK: u64 = 17;
/// `AT_RANDOM`: the address of 16 random bytes the kernel wrote for this process.
/// replaces libc::AT_RANDOM
pub const AT_RANDOM: u64 = 25;
/// `AT_HWCAP2`: the second word of hardware capability bits.
/// replaces libc::AT_HWCAP2
pub const AT_HWCAP2: u64 = 26;
/// `AT_SYSINFO_EHDR`: the address of the vDSO's ELF header ([`super::vdso`]); absent on a
/// kernel built without one.
/// replaces libc::AT_SYSINFO_EHDR
pub const AT_SYSINFO_EHDR: u64 = 33;

/// The most pairs a table holds, [`AT_NULL`] not counted. Linux 6.x saves at most
/// `AT_VECTOR_SIZE_BASE + AT_VECTOR_SIZE_ARCH` (22 + 3 on x86-64, 22 + 2 on arm64) pairs: 64
/// leaves room for every key added in the next years.
pub const CAP: usize = 64;

/// The bytes one `(key, value)` pair takes: two native-endian `u64`s.
pub const PAIR: usize = 16;

/// The bytes a whole vector of [`CAP`] pairs and its end takes: the room a reader of
/// `/proc/self/auxv` gives it.
pub const ROOM: usize = (CAP + 1) * PAIR;

/// `PR_GET_AUXV` (linux/prctl.h, Linux 6.4 and later): copy the saved vector out; the call's
/// value is the saved vector's whole size.
const PR_GET_AUXV: usize = 0x4155_5856;

/// One `prctl(PR_GET_AUXV)`: the kernel's saved vector copied into `into` (at most its length),
/// and the saved vector's whole size, which may be more than was copied. [`crate::Errno::Inval`]
/// on a kernel before 6.4. `crate::facts::Aux::from_prctl` calls it, only where the
/// probed fact `KernelFacts::get_auxv` says the kernel has it (D185); nothing here decides.
/// replaces libc::prctl(libc::PR_GET_AUXV, ..)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn prctl_get_auxv(into: &mut [u8]) -> Result<usize, crate::Errno> {
    // SAFETY: `into` is a live, writable buffer of `into.len()` bytes, the length passed; the
    // kernel copies at most that many and keeps no pointer. The fourth and fifth arguments are
    // 0, as `PR_GET_AUXV` requires.
    super::raw::check(unsafe {
        super::raw::syscall5(super::nr::PRCTL, PR_GET_AUXV, into.as_mut_ptr() as usize, into.len(), 0, 0)
    })
}

/// Why a vector could not be parsed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuxvError {
    /// The vector has more than [`CAP`] pairs before its end.
    TooLong,
    /// The vector is not whole 16-byte pairs before an [`AT_NULL`], or has no end.
    Malformed,
}

impl core::fmt::Display for AuxvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AuxvError::TooLong => write!(f, "the auxiliary vector: more than {CAP} pairs"),
            AuxvError::Malformed => f.write_str("the auxiliary vector: not whole pairs before an AT_NULL end"),
        }
    }
}

impl core::error::Error for AuxvError {}

/// A parsed auxiliary vector: its pairs before [`AT_NULL`], at most [`CAP`], in the kernel's
/// order.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Auxv {
    /// The pairs; those at `len..` are zero and unread.
    pairs: [(u64, u64); CAP],
    /// How many of `pairs` the vector has.
    len: usize,
}

impl Auxv {
    /// The vector in `bytes`: `(key, value)` pairs of native-endian `u64`s to an [`AT_NULL`],
    /// as `/proc/<pid>/auxv` holds it. Bytes past the end are not read.
    /// [`AuxvError::Malformed`] where no end comes before the bytes do; [`AuxvError::TooLong`]
    /// past [`CAP`] pairs.
    /// replaces the parse in libc's `getauxval`
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn from_bytes(bytes: &[u8]) -> Result<Auxv, AuxvError> {
        let mut table = Auxv { pairs: [(0, 0); CAP], len: 0 };
        let (pairs, _partial) = bytes.as_chunks::<PAIR>();
        for pair in pairs {
            let (words, _) = pair.as_chunks::<8>();
            let [key, value] = words else { return Err(AuxvError::Malformed) };
            let (key, value) = (u64::from_ne_bytes(*key), u64::from_ne_bytes(*value));
            if key == AT_NULL {
                return Ok(table);
            }
            table.push(key, value)?;
        }
        Err(AuxvError::Malformed)
    }

    /// The vector at `vector` in memory, as the start block holds it: pairs to the [`AT_NULL`],
    /// at most [`CAP`] and the end read. No call is made.
    ///
    /// # Safety
    ///
    /// `vector` is aligned to 8 and readable for pairs to its `AT_NULL` pair (or for
    /// `CAP + 1` pairs, whichever comes first), unchanged while this runs.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub unsafe fn from_vector(vector: *const u64) -> Result<Auxv, AuxvError> {
        let mut table = Auxv { pairs: [(0, 0); CAP], len: 0 };
        for pair in 0..=CAP {
            // SAFETY: the caller's contract: the pairs up to the end are readable and aligned;
            // the loop stops at the end, and at `CAP + 1` pairs at most.
            let (key, value) = unsafe { (vector.add(2 * pair).read(), vector.add(2 * pair + 1).read()) };
            if key == AT_NULL {
                return Ok(table);
            }
            table.push(key, value)?;
        }
        Err(AuxvError::TooLong)
    }

    /// Adds one pair: [`AuxvError::TooLong`] once [`CAP`] are held.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn push(&mut self, key: u64, value: u64) -> Result<(), AuxvError> {
        let slot = self.pairs.get_mut(self.len).ok_or(AuxvError::TooLong)?;
        *slot = (key, value);
        self.len += 1;
        Ok(())
    }

    /// The value of `key` (one of the `AT_*` constants here), `None` where the vector has no
    /// such pair. The first pair of the key where the kernel wrote two (it does not).
    /// replaces libc::getauxval (over a table in hand; the facts are [`crate::facts::Aux`])
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn get(&self, key: u64) -> Option<u64> {
        self.pairs().iter().find(|(at, _)| *at == key).map(|(_, value)| *value)
    }

    /// The pairs, in the kernel's order, [`AT_NULL`] not among them.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn pairs(&self) -> &[(u64, u64)] {
        self.pairs.get(..self.len).unwrap_or(&[])
    }
}

/// The parse against `/proc/self/auxv` and libc's `getauxval`, and the constants against
/// libc's. Dev-only: deleted with the `libc` dev-dependency.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_constants_are_libcs() {
        let ours = [AT_NULL, AT_PHDR, AT_PHNUM, AT_PAGESZ, AT_HWCAP, AT_CLKTCK, AT_RANDOM, AT_HWCAP2, AT_SYSINFO_EHDR];
        let libcs = [
            libc::AT_NULL,
            libc::AT_PHDR,
            libc::AT_PHNUM,
            libc::AT_PAGESZ,
            libc::AT_HWCAP,
            libc::AT_CLKTCK,
            libc::AT_RANDOM,
            libc::AT_HWCAP2,
            libc::AT_SYSINFO_EHDR,
        ];
        for (ours, libcs) in ours.into_iter().zip(libcs) {
            assert_eq!(ours, libcs as u64);
        }
    }

    #[test]
    fn the_proc_files_vector_is_getauxvals() {
        let table = Auxv::from_bytes(&std::fs::read("/proc/self/auxv").unwrap()).unwrap();
        // glibc on x86-64 answers `AT_HWCAP` with its own `dl_hwcap` (`HWCAP_X86_64`), not the
        // kernel's word, so that key is not compared there.
        let keys: &[u64] = if cfg!(target_arch = "x86_64") {
            &[AT_PHDR, AT_PHNUM, AT_PAGESZ, AT_CLKTCK, AT_RANDOM, AT_HWCAP2, AT_SYSINFO_EHDR]
        } else {
            &[AT_PHDR, AT_PHNUM, AT_PAGESZ, AT_HWCAP, AT_CLKTCK, AT_RANDOM, AT_HWCAP2, AT_SYSINFO_EHDR]
        };
        for &key in keys {
            // SAFETY: `getauxval` takes a number and reads the C library's saved vector.
            let libcs = unsafe { libc::getauxval(key as libc::c_ulong) } as u64;
            // `getauxval` says 0 for a key the vector lacks.
            assert_eq!(table.get(key).unwrap_or(0), libcs, "key {key}");
        }
    }

    /// Native-endian bytes of `pairs`.
    fn bytes_of(pairs: &[(u64, u64)]) -> std::vec::Vec<u8> {
        pairs.iter().flat_map(|(key, value)| key.to_ne_bytes().into_iter().chain(value.to_ne_bytes())).collect()
    }

    #[test]
    fn a_vector_without_an_end_or_past_the_cap_is_refused() {
        assert_eq!(Auxv::from_bytes(&bytes_of(&[(AT_PAGESZ, 4096)])), Err(AuxvError::Malformed));
        assert_eq!(Auxv::from_bytes(&[]), Err(AuxvError::Malformed));
        // A torn last pair is no end.
        let mut torn = bytes_of(&[(AT_PAGESZ, 4096)]);
        torn.extend_from_slice(&[0; 8]);
        assert_eq!(Auxv::from_bytes(&torn), Err(AuxvError::Malformed));
        let full: std::vec::Vec<(u64, u64)> = (0..CAP as u64).map(|at| (at + 100, at)).chain([(AT_NULL, 0)]).collect();
        assert_eq!(Auxv::from_bytes(&bytes_of(&full)).unwrap().pairs().len(), CAP);
        let over: std::vec::Vec<(u64, u64)> = (0..=CAP as u64).map(|at| (at + 100, at)).chain([(AT_NULL, 0)]).collect();
        assert_eq!(Auxv::from_bytes(&bytes_of(&over)), Err(AuxvError::TooLong));
        // Bytes past the end are not read.
        let mut tail = bytes_of(&[(AT_PAGESZ, 4096), (AT_NULL, 0)]);
        tail.extend_from_slice(&[0xff; 5]);
        assert_eq!(Auxv::from_bytes(&tail).unwrap().get(AT_PAGESZ), Some(4096));
    }

    #[test]
    fn the_memory_form_reads_to_the_end() {
        let words = [AT_PAGESZ, 4096, AT_CLKTCK, 100, AT_NULL, 0, 0xdead, 0xbeef];
        // SAFETY: `words` is aligned, readable and has its end at its third pair.
        let table = unsafe { Auxv::from_vector(words.as_ptr()) }.unwrap();
        assert_eq!(table.pairs(), &[(AT_PAGESZ, 4096), (AT_CLKTCK, 100)]);
        let words = [AT_PAGESZ; 2 * (CAP + 1)];
        // SAFETY: `words` is aligned and readable for `CAP + 1` pairs.
        assert_eq!(unsafe { Auxv::from_vector(words.as_ptr()) }, Err(AuxvError::TooLong));
    }
}
