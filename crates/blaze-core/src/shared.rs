//! A small file of `u64` words mapped shared by every process that opens it, with waits and
//! wakes on its words across processes: the driver of a ledger every blaze process of one user
//! counts in (`blaze_graph::shared_ledger`, docs/v2/plan/design/end-state/memory.md section
//! 4.5), with no daemon to own it.
//!
//! [`Shared::open_or_create`] opens the file, or makes it: the first process writes the whole
//! file (its header, then zeros) under a name of its own and links it into place, so every
//! process that opens the file sees it whole, and of two first processes one link wins and the
//! other opens the winner's. A file of another length, of another header identity (magic,
//! layout) or of another user is a typed error naming the file, and is never truncated or
//! written: a blaze of another layout keeps its own file (the layout is in the name), and a file
//! that is no ledger is for the person to remove.
//!
//! [`Shared::wait`] sleeps while a word's low 32 bits hold a value, until a [`Shared::wake_all`]
//! of that word from any process or a deadline, through [`crate::wait::until`] (the time left
//! recomputed from the clock before every call, interrupted and spurious returns retried, the
//! wait named in the wait-for graph). The kernel compares the word in the same step as it
//! blocks, so a wake between the caller's look and its wait is not lost:
//! - Linux: `futex(FUTEX_WAIT)` and `futex(FUTEX_WAKE)` without `FUTEX_PRIVATE_FLAG`, which key
//!   the wait by the mapped page, so processes that map one file meet on one word;
//! - macOS: `os_sync_wait_on_address_with_timeout` and `os_sync_wake_by_address_all` with
//!   `OS_SYNC_WAIT_ON_ADDRESS_SHARED` / `OS_SYNC_WAKE_BY_ADDRESS_SHARED` (<os/os_sync_wait_on_address.h>,
//!   macOS 14.4): the same, keyed by the shared mapping.
//!
//! Each wait is one system call with the time left of an absolute deadline, computed from the
//! clock before the call: never a relative timeout carried across a retry (rule deadline-waits).

use std::{
    ffi::OsString,
    fmt,
    fs::{File, OpenOptions},
    io::{self, Write as _},
    os::unix::fs::{MetadataExt as _, OpenOptionsExt as _},
    path::{Path, PathBuf},
    ptr::NonNull,
    sync::atomic::AtomicU64,
    time::Duration,
};

use crate::wait::{Clock, Deadline, Once, WaitError, Waited, until};

#[cfg(not(target_endian = "little"))]
compile_error!("the shared file's words are little-endian, and a wait reads a word's low half at its address");

const WORD: usize = size_of::<u64>();

/// A file of words mapped shared: what every process that opened it reads and writes.
pub struct Shared {
    words: NonNull<AtomicU64>,
    len: usize,
    path: PathBuf,
}

// SAFETY: the mapping is the process's until `Drop` unmaps it, and the words are atomics: any
// thread may read and write them through `&self`.
unsafe impl Send for Shared {}
// SAFETY: as above.
unsafe impl Sync for Shared {}

impl Shared {
    /// Opens the file `path` of `len` words and maps it shared, or makes it: written whole under
    /// a name of this process's (`initial`, then zeros, mode 0600), then linked into place. Its
    /// first `identity` words must be `initial`'s, its length `len` words and its owner this
    /// user; otherwise a typed error names the file, which is left as it is.
    pub fn open_or_create(
        path: &Path,
        initial: &[u64],
        len: usize,
        identity: usize,
    ) -> Result<Shared, SharedError> {
        let error = |why| SharedError {
            // alloc: the path an error names, once an error
            path: path.to_path_buf(),
            why,
        };
        let io = |op: &'static str| move |source: io::Error| error(SharedWhy::Io { op, source });
        assert!(
            initial.len() <= len && identity <= initial.len() && len > 0,
            "a shared file of {len} words with {} initial and {identity} identity words",
            initial.len()
        );
        let file = match open(path) {
            Ok(file) => file,
            Err(missing) if missing.kind() == io::ErrorKind::NotFound => {
                create(path, initial, len).map_err(io("create"))?;
                open(path).map_err(io("open the file just made"))?
            }
            Err(other) => return Err(io("open")(other)),
        };
        let meta = file.metadata().map_err(io("stat"))?;
        // SAFETY: a read of this process's effective user id.
        let mine = unsafe { libc::geteuid() };
        if meta.uid() != mine {
            return Err(error(SharedWhy::Owner { uid: meta.uid(), mine }));
        }
        let want = (len * WORD) as u64;
        if meta.len() != want {
            return Err(error(SharedWhy::Length { bytes: meta.len(), want }));
        }
        let words = map(&file, len).map_err(io("mmap"))?;
        // alloc: the path the file's errors name, once it is opened
        let shared = Shared { words, len, path: path.to_path_buf() };
        for (word, &want) in initial.iter().enumerate().take(identity) {
            let found = shared.words()[word].load(std::sync::atomic::Ordering::SeqCst);
            if found != want {
                return Err(error(SharedWhy::Identity { word, found, want }));
            }
        }
        Ok(shared)
    }

    /// The mapped words.
    pub fn words(&self) -> &[AtomicU64] {
        // SAFETY: `len` atomics mapped readable and writable until `Drop`, aligned to a page.
        unsafe { std::slice::from_raw_parts(self.words.as_ptr(), self.len) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sleeps while word `index`'s low 32 bits hold `expected`, until a [`Shared::wake_all`] of
    /// it from any process that mapped the file, or `deadline` on `clock`. `Ready` when woken or
    /// when the word no longer held `expected`; the caller looks at it again either way.
    pub fn wait(
        &self,
        what: &'static str,
        index: usize,
        expected: u32,
        deadline: Deadline,
        clock: &impl Clock,
    ) -> Result<Waited<()>, WaitError> {
        let word = &self.words()[index];
        until(what, clock, Some(deadline), |left| wait_once(word, expected, left))
    }

    /// Wakes every thread of every process sleeping on word `index`.
    pub fn wake_all(&self, index: usize) {
        wake_all(&self.words()[index]);
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: the mapping `map` made, of this length, unmapped once; no `&` to its words
        // outlives `self`.
        _ = unsafe { libc::munmap(self.words.as_ptr().cast(), self.len * WORD) };
    }
}

fn open(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

/// Writes the whole file under a name of this process's, then links it into place: a file at
/// `path` is never seen part-written. A link that finds one there lost the race to another
/// first process, whose file is the one.
fn create(path: &Path, initial: &[u64], len: usize) -> io::Result<()> {
    let mut temp = OsString::from(path.as_os_str());
    // alloc: a file name, once the file is made
    temp.push(format!(".{}.new", std::process::id()));
    let temp = PathBuf::from(temp);
    let written = (|| {
        let mut file =
            OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
        // alloc: the file's bytes, once the file is made (a page)
        let mut bytes = vec![0u8; len * WORD];
        for (chunk, word) in bytes.chunks_exact_mut(WORD).zip(initial) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        file.write_all(&bytes)?;
        match std::fs::hard_link(&temp, path) {
            Ok(()) => Ok(()),
            Err(raced) if raced.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(other) => Err(other),
        }
    })();
    let removed = std::fs::remove_file(&temp);
    written?;
    removed
}

fn map(file: &File, len: usize) -> io::Result<NonNull<AtomicU64>> {
    use std::os::fd::AsRawFd as _;
    // SAFETY: a new shared mapping of the whole file, readable and writable; the kernel picks
    // the address. The descriptor may close after: the mapping holds the file.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len * WORD,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    NonNull::new(at.cast()).ok_or_else(|| io::Error::other("mmap returned the null address"))
}

// --- Linux: the futex, shared ----------------------------------------------------------------

/// One wait on `word`'s low half: `FUTEX_WAIT` without `FUTEX_PRIVATE_FLAG`, so the kernel keys it
/// by the mapped file's page and a wake from another process's mapping finds it. The timeout is
/// relative, the time left of the caller's absolute deadline as `until` computed it just now.
#[cfg(target_os = "linux")]
fn wait_once(word: &AtomicU64, expected: u32, left: Option<Duration>) -> io::Result<Once<()>> {
    let timeout = left.map(|left| libc::timespec {
        tv_sec: libc::time_t::try_from(left.as_secs()).unwrap_or(libc::time_t::MAX),
        tv_nsec: libc::c_long::from(left.subsec_nanos() as i32),
    });
    let timeout_ptr = timeout.as_ref().map_or(std::ptr::null(), std::ptr::from_ref);
    // SAFETY: a futex wait on a live, aligned word of a shared mapping; the kernel only reads it.
    let got = unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr().cast::<u32>(),
            libc::FUTEX_WAIT,
            expected,
            timeout_ptr,
            std::ptr::null::<u32>(),
            0u32,
        )
    };
    if got == 0 {
        return Ok(Once::Ready(()));
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        // The word no longer held `expected`: the caller looks again.
        Some(libc::EAGAIN) => Ok(Once::Ready(())),
        Some(libc::EINTR) => Ok(Once::Interrupted),
        Some(libc::ETIMEDOUT) => Ok(Once::TimedOut),
        // alloc: an error
        _ => Err(io::Error::new(error.kind(), format!("futex(FUTEX_WAIT), shared: {error}"))),
    }
}

#[cfg(target_os = "linux")]
fn wake_all(word: &AtomicU64) {
    // A wake of a live aligned word cannot fail; the count woken is not needed.
    // SAFETY: a futex wake of a word of a shared mapping; the kernel reads no memory of it.
    _ = unsafe {
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr().cast::<u32>(),
            libc::FUTEX_WAKE,
            i32::MAX,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            0u32,
        )
    };
}

// --- macOS: the address wait, shared ---------------------------------------------------------

// macOS's futex across processes (rule macos-private-apis: the public one is the better here).
// `os_sync_wait_on_address_with_timeout` and `os_sync_wake_by_address_all` with the `SHARED`
// flag are the documented face of the private `__ulock_wait2`/`__ulock_wake` with
// `UL_COMPARE_AND_WAIT_SHARED`: the kernel keys the wait by the shared mapping's object, so
// processes that map one file meet on one word, and compares the word as it blocks. They win
// over the `__ulock` calls because their signatures and flags are a header's, where the
// `__ulock` operation numbers are XNU's to change; over a kqueue `EVFILT_USER` or a pipe because
// there is no descriptor to share or to pass, only the word already in the file. The deadline
// form (`_with_deadline`, the one `wait/macos.rs` uses for a park) takes an absolute Mach time
// that module computes privately; this takes the time left of the absolute deadline, computed by
// `until` before each call, so no relative time is carried across a retry. Both move into
// `wait/macos.rs` beside the park's when the ledger's driver lands.
//
// There is no other cross-process wait on macOS (rule no-fallback): the test below resolves each
// symbol at run time, so a system that drops one fails CI naming it.
//
// SAFETY: the signatures are the header's: the flags and `os_clockid_t` are `uint32_t`, `size` a
// `size_t`, the timeout a `uint64_t` of nanoseconds; each returns an `int`, -1 with `errno` set.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn os_sync_wait_on_address_with_timeout(
        addr: *mut libc::c_void,
        value: u64,
        size: usize,
        flags: u32,
        clockid: u32,
        timeout_ns: u64,
    ) -> libc::c_int;
    fn os_sync_wait_on_address(
        addr: *mut libc::c_void,
        value: u64,
        size: usize,
        flags: u32,
    ) -> libc::c_int;
    fn os_sync_wake_by_address_all(addr: *mut libc::c_void, size: usize, flags: u32)
    -> libc::c_int;
}

/// `OS_SYNC_WAIT_ON_ADDRESS_SHARED` and `OS_SYNC_WAKE_BY_ADDRESS_SHARED`: a word other processes
/// map too.
#[cfg(target_os = "macos")]
const SHARED: u32 = 1;
/// `OS_CLOCK_MACH_ABSOLUTE_TIME` (<os/clock.h>): the one clock a timeout may be on.
#[cfg(target_os = "macos")]
const OS_CLOCK_MACH_ABSOLUTE_TIME: u32 = 32;
/// The size of the half waited on.
#[cfg(target_os = "macos")]
const HALF: usize = size_of::<u32>();

#[cfg(target_os = "macos")]
fn wait_once(word: &AtomicU64, expected: u32, left: Option<Duration>) -> io::Result<Once<()>> {
    let addr = word.as_ptr().cast::<libc::c_void>();
    let value = u64::from(expected);
    let got = match left {
        Some(left) => {
            let ns = u64::try_from(left.as_nanos()).unwrap_or(u64::MAX).max(1);
            // SAFETY: a wait on a live, aligned word of a shared mapping; the kernel only reads
            // it, and blocks until a wake of the word, a signal or the timeout.
            unsafe {
                os_sync_wait_on_address_with_timeout(
                    addr,
                    value,
                    HALF,
                    SHARED,
                    OS_CLOCK_MACH_ABSOLUTE_TIME,
                    ns,
                )
            }
        }
        // SAFETY: as above, with no limit (a caller of `Shared::wait` always has one).
        None => unsafe { os_sync_wait_on_address(addr, value, HALF, SHARED) },
    };
    // Woken, or the word no longer held `expected`: the caller looks again.
    if got >= 0 {
        return Ok(Once::Ready(()));
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EINTR) => Ok(Once::Interrupted),
        Some(libc::ETIMEDOUT) => Ok(Once::TimedOut),
        // The header's transient returns under low memory: waited for again.
        Some(libc::ENOMEM | libc::EFAULT) => Ok(Once::Spurious),
        // alloc: an error
        _ => Err(io::Error::new(
            error.kind(),
            format!("os_sync_wait_on_address_with_timeout, shared: {error}"),
        )),
    }
}

#[cfg(target_os = "macos")]
fn wake_all(word: &AtomicU64) {
    // `ENOENT`, nobody waits on the word, is not an error of a wake; nothing else can fail a
    // wake of a live aligned word.
    // SAFETY: a wake of a word of a shared mapping; the kernel reads no memory of it.
    _ = unsafe { os_sync_wake_by_address_all(word.as_ptr().cast(), HALF, SHARED) };
}

// --- Errors ----------------------------------------------------------------------------------

/// What went wrong with which shared file.
#[derive(Debug)]
pub struct SharedError {
    pub path: PathBuf,
    pub why: SharedWhy,
}

#[derive(Debug)]
pub enum SharedWhy {
    Io { op: &'static str, source: io::Error },
    /// The file is not the length asked: another layout's, or no ledger.
    Length { bytes: u64, want: u64 },
    /// A word of the header's identity (its magic, its layout) is not what was asked.
    Identity { word: usize, found: u64, want: u64 },
    /// Another user's file, where only this user's may be.
    Owner { uid: u32, mine: u32 },
}

impl fmt::Display for SharedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match &self.why {
            SharedWhy::Io { op, source } => write!(f, "the shared file {path}: {op}: {source}"),
            SharedWhy::Length { bytes, want } => write!(
                f,
                "the shared file {path} is {bytes} bytes, where it must be {want}: another \
                 layout's or another program's file, left as it is; once no blaze of this user \
                 runs, remove it"
            ),
            SharedWhy::Identity { word, found, want } => write!(
                f,
                "the shared file {path} holds {found:#018x} in its word {word}, where it must hold \
                 {want:#018x}: another layout's or another program's file, left as it is; once no \
                 blaze of this user runs, remove it"
            ),
            SharedWhy::Owner { uid, mine } => write!(
                f,
                "the shared file {path} belongs to user {uid}, and this process runs as {mine}: \
                 refused, as another user's file could count this user's processes wrong"
            ),
        }
    }
}

impl std::error::Error for SharedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.why {
            SharedWhy::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, atomic::Ordering::SeqCst},
        time::Duration,
    };

    use super::{Shared, SharedWhy};
    use crate::wait::{Clock as _, Deadline, Monotonic, Waited};

    const LEN: usize = 16;
    const HEADER: [u64; 4] = [0x6e68_6361_6d7a_6c62, 1, 4096, 8];

    /// A directory of this test's own.
    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-shared-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a test's directory is made");
        dir
    }

    /// The symbols the cross-process wait is built on resolve at run time (rule
    /// macos-private-apis): a macOS without one (before 14.4, or an update that removes it)
    /// fails here, naming it.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_shared_address_waits_resolve() {
        for name in [
            c"os_sync_wait_on_address_with_timeout",
            c"os_sync_wait_on_address",
            c"os_sync_wake_by_address_all",
        ] {
            // SAFETY: a lookup of a NUL-terminated name in every image loaded.
            let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
            assert!(
                !found.is_null(),
                "{name:?} is gone from this macOS (it is there from 14.4): blaze_core::shared \
                 needs it, and there is no other cross-process wait"
            );
        }
    }

    /// The invariant: the first process stamps the header and zeros the rest, and a second
    /// mapping of the file sees it and the first's writes.
    #[test]
    fn the_first_process_stamps_the_header_and_others_see_it() {
        let path = dir("stamp").join("machine.1");
        let first = Shared::open_or_create(&path, &HEADER, LEN, 2).expect("the file is made");
        let words: Vec<u64> = first.words().iter().map(|word| word.load(SeqCst)).collect();
        assert_eq!(&words[..4], &HEADER);
        assert!(words[4..].iter().all(|&word| word == 0), "{words:?}");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), (LEN * 8) as u64);
        // Another header asked: the file's stays, as only its identity is checked.
        let second = Shared::open_or_create(&path, &[HEADER[0], HEADER[1], 1, 1], LEN, 2)
            .expect("the file opens");
        assert_eq!(second.words()[2].load(SeqCst), HEADER[2]);
        first.words()[9].store(77, SeqCst);
        assert_eq!(second.words()[9].load(SeqCst), 77);
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "the file made under a name of its own was left");
    }

    /// The invariant: a file of another length is refused naming it, and left as it is.
    #[test]
    fn a_file_of_another_length_is_refused_and_left_whole() {
        let path = dir("length").join("machine.1");
        std::fs::write(&path, [7u8; 10]).unwrap();
        let Err(error) = Shared::open_or_create(&path, &HEADER, LEN, 2) else {
            panic!("a file of 10 bytes was mapped as {LEN} words");
        };
        assert!(matches!(error.why, SharedWhy::Length { bytes: 10, want: 128 }), "{error}");
        assert!(error.to_string().contains(&path.display().to_string()), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), [7u8; 10], "the file was changed");
    }

    /// The invariant: a file of another identity (another magic or layout) is refused naming the
    /// word.
    #[test]
    fn a_file_of_another_identity_is_refused() {
        let path = dir("identity").join("machine.1");
        drop(Shared::open_or_create(&path, &HEADER, LEN, 2).expect("the file is made"));
        let Err(error) = Shared::open_or_create(&path, &[HEADER[0], 2], LEN, 2) else {
            panic!("a file of layout 1 was mapped as layout 2");
        };
        assert!(matches!(error.why, SharedWhy::Identity { word: 1, found: 1, want: 2 }), "{error}");
    }

    /// A wait on a word that holds what is expected, with no wake, ends at its deadline.
    #[test]
    fn a_wait_nobody_wakes_ends_at_its_deadline() {
        let path = dir("deadline").join("machine.1");
        let shared = Shared::open_or_create(&path, &HEADER, LEN, 2).expect("the file is made");
        let clock = Monotonic::new();
        let wait = Duration::from_millis(30);
        let waited = shared
            .wait("a test's wait nobody wakes", 5, 0, Deadline::after(clock.now(), wait), &clock)
            .unwrap();
        assert_eq!(waited, Waited::Deadline);
        assert!(clock.now() >= wait);
    }

    /// The invariant: a thread asleep on a word through one mapping is woken by a wake through
    /// another mapping of the file (as another process's is), the word unchanged, long before
    /// its deadline.
    #[test]
    fn a_wait_is_woken_through_another_mapping() {
        let path = dir("wake").join("machine.1");
        let waiter_side = Arc::new(Shared::open_or_create(&path, &HEADER, LEN, 2).unwrap());
        let waker_side = Shared::open_or_create(&path, &HEADER, LEN, 2).unwrap();
        let waiter = std::thread::Builder::new()
            .name("shared-wait-test".into())
            .spawn({
                let shared = waiter_side.clone();
                move || {
                    let clock = Monotonic::new();
                    let deadline = Deadline::after(clock.now(), Duration::from_secs(60));
                    let waited = shared.wait("a test's shared wait", 4, 0, deadline, &clock);
                    (waited, clock.now())
                }
            })
            .unwrap();
        let clock = Monotonic::new();
        while !waiter.is_finished() {
            waker_side.wake_all(4);
            let pause = Deadline::after(clock.now(), Duration::from_millis(5));
            crate::wait::sleep_until("a test's pause between wakes", pause, &clock).unwrap();
            assert!(clock.now() < Duration::from_secs(30), "the waiter was never woken");
        }
        let (waited, took) = waiter.join().unwrap();
        assert_eq!(waited.unwrap(), Waited::Ready(()));
        assert!(took < Duration::from_secs(30), "the wait took {took:?}");
        assert_eq!(waiter_side.words()[4].load(SeqCst), 0);
    }
}
