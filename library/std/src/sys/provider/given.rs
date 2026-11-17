//! blaze: the provider a process was given, read from its environment once.

use super::derive::{self, Seed};
use crate::cell::{Cell, UnsafeCell};
use crate::ffi::{CStr, c_char};
use crate::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use crate::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64};
use crate::sync::{Mutex, MutexGuard, PoisonError};

/// Not read yet.
const UNREAD: u8 = 0;
/// The system's provider.
const SYSTEM: u8 = 1;
/// A given provider: [`SEED`], [`WALL`] and [`WALL_GIVEN`] hold it.
const GIVEN: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(UNREAD);
static SEED: [AtomicU64; 4] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static WALL_GIVEN: AtomicBool = AtomicBool::new(false);
static WALL: AtomicI64 = AtomicI64::new(0);
/// Whether this process has told blaze's sandbox that it read the wall clock.
static WALL_SAID: AtomicBool = AtomicBool::new(false);
/// How many children this process has started.
static SPAWNS: AtomicU64 = AtomicU64::new(0);

/// `VAR=`.
const NAME: &[u8] = b"__BLAZE_PROVIDER=";

/// Reads the provider before `main`.
pub fn init() {
    if STATE.load(Acquire) == UNREAD {
        read();
    }
}

/// The given seed; `None` under the system's provider.
#[inline]
fn seed() -> Option<Seed> {
    let state = match STATE.load(Acquire) {
        UNREAD => read(),
        state => state,
    };
    if state != GIVEN {
        return None;
    }
    let mut seed = [0u8; 32];
    for (at, word) in SEED.iter().enumerate() {
        seed[at * 8..at * 8 + 8].copy_from_slice(&word.load(Relaxed).to_le_bytes());
    }
    Some(seed)
}

/// Reads [`NAME`]'s entry of `environ` and keeps what it says: the state it leaves. Two
/// threads that both read it store the same values, so there is no lock on the state; the
/// state is stored last, so whoever sees `GIVEN` sees the seed.
#[cold]
fn read() -> u8 {
    let found = {
        let _guard = crate::sys::env::env_read_lock();
        // SAFETY: `environ` is the process's environment, an array of pointers to
        // nul-terminated strings ended by a null pointer, which nothing changes while the
        // read lock is held (std's own writers take the write lock).
        unsafe {
            let environ = crate::sys::env::environ();
            let mut found = None;
            if !environ.is_null() {
                let mut entry = *environ;
                while !entry.is_null() && !(*entry).is_null() {
                    let bytes = CStr::from_ptr(*entry).to_bytes();
                    if let Some(value) = bytes.strip_prefix(NAME) {
                        found = derive::decode(value);
                        break;
                    }
                    entry = entry.add(1);
                }
            }
            found
        }
    };
    let state = match found {
        // A value this std does not read is another version's: the system's provider, as for
        // a program that was given none.
        None => SYSTEM,
        Some((seed, wall)) => {
            for (at, word) in SEED.iter().enumerate() {
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&seed[at * 8..at * 8 + 8]);
                word.store(u64::from_le_bytes(bytes), Relaxed);
            }
            if let Some(seconds) = wall {
                WALL.store(seconds, Relaxed);
                WALL_GIVEN.store(true, Relaxed);
            }
            GIVEN
        }
    };
    STATE.store(state, Release);
    state
}

/// This thread's number in the derivation: its `ThreadId`, which its spawner drew, so the
/// threads one thread starts one after another have the same numbers on every run.
fn thread() -> u64 {
    crate::thread::current_id().as_u64().get()
}

/// The keys of this thread's `RandomState`s under a given provider.
pub fn hashmap_keys() -> Option<(u64, u64)> {
    Some(derive::keys(&seed()?, thread()))
}

/// Fills `bytes` from this thread's stream under a given provider; false under the system's,
/// with `bytes` as they were.
pub fn fill(bytes: &mut [u8]) -> bool {
    thread_local! {
        /// How many bytes of its stream this thread has taken.
        static TAKEN: Cell<u64> = const { Cell::new(0) };
    }
    let Some(seed) = seed() else { return false };
    let offset = TAKEN.with(|taken| {
        let offset = taken.get();
        // Whole blocks, so the next fill starts on one.
        taken.set(offset.wrapping_add((bytes.len() as u64).next_multiple_of(8)));
        offset
    });
    derive::fill(&seed, derive::FILL, thread(), offset, bytes);
    true
}

/// The seconds since the Unix epoch `SystemTime::now` gives under a provider with a given
/// wall clock; `None` when the clock is the system's.
///
/// The first read says so to blaze's sandbox, which notes that the run read
/// `SOURCE_DATE_EPOCH`, the variable the time comes from: one `access` of a path that never
/// exists (`/.blaze-env/<name>`, the sandbox's mark for a variable read), which only a
/// sandbox that traces sees.
pub fn wall() -> Option<i64> {
    seed()?;
    if !WALL_GIVEN.load(Relaxed) {
        return None;
    }
    if !WALL_SAID.swap(true, Relaxed) {
        // SAFETY: a nul-terminated path; `access` reads it and changes nothing.
        unsafe { libc::access(c"/.blaze-env/SOURCE_DATE_EPOCH".as_ptr(), libc::F_OK) };
    }
    Some(WALL.load(Relaxed))
}

/// `NAME`, the longest value and a nul.
const ENTRY: usize = NAME.len() + derive::VALUE_MOST + 1;

/// The entry of `environ` the next child inherits. Written only under [`SPAWNING`].
struct Entry(UnsafeCell<[u8; ENTRY]>);

// SAFETY: every access is under `SPAWNING`'s lock, but the kernel's and a forked child's
// reads of what a finished spawn left there.
unsafe impl Sync for Entry {}

static CHILD: Entry = Entry(UnsafeCell::new([0; ENTRY]));
static SPAWNING: Mutex<()> = Mutex::new(());

/// Called as a `Command` starts a child: under a given provider, makes this process's
/// [`NAME`] entry of `environ` name the seed of that child (`derive::child` of this
/// process's seed and the spawn's number) and holds the spawns' lock until the guard is
/// dropped, so each child inherits its own. `None` under the system's provider.
///
/// A program that removed the variable from its environment gives its children none: they
/// have the system's provider, and in blaze's sandbox the sandbox answers them.
pub fn spawning() -> Option<MutexGuard<'static, ()>> {
    let seed = seed()?;
    let guard = SPAWNING.lock().unwrap_or_else(PoisonError::into_inner);
    let child = derive::child(&seed, SPAWNS.fetch_add(1, Relaxed));
    let wall = WALL_GIVEN.load(Relaxed).then(|| WALL.load(Relaxed));
    let mut value = [0u8; derive::VALUE_MOST];
    let len = derive::encode(&child, wall, &mut value);
    // SAFETY: the lock is held, so nothing else writes the entry; a child of an earlier
    // spawn has its own copy of it (a forked child's memory, or the kernel's copy at its
    // exec, which `posix_spawn` and the fork's exec both made before that spawn returned).
    let entry = unsafe { &mut *CHILD.0.get() };
    entry[..NAME.len()].copy_from_slice(NAME);
    entry[NAME.len()..NAME.len() + len].copy_from_slice(&value[..len]);
    entry[NAME.len() + len] = 0;
    crate::sys::env::replace_entry(NAME, entry.as_ptr().cast::<c_char>());
    Some(guard)
}
