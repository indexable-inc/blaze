//! The engine's single home for operating-system access.
//!
//! Everything the engine wants from the OS — filesystem operations for the
//! failure database, a monotonic clock for deadlines and health checks,
//! entropy for PRNG seeding, the `/dev/urandom` reader and the
//! shared-library lookup of Antithesis's fuzzer entropy behind the urandom
//! backend, environment lookups, and stderr output — goes through this
//! module. No other module may touch the OS directly, and no other module
//! may assume an OS exists: every capability here is either fallible
//! ([`Result`]/[`Option`], which callers can disable, fall back from, or
//! surface as an error) or best-effort ([`stderr_line`]). WebAssembly,
//! browser or WASI, obtains the capabilities it can use through explicit
//! host imports.
//!
//! The per-target backends deliberately avoid process-global runtime state:
//! `rustix` on Unix (raw syscalls on Linux, so no libc thread-locals) and
//! direct `windows-sys` calls on Windows. This is part of making `libhegel`
//! safe to `dlclose` — nothing here registers TLS destructors or hooks that
//! could dangle after unload.
//!
//! The one capability here that is not I/O is thread parking, which
//! [`sync`] builds the engine's mutex out of: a platform needs it only to
//! make a contended lock cheap, and a platform without it (wasm) can spin.

use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

#[cfg(all(feature = "runtime", not(feature = "std"), not(test)))]
mod runtime;
pub mod sync;

#[cfg(unix)]
#[path = "unix.rs"]
mod imp;

#[cfg(windows)]
#[path = "windows.rs"]
mod imp;

// Every non-Unix WebAssembly target, WASI included: the backend needs only the
// two `hegel_host` imports, which a WASI embedder provides like a browser does.
// Emscripten reports `unix` and keeps the Unix backend.
#[cfg(all(target_family = "wasm", not(unix)))]
#[path = "wasm.rs"]
mod imp;

#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
compile_error!("libhegel's WebAssembly build does not support threads or shared memory");

/// A platform operation failed. Carries no detail because callers apply
/// capability-specific behavior: fallback for entropy and unavailability for
/// time or direct random bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error;

/// Filesystem operations, all rooted at plain `&str` paths joined with
/// `/`. Failures are reported as [`Error`] without detail: the failure
/// database treats every failure as a silent no-op, and the Antithesis
/// reporter announces one on the run's output.
#[cfg(not(target_family = "wasm"))]
pub mod fs {
    use alloc::string::String;
    use alloc::vec::Vec;

    use super::{Error, imp};

    /// Names of the entries in the directory at `path`, excluding `.` and
    /// `..`. Entries whose names are not valid Unicode are skipped.
    pub fn read_dir(path: &str) -> Result<Vec<String>, Error> {
        imp::read_dir(path)
    }

    /// The full contents of the file at `path`.
    pub fn read(path: &str) -> Result<Vec<u8>, Error> {
        imp::read(path)
    }

    /// Create (or truncate) the file at `path` and write `data` to it.
    pub fn write(path: &str, data: &[u8]) -> Result<(), Error> {
        imp::write(path, data)
    }

    /// Append `data` to the file at `path`, creating it if it does not
    /// exist.
    pub fn append(path: &str, data: &[u8]) -> Result<(), Error> {
        imp::append(path, data)
    }

    /// Create the directory at `path` and any missing parents.
    ///
    /// Reports success if anything already exists at `path`, even a
    /// non-directory (unlike `std`); operations under such a path fail on
    /// their own, which the engine's only filesystem client treats as a
    /// no-op anyway.
    pub fn create_dir_all(path: &str) -> Result<(), Error> {
        let bytes = path.as_bytes();
        for i in 1..bytes.len() {
            if is_separator(bytes[i]) && !is_separator(bytes[i - 1]) {
                let _ = imp::mkdir(&path[..i]);
            }
        }
        match imp::mkdir(path) {
            Ok(()) => Ok(()),
            Err(_) if exists(path) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn is_separator(byte: u8) -> bool {
        byte == b'/' || (cfg!(windows) && byte == b'\\')
    }

    /// Atomically rename `from` to `to`, replacing `to` if it is an
    /// existing file.
    pub fn rename(from: &str, to: &str) -> Result<(), Error> {
        imp::rename(from, to)
    }

    /// Remove the file at `path`.
    pub fn remove_file(path: &str) -> Result<(), Error> {
        imp::remove_file(path)
    }

    /// Remove the directory at `path`; fails unless it is empty.
    pub fn remove_dir(path: &str) -> Result<(), Error> {
        imp::remove_dir(path)
    }

    /// Whether anything exists at `path`.
    pub fn exists(path: &str) -> bool {
        imp::exists(path)
    }
}

/// A point on the OS monotonic clock, used for deadlines and elapsed-time
/// accounting.
///
/// [`Instant::now`] returns `Option` because the clock is a capability, not
/// a given: on a platform without one, every `now` is `None`, deadlines
/// built from it stay unset, and elapsed times read as zero — timing-based
/// features (shrink deadline, TooSlow health check) quietly disable
/// themselves rather than break the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant {
    nanos: u64,
}

impl Instant {
    /// The current point on the monotonic clock, or `None` if this platform
    /// has no monotonic clock.
    pub fn now() -> Option<Instant> {
        imp::monotonic_nanos().map(|nanos| Instant { nanos })
    }

    /// Time elapsed since this instant, saturating to zero if the clock has
    /// since become unavailable or has not advanced.
    pub fn elapsed(&self) -> Duration {
        Instant::now().map_or(Duration::ZERO, |now| now.duration_since(*self))
    }

    /// Time from `earlier` to this instant, saturating to zero if `earlier`
    /// is the later of the two.
    pub fn duration_since(&self, earlier: Instant) -> Duration {
        Duration::from_nanos(self.nanos.saturating_sub(earlier.nanos))
    }
}

impl core::ops::Add<Duration> for Instant {
    type Output = Instant;

    /// The instant `rhs` after this one, saturating at the clock's maximum
    /// representable point.
    fn add(self, rhs: Duration) -> Instant {
        Instant {
            nanos: self.nanos.saturating_add(duration_nanos(rhs)),
        }
    }
}

impl core::ops::Sub<Duration> for Instant {
    type Output = Instant;

    /// The instant `rhs` before this one, saturating at the clock's zero
    /// point.
    fn sub(self, rhs: Duration) -> Instant {
        Instant {
            nanos: self.nanos.saturating_sub(duration_nanos(rhs)),
        }
    }
}

fn duration_nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Fill `buf` from the platform entropy source. On Linux, the Unix backend
/// uses the `getrandom` syscall; other Unix targets read `/dev/urandom`;
/// Windows uses `ProcessPrng`; browser WebAssembly delegates to its
/// `entropy_fill` host
/// import. Callers fall back to a fixed seed on failure.
pub fn entropy(buf: &mut [u8]) -> Result<(), Error> {
    imp::entropy(buf)
}

/// Whether this platform has a direct random source for the urandom backend.
/// Unix provides `/dev/urandom`; Windows and browser WebAssembly do not. When
/// `false`, [`urandom`] always fails and the backend falls back to an
/// OS-seeded PRNG at selection time.
pub fn urandom_available() -> bool {
    imp::urandom_available()
}

/// Fill `buf` with fresh bytes from the platform's direct random source.
/// Native Unix opens `/dev/urandom` for each request. Windows and browser
/// WebAssembly do not provide a direct random source in this phase.
pub fn urandom(buf: &mut [u8]) -> Result<(), Error> {
    imp::urandom(buf)
}

/// A function returning 64 bits of entropy per call, with the fuzzer
/// supplying them: the C signature `uint64_t (*)(void)` of Antithesis's
/// `fuzz_get_random`, which its language SDKs draw their randomness from.
pub type RandomFn = unsafe extern "C" fn() -> u64;

/// A source of entropy delivering eight bytes per call to a function
/// resolved from a shared library, so that the function's provider — the
/// Antithesis fuzzer — sees every draw the engine makes as a request of
/// its own.
#[derive(Debug, Clone, Copy)]
pub struct FuzzSource {
    get_random: RandomFn,
}

impl FuzzSource {
    /// A source drawing from `get_random`.
    ///
    /// # Safety
    ///
    /// `get_random` must be safe to call at any time from any thread with
    /// no preconditions, as a C `uint64_t f(void)` would be.
    pub unsafe fn new(get_random: RandomFn) -> Self {
        FuzzSource { get_random }
    }

    /// Fill `buf` with as few calls as cover it, each call's value laid
    /// down little-endian and the last one's surplus bytes dropped. Nothing
    /// is carried over between fills: every fill starts a fresh call.
    pub fn fill(&self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            // SAFETY: the constructor's contract: the function has no
            // preconditions.
            let word = unsafe { (self.get_random)() };
            chunk.copy_from_slice(&word.to_le_bytes()[..chunk.len()]);
        }
    }
}

/// The source `symbol` names in the shared library at `path`, loading the
/// library if needed. `None` if the library cannot be loaded or lacks the
/// symbol — on Unix, where loading a library by path is possible, and
/// always on Windows and browser WebAssembly. The library stays loaded.
///
/// `symbol` must name a function of [`RandomFn`]'s C signature; there is
/// no way to check, so a mismatch is undefined behaviour at the first draw.
pub fn fuzz_source(path: &str, symbol: &str) -> Option<FuzzSource> {
    let address = imp::dynamic_symbol(path, symbol)?;
    // SAFETY: the caller's contract: the symbol is a C function of
    // `RandomFn`'s signature, so the non-null address is one, and a
    // function pointer has the same representation as a data pointer on
    // every supported target.
    unsafe {
        let get_random = core::mem::transmute::<*mut core::ffi::c_void, RandomFn>(address);
        Some(FuzzSource::new(get_random))
    }
}

/// The value of the environment variable `name`, decoded lossily. `None`
/// if unset.
pub fn env_var(name: &str) -> Option<String> {
    imp::env_var(name)
}

/// The current working directory, decoded lossily. `None` if the OS cannot
/// report one; callers treat that as "no directory to search".
#[cfg(not(target_family = "wasm"))]
pub fn cwd() -> Option<String> {
    imp::cwd()
}

/// Write `line` plus a trailing newline to stderr, best-effort: failures
/// are ignored, and a short write may truncate the line. Diagnostics must
/// never take down a run.
pub fn stderr_line(line: &str) {
    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    imp::stderr_write(&buf);
}

/// The current process id, used to make temporary file names unique across
/// processes sharing a database directory.
#[cfg(not(target_family = "wasm"))]
pub fn pid() -> u32 {
    imp::pid()
}

#[cfg(all(test, not(target_family = "wasm")))]
#[path = "../../tests/embedded/sys_tests.rs"]
mod tests;
