//! The flight recorder past a crash: the rings, and all a reader needs to decode them, in a
//! file of the process in the history's directory, `flight-<pid>.map`, which the kernel keeps
//! whole when the process dies however it dies (a panic that aborts, a signal, an OOM kill).
//!
//! The file, laid out in [`PAGE`]s so each part maps on its own:
//! - a header: [`MAGIC`], an endianness mark, [`LAYOUT`], [`ROWS_FINGERPRINT`] (every constant
//!   a row's decoding rests on), the pid, the ring sizes, the stores mapped, whether frame
//!   copies were counted, the clock's base ([`clock::Scale::line`], in two slots so a torn
//!   write leaves the last whole one), the time of the last flush, the head of each name log,
//!   blaze's hash and the process's name, the mapping's nonce, and the history's flush marks
//!   (below);
//! - the callsites: a record a site, its name, target and level, written as the site
//!   registers ([`super::register`]);
//! - the builds running: a record a build, written as the history notes it started and freed
//!   as it ends ([`build_started`], [`build_ended`]);
//! - the kept names ([`super::KEPT`]: threads' names, `&'static str`s): one log, appended as a
//!   name is first given and read back newest first. Every other name a row holds is defined
//!   in the rows of its ring ([`super::names`]), so the rings are all a postmortem needs of them;
//! - the rings' [`Store`]s: [`MAPPED_RINGS`] of them, back to back, reserved on disk and
//!   mapped as one when the recorder is persisted, and handed to rings as they are made.
//!
//! Recording costs what it did: a thread writes its store with the same plain atomics, the
//! hot path makes no system call and allocates nothing, and no row holds an address (sites and
//! names are numbers, decoded through the file's own tables). Only a ring's thread writes its
//! store; [`persist`] moves a ring no thread holds itself and leaves the others a store to move
//! to as their thread next writes ([`super::Ring::move_store`]). Every byte the file will hold
//! is reserved as it is made, so [`persist`] is the one place that can fail for want of disk
//! (a typed error naming the file and why); no ring is ever left on the heap once the recorder
//! is persisted: a thread past [`MAPPED_RINGS`] recording at once gets no ring and its spans
//! count as ringless, as past [`super::RINGS_MAX`] in any process, and the next flush says the
//! bound was hit ([`failed`]). Handing out a store, a name or a site writes the file under the
//! lock its caller already holds and does no system call.
//!
//! The file's size is fixed as it is made: the fixed part (about 3.3 MiB: the header 64 KiB,
//! sites 2 MiB, builds 256 KiB, kept names 1 MiB) and [`MAPPED_RINGS`] stores of a [`Store`]
//! each (2 MiB of rows, [`super::CAP`] rows of 64 bytes, and 4 KiB of head), about 516 MiB in
//! all.
//! The history counts it, and the crash records beside it, against its cap
//! ([`crate::history::CAP_BYTES`]).
//!
//! The writer holds an exclusive lock on its file for its life. A reader ([`dead`]) takes the
//! files no one holds, which are of processes that died: their header must be of this layout,
//! or the reader refuses the file with an error naming it ([`PostmortemError::Refused`]), never
//! skipping it. It decodes each store's rows past the history's last flush with the decoder
//! this process's reads use
//! ([`super::decode`]), gives their names and sites numbers of this process (names for the
//! length of one [`Recovered`], sites for the process's life, [`super::foreign_site`]), and
//! hands them to the history, which writes them as rows of the dead process and marks its
//! running builds `crashed`; then the history's core deletes the file.
//!
//! The history's flush marks are two-phase, so a crash between a flush's commit and its mark
//! never has its rows written twice (2026-11-04: they were). Before it commits flush `seq`, the
//! history marks in each store, in a slot neither the last commit's nor the last flush's
//! ([`MARK_SLOTS`]), the first row the flush did not take and the first DEBUG row it still holds
//! for a build not ended (`logs::Hold`); then the builds those held rows are of; then, last,
//! that slot as pending for `seq` ([`mark_flush`]). Its commit names `seq` in the manifest beside
//! the mapping's pid and nonce (`manifest::Recorder`); after the commit the slot is marked
//! committed ([`flush_committed`]). A postmortem reads from the pending slot when the manifest
//! says `seq` committed, from the committed slot otherwise: every row past its marks, and before
//! them the DEBUG rows of the held builds. A postmortem that wrote a mapping's rows names its
//! nonce in the commit that lists them, so a crash before the mapping is deleted never has it
//! read again ([`Found::nonce`]).
//!
//! blaze's binary hash is kept, not checked: the file describes itself (layout, fingerprint,
//! its own callsite and name tables), so a blaze rebuilt after the crash, the common case, reads
//! it; the hash names the blaze the crashed builds ran (`builds.blaze`).

#![cfg_attr(not(unix), allow(dead_code, unused_imports, unused_variables, unused_mut))]

#[cfg(test)]
use std::{fs::File, io::Read as _};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use blaze_hash::HashMap;
use blaze_core::lock::{Lock, LockGuard};
use std::alloc::Global;
use stdx::file_lock::Locked;

use super::{
    CAP, KEPT, MARK_SLOTS, Owner, RINGS, Read, SITES, SLOTS, Store, Sym, TAKEN, WORDS, clock,
    decode, lock, names, span,
};
use crate::logs::HOLD_BUILDS;

// --- Layout -----------------------------------------------------------------------------

/// Every part of the file starts at a multiple of this: a page on every system blaze runs on
/// divides it (4 KiB on x86_64, 16 KiB on Apple silicon, up to 64 KiB on some aarch64 Linux).
const PAGE: usize = 1 << 16;
/// `BLZFLGHT`, little-endian.
const MAGIC: u64 = u64::from_le_bytes(*b"BLZFLGHT");
/// Read back as written only on a machine of the writer's byte order.
const ENDIAN: u64 = 0x0102_0304_0506_0708;
/// The layout of the file. Bump it with any change to the parts below, the header's words or
/// a [`Store`]'s.
pub(crate) const LAYOUT: u64 = 4;

const SITE_RECORD: usize = 512;
const SITES_AT: usize = PAGE;
const SITES_BYTES: usize = (SLOTS * SITE_RECORD).div_ceil(PAGE) * PAGE;
/// Builds a process runs at once that a postmortem names.
const BUILD_SLOTS: usize = 32;
const BUILD_RECORD: usize = 8192;
const BUILDS_AT: usize = SITES_AT + SITES_BYTES;
const BUILDS_BYTES: usize = BUILD_SLOTS * BUILD_RECORD;
/// The one log of kept names (threads'); in layout 3 also a log a shard of the shared names,
/// which names defined in the rings replaced ([`super::names`]).
const LOGS: usize = 1;
const LOG_BYTES: usize = 1 << 20;
const LOGS_AT: usize = BUILDS_AT + BUILDS_BYTES;
/// Where the first ring's store is: what is mapped as one, the fixed part.
const RINGS_AT: usize = LOGS_AT + LOGS * LOG_BYTES;
/// Threads a persisted process records on at once: the stores reserved up front. A ring is
/// 2 MiB of rows ([`super::CAP`]), which the drain needs (a busy worker writes thousands of
/// rows between two drains), so the bound is on rings, not their size: [`super::RINGS_MAX`]
/// (4094) of them would reserve 8 GiB. A daemon records on its pool's threads, its IO and
/// history threads and the threads of the crates it runs, well under this on the machines
/// blaze runs on; past it a thread is not recorded and the next flush says so.
pub(crate) const MAPPED_RINGS: usize = 256;
/// A store's stride in the file: the stores are one mapping, so they need no page each, only
/// a `Store`'s alignment.
const STORE_BYTES: usize = std::mem::size_of::<Store>().div_ceil(8) * 8;
/// The stores' part of the file, whole pages.
const STORES_BYTES: usize = (MAPPED_RINGS * STORE_BYTES).div_ceil(PAGE) * PAGE;
/// The file's length, reserved as it is made.
pub(crate) const FILE_BYTES: u64 = (RINGS_AT + STORES_BYTES) as u64;
/// Bytes of a name a log keeps; a longer one is cut at a character's boundary and marked.
const NAME_MAX: usize = 4096;
/// The longest name and target a site record keeps.
const SITE_TEXT: usize = SITE_RECORD - 8;
const SITE_TARGET_MAX: usize = 200;
const BUILD_COMMIT_MAX: usize = 128;
const BUILD_WORKSPACE_MAX: usize = 2048;
const BUILD_COMMAND_MAX: usize = BUILD_RECORD - 32 - BUILD_COMMIT_MAX - BUILD_WORKSPACE_MAX;
const PROCESS_MAX: usize = 1024;
const BLAZE_MAX: usize = 64;

const _: () = assert!(SITES_BYTES % PAGE == 0 && BUILDS_BYTES % PAGE == 0 && LOG_BYTES % PAGE == 0);
const _: () = assert!(STORE_BYTES >= std::mem::size_of::<Store>() && STORE_BYTES % 8 == 0);
const _: () = assert!(MAPPED_RINGS <= super::RINGS_MAX);
const _: () = assert!(NAME_MAX + 8 < LOG_BYTES);

// The header's words, by index.
const H_MAGIC: usize = 0;
const H_ENDIAN: usize = 1;
const H_LAYOUT: usize = 2;
const H_FINGERPRINT: usize = 3;
const H_PID: usize = 4;
const H_CAP: usize = 5;
const H_WORDS: usize = 6;
const H_STORE_BYTES: usize = 7;
/// Stores mapped: each is whole (zeroed and numbered) before this counts it.
const H_RINGS: usize = 8;
const H_COPIES: usize = 9;
/// Which of the two clock slots is whole: 0 or 1.
const H_CLOCK_AT: usize = 10;
/// Two slots of three words ([`clock::Scale::line`]).
const H_CLOCK: usize = 11;
/// The read time of the last flush, on the schema's clock: a crashed build ran at least to it.
const H_FLUSHED_NS: usize = 17;
const H_LOG_HEADS: usize = 24;
const H_BLAZE_LEN: usize = 46;
const H_PROCESS_LEN: usize = 47;
const H_BLAZE: usize = 48;
const H_PROCESS: usize = H_BLAZE + BLAZE_MAX / 8;
/// The mapping's nonce: told from a mapping of a later process given the same pid.
const H_NONCE: usize = 256;
/// The slot the last flush marked, pending its commit, and the slot the last commit marked:
/// each `seq << 2 | slot`, `seq` 0 for none.
const H_PENDING: usize = 257;
const H_COMMITTED: usize = 258;
/// A slot's builds whose DEBUG rows the flush held: their count, then each build.
const H_HELD: usize = 260;
const HELD_WORDS: usize = 1 + HOLD_BUILDS;
const _: () = assert!(H_LOG_HEADS + LOGS <= H_BLAZE_LEN && (H_PROCESS * 8 + PROCESS_MAX) <= H_NONCE * 8);
const _: () = assert!((H_HELD + MARK_SLOTS * HELD_WORDS) * 8 <= PAGE && MARK_SLOTS <= 4);

/// A pending or committed slot's word: its flush and its slot.
const fn marked(word: u64) -> Option<(u64, usize)> {
    match word >> 2 {
        0 => None,
        seq => Some((seq, (word & 3) as usize)),
    }
}

/// The levels a site record names, by their index.
const LEVELS: [&str; 5] = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"];

const fn fnv(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut at = 0;
    while at < bytes.len() {
        hash ^= bytes[at] as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
        at += 1;
    }
    hash
}

const fn fnv_word(hash: u64, word: u64) -> u64 {
    fnv(hash, &word.to_le_bytes())
}

/// Every constant the decoding of a row rests on: the bits of a row's words, the plot bytes and
/// column ids, the plots, switches and waits by number, the span columns by place (and which
/// hold names), and the bits of a [`Sym`]. A file whose rows were written under other ones is
/// refused, not misread.
pub(crate) const ROWS_FINGERPRINT: u64 = {
    use super::{
        ALLOCS_BITS, BYTES_MAX, COL_WORDS, CPU_COL, CPU_READ, CPU_ROW, CPU_VALID, CpuWhy,
        DEPTH_BITS, EVENT_FIELDS_ROW, EVENT_ROW, FIBER_STACK_ROW, FIELD_SLOT_BITS,
        FIELD_SLOT_SHIFT, FOLD_ALLOCS, FOLD_CPU, FOLD_PARENT_SHIFT, FOLD_ROOT, FOLD_ROW,
        FOLD_SPANS_MAX, FOLD_SPANS_SHIFT, FOLDED_COL, FOLDED_TICKS_COL, FieldValue, KIND_MARK,
        KIND_PLOT, KIND_SPAN, NO_COL, OWNERS_KEPT, Plot, QUEUED_LOW_SHIFT, RING_BITS, SHARE_SHIFT,
        SITE_BITS, SLOT_BITS, SPAN_COLS_ROW, SPAN_COPIED_ROW, STORE_HEAD, SWITCH_ROW, Switched,
        THREAD_COL, UNCOUNTED, WaitedOn,
    };
    use crate::time::cpu::{SHARE_BITS, SHARE_COUNTED, SHARE_ONE};
    let mut hash = 0xcbf2_9ce4_8422_2325;
    let words = [
        KIND_SPAN,
        KIND_PLOT,
        KIND_MARK,
        SITE_BITS as u64,
        DEPTH_BITS as u64,
        RING_BITS as u64,
        SLOT_BITS as u64,
        ALLOCS_BITS as u64,
        FIBER_STACK_ROW,
        SPAN_COPIED_ROW,
        SWITCH_ROW,
        EVENT_ROW,
        EVENT_FIELDS_ROW,
        FIELD_SLOT_SHIFT as u64,
        FIELD_SLOT_BITS as u64,
        // A fields row's value kinds, as its slots say them.
        FieldValue::I64(0).kind(),
        FieldValue::U64(0).kind(),
        FieldValue::F64(0.0).kind(),
        FieldValue::Bool(false).kind(),
        FieldValue::Str(0).kind(),
        SPAN_COLS_ROW,
        FOLD_ROW,
        THREAD_COL,
        CPU_COL,
        NO_COL,
        FOLDED_COL,
        FOLDED_TICKS_COL,
        UNCOUNTED,
        QUEUED_LOW_SHIFT as u64,
        FOLD_PARENT_SHIFT as u64,
        FOLD_SPANS_SHIFT as u64,
        FOLD_SPANS_MAX,
        FOLD_ROOT,
        FOLD_CPU,
        FOLD_ALLOCS,
        WORDS as u64,
        OWNERS_KEPT as u64,
        STORE_HEAD as u64,
        // Names: a row's own ids, its ring's definitions and links ([`super::names`]), and a
        // kept name's class.
        names::GEN_ROW,
        names::NAME_ROW,
        names::NAME_MORE_ROW,
        names::LINK_ROW,
        names::KEPT_SYM as u64,
        names::INDEX_MAX as u64,
        names::WINDOW as u64,
        // A boundary's row, and a span row's CPU share above its allocated bytes.
        CPU_ROW,
        CPU_VALID,
        CPU_READ,
        SHARE_SHIFT as u64,
        BYTES_MAX,
        SHARE_BITS as u64,
        SHARE_COUNTED,
        SHARE_ONE,
        CpuWhy::Switch as u64,
        CpuWhy::Wait as u64,
        CpuWhy::Gap as u64,
        CpuWhy::Root as u64,
        CpuWhy::Age as u64,
        CpuWhy::Claim as u64,
        CpuWhy::End as u64,
        CpuWhy::Mark as u64,
        CpuWhy::Read as u64,
        CpuWhy::Fold as u64,
        CpuWhy::Left as u64,
        CpuWhy::Park as u64,
    ];
    let mut at = 0;
    while at < words.len() {
        hash = fnv_word(hash, words[at]);
        at += 1;
    }
    let mut at = 0;
    while at < COL_WORDS.len() {
        hash = fnv_word(hash, COL_WORDS[at] as u64);
        at += 1;
    }
    let mut at = 0;
    while at < Plot::ALL.len() {
        hash = fnv_word(hash, Plot::ALL[at] as u64);
        hash = fnv(hash, Plot::ALL[at].name().as_bytes());
        at += 1;
    }
    let mut at = 0;
    while at < Switched::ALL.len() {
        hash = fnv_word(hash, Switched::ALL[at] as u64);
        hash = fnv(hash, Switched::ALL[at].name().as_bytes());
        at += 1;
    }
    let waits = [
        WaitedOn::Job,
        WaitedOn::Key,
        WaitedOn::Counter,
        WaitedOn::Until,
        WaitedOn::Moved,
        WaitedOn::Nothing,
    ];
    let mut at = 0;
    while at < waits.len() {
        hash = fnv_word(hash, waits[at] as u64);
        hash = fnv(hash, waits[at].name().as_bytes());
        at += 1;
    }
    let mut at = 0;
    while at < span::GENERIC_COLUMNS.len() {
        hash = fnv(hash, span::GENERIC_COLUMNS[at].name.as_bytes());
        at += 1;
    }
    let mut at = 0;
    while at < span::TYPED_COLUMNS.len() {
        hash = fnv(hash, span::TYPED_COLUMNS[at].name.as_bytes());
        at += 1;
    }
    let mut at = 0;
    while at < span::SYM_COLUMNS.len() {
        hash = fnv_word(hash, span::SYM_COLUMNS[at] as u64);
        at += 1;
    }
    hash
};

/// The file of process `pid` in a history's directory.
pub(crate) fn file_name(pid: u32) -> String {
    // alloc: a file's name, once a mapping made or found
    format!("flight-{pid}.map")
}

/// The pid of a file named as [`file_name`] names one.
pub(crate) fn pid_of(name: &str) -> Option<u32> {
    name.strip_prefix("flight-")?.strip_suffix(".map")?.parse().ok()
}

// --- Errors -----------------------------------------------------------------------------

/// Why the recorder's rows could not be kept past a crash, or a dead process's read back.
#[derive(Debug)]
pub enum PostmortemError {
    Io {
        path: PathBuf,
        doing: &'static str,
        error: io::Error,
    },
    /// The file at `path` is not of this layout: its `what` is `found`, this blaze reads
    /// `expected`. Refused, never skipped.
    Refused {
        path: PathBuf,
        what: &'static str,
        found: u64,
        expected: u64,
    },
    /// A part of the file at `path` does not decode: a length past its room, a clock never
    /// written beside rings that were.
    Torn {
        path: PathBuf,
        what: &'static str,
    },
    /// This system can't keep the rows past a crash.
    Unsupported {
        path: PathBuf,
        why: &'static str,
    },
    /// More than `most` of `what` at once: those past it are not kept past a crash (a write),
    /// or not read back (a read).
    Full {
        path: PathBuf,
        what: &'static str,
        most: usize,
    },
    /// The recorder is persisted already, to `path`: once a process.
    AlreadyPersisted {
        path: PathBuf,
    },
}

impl std::fmt::Display for PostmortemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PostmortemError::Io { path, doing, error } => {
                write!(f, "the flight recorder's mapping {}: {doing}: {error}", path.display())
            }
            PostmortemError::Refused { path, what, found, expected } => write!(
                f,
                "the flight recorder's mapping {} is of another layout (its {what} is {found:#x}, this blaze reads \
                 {expected:#x}), so the rows of the process that died with it can't be read: refused, not skipped \
                 (no-fallback); read it with the blaze that wrote it, or delete it to go on without them",
                path.display()
            ),
            PostmortemError::Torn { path, what } => write!(
                f,
                "the flight recorder's mapping {}: {what} does not decode (a damaged file): refused, not skipped \
                 (no-fallback); delete it to go on without the rows of the process that died with it",
                path.display()
            ),
            PostmortemError::Unsupported { path, why } => {
                write!(
                    f,
                    "the flight recorder's rows can't be kept past a crash in {}: {why}",
                    path.display()
                )
            }
            PostmortemError::Full { path, what, most } => write!(
                f,
                "the flight recorder's mapping {}: more than {most} {what} at once, and those past them are not kept",
                path.display()
            ),
            PostmortemError::AlreadyPersisted { path } => {
                write!(
                    f,
                    "the flight recorder is persisted already, to {}: once a process",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for PostmortemError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PostmortemError::Io { error, .. } => Some(error),
            _ => None,
        }
    }
}

fn io_error<'a>(path: &'a Path, doing: &'static str) -> impl FnOnce(io::Error) -> PostmortemError + 'a {
    // alloc: an error
    move |error| PostmortemError::Io { path: path.to_owned(), doing, error }
}

// --- The writer -------------------------------------------------------------------------

/// The file of a live process: the fixed part mapped, and its stores, handed to rings as they
/// are made.
pub(crate) struct Mapping {
    path: PathBuf,
    /// The file, locked exclusively for the mapping's life: a reader takes a file it can lock
    /// as one whose process died.
    file: Locked,
    /// The fixed part, `0..RINGS_AT`, mapped shared and never unmapped.
    fixed: std::ptr::NonNull<u8>,
    /// The kept names' log's bytes ever written, taken under the kept table's lock: a name is
    /// kept only the first time a thread asks for it, never on a thread's hot path.
    log: Lock<u64>,
    /// One writer of the site records at a time.
    sites: Lock<()>,
    /// The build each build record holds, 0 for none.
    builds: Lock<[u64; BUILD_SLOTS]>,
    /// The stores' part, `RINGS_AT..FILE_BYTES`, mapped shared and never unmapped.
    stores: std::ptr::NonNull<u8>,
    /// Stores handed to rings.
    given: Lock<usize>,
    /// Its nonce ([`H_NONCE`]).
    nonce: u64,
    /// One flush marks at a time ([`mark_flush`]).
    marking: Lock<()>,
}

// SAFETY: `fixed` and `stores` point into shared mappings that are never unmapped; a store is
// handed to one ring and written through atomics only; every write through `fixed`
// is either to an atomic word (`Mapping::word`) or to bytes of a log, a site record or a build
// record under the `Lock` that keeps that part to one writer, and no thread of this process
// reads those bytes (only a postmortem, in another process, after this one died).
unsafe impl Send for Mapping {}
// SAFETY: as for `Send`.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Makes the file of process `pid` in `dir`, locks it, reserves all of it on disk
    /// ([`FILE_BYTES`]) and maps it, and writes its header, the magic last.
    pub(crate) fn create(
        dir: &Path,
        pid: u32,
        blaze: &str,
        process: &str,
        counting_copies: bool,
    ) -> Result<Mapping, PostmortemError> {
        let path = dir.join(file_name(pid));
        #[cfg(not(unix))]
        {
            _ = (blaze, process, counting_copies);
            return Err(PostmortemError::Unsupported {
                path,
                why: "a mapping of the flight recorder needs `mmap`, which this blaze uses on unix only",
            });
        }
        #[cfg(unix)]
        {
            let page = sys::page_size();
            if page == 0 || PAGE % page != 0 {
                return Err(PostmortemError::Unsupported {
                    path,
                    why: "the system's page size does not divide 64 KiB",
                });
            }
            fs::create_dir_all(dir).map_err(io_error(dir, "making the history's directory"))?;
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(io_error(&path, "opening"))?;
            let file = match Locked::try_lock(file).map_err(io_error(&path, "locking"))? {
                Ok(file) => file,
                Err(_held) => return Err(PostmortemError::AlreadyPersisted { path }),
            };
            // A file of a dead process of this pid that no postmortem read: its rows are lost
            // here, as the history opened before this and read every file no one held.
            file.set_len(0).map_err(io_error(&path, "emptying"))?;
            // Every ring's store with the fixed part: a full disk is this error, never a SIGBUS
            // on a page a ring writes later, and never a ring left on the heap.
            if let Err(error) = sys::reserve(&file, 0, FILE_BYTES) {
                _ = fs::remove_file(&path);
                return Err(io_error(
                    &path,
                    "reserving the disk of every ring's store (about 516 MiB) for",
                )(error));
            }
            let fixed = sys::map(&file, 0, RINGS_AT)
                .map_err(io_error(&path, "mapping the fixed part of"))?;
            let stores = sys::map(&file, RINGS_AT as u64, STORES_BYTES)
                .map_err(io_error(&path, "mapping the rings' stores of"))?;
            let mapping = Mapping {
                path,
                file,
                fixed,
                log: Lock::unrecorded("profile.postmortem.kept_log", 0),
                sites: Lock::unrecorded("profile.postmortem.sites", ()),
                builds: Lock::unrecorded("profile.postmortem.builds", [0; BUILD_SLOTS]),
                stores,
                given: Lock::unrecorded("profile.postmortem.given", 0),
                nonce: nonce(pid),
                marking: Lock::unrecorded("profile.postmortem.marking", ()),
            };
            mapping.header(pid, blaze, process, counting_copies);
            Ok(mapping)
        }
    }

    fn header(&self, pid: u32, blaze: &str, process: &str, counting_copies: bool) {
        let words = [
            (H_ENDIAN, ENDIAN),
            (H_LAYOUT, LAYOUT),
            (H_FINGERPRINT, ROWS_FINGERPRINT),
            (H_PID, u64::from(pid)),
            (H_CAP, CAP as u64),
            (H_WORDS, WORDS as u64),
            (H_STORE_BYTES, STORE_BYTES as u64),
            (H_COPIES, u64::from(counting_copies)),
            (H_NONCE, self.nonce),
        ];
        for (at, value) in words {
            self.word(at).store(value, Ordering::Relaxed);
        }
        let blaze = cut(blaze, BLAZE_MAX).0;
        self.bytes(H_BLAZE * 8, blaze.as_bytes());
        self.word(H_BLAZE_LEN).store(blaze.len() as u64, Ordering::Relaxed);
        let process = cut(process, PROCESS_MAX).0;
        self.bytes(H_PROCESS * 8, process.as_bytes());
        self.word(H_PROCESS_LEN).store(process.len() as u64, Ordering::Relaxed);
        self.word(H_MAGIC).store(MAGIC, Ordering::Release);
    }

    /// Word `at` of the header.
    fn word(&self, at: usize) -> &AtomicU64 {
        self.at(at * 8)
    }

    /// The word at byte `at` of the fixed part.
    fn at(&self, at: usize) -> &AtomicU64 {
        assert!(at % 8 == 0 && at + 8 <= RINGS_AT, "a word of the fixed part");
        // SAFETY: `at` is 8-aligned and in the fixed part (checked above), which is mapped for
        // the mapping's life from a page-aligned base; an `AtomicU64` has no invalid bytes.
        unsafe { &*self.fixed.as_ptr().add(at).cast::<AtomicU64>() }
    }

    /// Writes `bytes` at byte `at` of the fixed part.
    fn bytes(&self, at: usize, bytes: &[u8]) {
        assert!(at + bytes.len() <= RINGS_AT, "bytes of the fixed part");
        // SAFETY: the range is in the fixed part (checked above); the caller holds the lock of
        // the part it writes, so no other thread writes these bytes, and none reads them.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.fixed.as_ptr().add(at), bytes.len())
        };
    }

    /// Notes the clock's base: in the slot not in use, then flips to it.
    pub(crate) fn set_clock(&self, scale: &clock::Scale) {
        let next = 1 - (self.word(H_CLOCK_AT).load(Ordering::Relaxed) & 1) as usize;
        for (at, value) in scale.line().into_iter().enumerate() {
            self.word(H_CLOCK + next * 3 + at).store(value, Ordering::Relaxed);
        }
        self.word(H_CLOCK_AT).store(next as u64, Ordering::Release);
    }

    /// The next store, for the ring `number`: reserved and mapped as the file was made, so
    /// handing it out does no system call; [`PostmortemError::Full`] past [`MAPPED_RINGS`].
    pub(crate) fn store(&self, number: u16) -> Result<&'static Store, PostmortemError> {
        let mut given = self.given.lock();
        if *given == MAPPED_RINGS {
            return Err(PostmortemError::Full {
                // alloc: an error
                path: self.path.clone(),
                what: "threads recording at once (a thread past them is not recorded)",
                most: MAPPED_RINGS,
            });
        }
        // SAFETY: `*given < MAPPED_RINGS`, so the store's `STORE_BYTES` lie inside the stores'
        // mapping (`MAPPED_RINGS * STORE_BYTES <= STORES_BYTES`), 8-aligned (the base is page
        // aligned and `STORE_BYTES` a multiple of 8, a `Store`'s alignment at most 8), of bytes
        // the file reserved as zeros; the mapping is never unmapped, all-zero is a valid `Store`
        // (atomics and integers), and `given` hands each one to one ring alone.
        let store: &'static Store =
            unsafe { &*self.stores.as_ptr().add(*given * STORE_BYTES).cast::<Store>() };
        store.number.store(u64::from(number), Ordering::Relaxed);
        *given += 1;
        self.word(H_RINGS).store(*given as u64, Ordering::Release);
        Ok(store)
    }

    /// Logs the kept name `name` as `sym`, under [`super::KEPT`]'s lock.
    pub(crate) fn log_name(&self, sym: Sym, name: &str) {
        let mut head = self.log.lock();
        self.write_name(LOGS_AT, &mut head, sym, name);
    }

    /// Writes one entry of the log at `base`, its head `head`: the name's bytes, then its
    /// trailer, then the head published.
    fn write_name(&self, base: usize, head: &mut u64, sym: Sym, name: &str) {
        let log = (base - LOGS_AT) / LOG_BYTES;
        let (text, cut_short) = cut(name, NAME_MAX);
        let padded = text.len().div_ceil(8) * 8;
        self.ring_bytes(base, *head, text.as_bytes());
        let trailer = u64::from(sym) << 32 | u64::from(cut_short) << 31 | text.len() as u64;
        self.ring_bytes(base, *head + padded as u64, &trailer.to_ne_bytes());
        *head += padded as u64 + 8;
        self.word(H_LOG_HEADS + log).store(*head, Ordering::Release);
    }

    /// Writes `bytes` at byte `at` of the log at `base`, wrapping at [`LOG_BYTES`].
    fn ring_bytes(&self, base: usize, at: u64, bytes: &[u8]) {
        let at = (at % LOG_BYTES as u64) as usize;
        let first = bytes.len().min(LOG_BYTES - at);
        self.bytes(base + at, &bytes[..first]);
        self.bytes(base, &bytes[first..]);
    }

    /// Writes the record of site `site`: its name, target and level.
    pub(crate) fn site(&self, site: u16, name: &str, target: &str, level: &str) {
        let Some(at) = usize::from(site).checked_sub(1).filter(|it| *it < SLOTS) else { return };
        let _one = self.sites.lock();
        let (target, target_cut) = cut(target, SITE_TARGET_MAX);
        let (name, name_cut) = cut(name, SITE_TEXT - target.len());
        let base = SITES_AT + at * SITE_RECORD;
        self.at(base).store(0, Ordering::Relaxed);
        self.bytes(base + 8, name.as_bytes());
        self.bytes(base + 8 + name.len(), target.as_bytes());
        let level = LEVELS.iter().position(|it| *it == level).unwrap_or(LEVELS.len() - 1) as u64;
        let record = 1 << 63
            | u64::from(name_cut || target_cut) << 62
            | level << 48
            | (target.len() as u64) << 16
            | name.len() as u64;
        self.at(base).store(record, Ordering::Release);
    }

    /// Notes the build `build` running from `started_ns`.
    pub(crate) fn build_started(&self, build: &RunningBuild<'_>) -> Result<(), PostmortemError> {
        let mut slots = self.builds.lock();
        let at = match slots.iter().position(|it| *it == build.build_id) {
            Some(at) => at,
            None => match slots.iter().position(|it| *it == 0) {
                Some(at) => at,
                None => {
                    return Err(PostmortemError::Full {
                        // alloc: an error
                        path: self.path.clone(),
                        what: "builds running",
                        most: BUILD_SLOTS,
                    });
                }
            },
        };
        let base = BUILDS_AT + at * BUILD_RECORD;
        self.at(base).store(0, Ordering::Release);
        let (commit, has_commit) = match build.commit {
            Some(commit) => (cut(commit, BUILD_COMMIT_MAX).0, true),
            None => ("", false),
        };
        let workspace = cut(build.workspace, BUILD_WORKSPACE_MAX).0;
        let command = cut(build.command, BUILD_COMMAND_MAX).0;
        self.at(base + 8).store(build.started_ns as u64, Ordering::Relaxed);
        self.at(base + 16)
            .store(build.started_unix_ns.unwrap_or(i64::MIN) as u64, Ordering::Relaxed);
        let lens = commit.len() as u64
            | u64::from(has_commit) << 8
            | (workspace.len() as u64) << 16
            | (command.len() as u64) << 32;
        self.at(base + 24).store(lens, Ordering::Relaxed);
        self.bytes(base + 32, commit.as_bytes());
        self.bytes(base + 32 + commit.len(), workspace.as_bytes());
        self.bytes(base + 32 + commit.len() + workspace.len(), command.as_bytes());
        self.at(base).store(build.build_id, Ordering::Release);
        slots[at] = build.build_id;
        Ok(())
    }

    /// Notes the build `build` ended: its record is free.
    pub(crate) fn build_ended(&self, build: u64) {
        let mut slots = self.builds.lock();
        if let Some(at) = slots.iter().position(|it| *it == build) {
            self.at(BUILDS_AT + at * BUILD_RECORD).store(0, Ordering::Release);
            slots[at] = 0;
        }
    }

    /// Marks flush `seq` pending: `marks` writes each store's marks in the slot given, then
    /// the builds whose DEBUG rows it held are noted in that slot, then the slot is published
    /// as pending. The slot is neither the last commit's nor the last flush's.
    pub(crate) fn mark_flush(&self, seq: u64, held_builds: &[u32], marks: impl FnOnce(usize)) {
        let _one = self.marking.lock();
        let taken = [H_PENDING, H_COMMITTED]
            .map(|at| marked(self.word(at).load(Ordering::Acquire)).map(|it| it.1));
        let slot = (0..MARK_SLOTS).find(|it| !taken.contains(&Some(*it))).unwrap_or(0);
        marks(slot);
        let held = &held_builds[..held_builds.len().min(HOLD_BUILDS)];
        let base = H_HELD + slot * HELD_WORDS;
        for (at, build) in held.iter().enumerate() {
            self.word(base + 1 + at).store(u64::from(*build), Ordering::Relaxed);
        }
        self.word(base).store(held.len() as u64, Ordering::Relaxed);
        self.word(H_PENDING).store(seq << 2 | slot as u64, Ordering::SeqCst);
    }

    /// Marks flush `seq`, pending, committed; `at_ns` the read that took its rows.
    pub(crate) fn flush_committed(&self, seq: u64, at_ns: i64) {
        {
            let _one = self.marking.lock();
            let pending = self.word(H_PENDING).load(Ordering::Acquire);
            if marked(pending).is_some_and(|it| it.0 == seq) {
                self.word(H_COMMITTED).store(pending, Ordering::SeqCst);
            }
        }
        self.flushed_at(at_ns);
    }

    /// Notes the read time of the flush that just wrote, with the clock's base now.
    pub(crate) fn flushed_at(&self, at_ns: i64) {
        self.set_clock(&clock::Scale::now());
        self.word(H_FLUSHED_NS).store(at_ns as u64, Ordering::Release);
    }
}

/// A mapping's nonce: never 0, and another for each mapping a process of `pid` makes.
fn nonce(pid: u32) -> u64 {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |it| it.as_nanos() as u64);
    let words = [unix, clock::ticks(), u64::from(pid)];
    let hash = words.iter().fold(0xcbf2_9ce4_8422_2325, |hash, word| fnv_word(hash, *word));
    hash | 1
}

/// `text` cut to at most `max` bytes at a character's boundary, and whether it was cut.
fn cut(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// A build as the history notes it started ([`build_started`]).
#[derive(Clone, Copy, Debug)]
pub struct RunningBuild<'a> {
    pub build_id: u64,
    pub started_ns: i64,
    pub started_unix_ns: Option<i64>,
    pub commit: Option<&'a str>,
    pub workspace: &'a str,
    pub command: &'a str,
}

#[cfg(unix)]
mod sys {
    use std::{fs::File, io, os::fd::AsRawFd as _, ptr::NonNull};

    pub(super) fn page_size() -> usize {
        // SAFETY: `sysconf` reads a constant of the system.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(page).unwrap_or(0)
    }

    /// Maps `len` bytes of `file` from `at`, shared, to read and write.
    pub(super) fn map(file: &File, at: u64, len: usize) -> io::Result<NonNull<u8>> {
        let offset =
            libc::off_t::try_from(at).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: a fresh mapping at an address the kernel picks: it aliases no Rust object.
        // The file is open to read and write, and `at` is a multiple of the page (the caller's
        // layout is in 64 KiB parts, which the page divides).
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                offset,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        NonNull::new(base.cast::<u8>())
            .ok_or_else(|| io::Error::from(io::ErrorKind::AddrNotAvailable))
    }

    /// Reserves `from..to` of `file` on disk, and makes the file that long: a page of a shared
    /// mapping written past what the disk holds kills the process with SIGBUS, so the disk is
    /// taken up front, where a full disk is an error.
    #[cfg(target_os = "linux")]
    pub(super) fn reserve(file: &File, from: u64, to: u64) -> io::Result<()> {
        let invalid = |_| io::Error::from(io::ErrorKind::InvalidInput);
        let (offset, len) = (
            libc::off_t::try_from(from).map_err(invalid)?,
            libc::off_t::try_from(to - from).map_err(invalid)?,
        );
        // SAFETY: `posix_fallocate` on an open file descriptor; it returns its error.
        let failed = unsafe { libc::posix_fallocate(file.as_raw_fd(), offset, len) };
        match failed {
            0 => Ok(()),
            errno => Err(io::Error::from_raw_os_error(errno)),
        }
    }

    #[cfg(target_vendor = "apple")]
    pub(super) fn reserve(file: &File, from: u64, to: u64) -> io::Result<()> {
        let len = libc::off_t::try_from(to - from)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut store = libc::fstore_t {
            fst_flags: libc::F_ALLOCATECONTIG,
            fst_posmode: libc::F_PEOFPOSMODE,
            fst_offset: 0,
            fst_length: len,
            fst_bytesalloc: 0,
        };
        // SAFETY: `F_PREALLOCATE` reads and writes the `fstore_t` given, which lives across the
        // call.
        if unsafe {
            libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, std::ptr::from_mut(&mut store))
        } == -1
        {
            // No contiguous run that long: any run.
            store.fst_flags = libc::F_ALLOCATEALL;
            // SAFETY: as above.
            if unsafe {
                libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, std::ptr::from_mut(&mut store))
            } == -1
            {
                return Err(io::Error::last_os_error());
            }
        }
        file.set_len(to)
    }

    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    pub(super) fn reserve(_file: &File, _from: u64, _to: u64) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "reserving a file's disk is written for Linux and macOS only",
        ))
    }
}

// --- The process's mapping --------------------------------------------------------------

/// This process's mapping, once [`persist`] made it.
static MAPPING: OnceLock<Mapping> = OnceLock::new();
/// What failed to be kept since the history last asked ([`failed`]): a store not made, a build
/// past [`BUILD_SLOTS`].
static FAILED: Lock<Option<PostmortemError>> = Lock::unrecorded("profile.postmortem.failed", None);

fn failing(error: PostmortemError) {
    let mut failed = lock(&FAILED);
    if failed.is_none() {
        *failed = Some(error);
    }
}

/// What failed to be kept past a crash since the last call: the history answers it with its
/// next flush.
pub fn failed() -> Option<PostmortemError> {
    lock(&FAILED).take()
}

/// Whether this process's rings are in a mapping.
pub(crate) fn persisted() -> bool {
    MAPPING.get().is_some()
}

/// See [`super::persist`].
pub(super) fn persist(dir: &Path, blaze: &str, process: &str) -> Result<(), PostmortemError> {
    if let Some(mapping) = MAPPING.get() {
        // alloc: an error
        return Err(PostmortemError::AlreadyPersisted { path: mapping.path.clone() });
    }
    let counting_copies = super::FRAME_COPIED.get().is_some();
    let made = Mapping::create(dir, std::process::id(), blaze, process, counting_copies)?;
    made.set_clock(&clock::Scale::now());
    {
        // Under `RINGS`'s lock no thread makes a ring: every ring made so far gets its store
        // before any made from here on (`store_for`) can take one.
        let rings = lock(&RINGS);
        if rings.len() > MAPPED_RINGS {
            let path = made.path.clone(); // alloc: an error
            drop(made);
            _ = fs::remove_file(&path);
            return Err(PostmortemError::Full {
                path,
                what: "threads recording at once as the recorder was persisted",
                most: MAPPED_RINGS,
            });
        }
        if let Err(made) = MAPPING.set(made) {
            // Another thread persisted meanwhile (its lock on the one file of this pid would
            // have refused this one's `create` first): the file is the winner's, left as it is.
            // alloc: an error
            return Err(PostmortemError::AlreadyPersisted { path: made.path.clone() });
        }
        let Some(mapping) = MAPPING.get() else { return Ok(()) };
        // One no thread holds has no writer and moves here; one a thread holds moves as its
        // thread next writes. No ring stays on the heap.
        for ring in rings.iter() {
            let store = mapping.store(ring.number)?;
            if ring.live.load(Ordering::Acquire) {
                ring.moving.store(std::ptr::from_ref(store).cast_mut(), Ordering::Release);
            } else {
                ring.store().copy_to(store);
                ring.store.store(std::ptr::from_ref(store).cast_mut(), Ordering::Release);
            }
        }
    }
    let Some(mapping) = MAPPING.get() else { return Ok(()) };
    // The kept names given so far, under their lock; those given from here on log themselves
    // as they are given ([`named`]), under the same lock, so each is logged once. Every other
    // name is in the rows of its ring, which the stores carry.
    {
        let kept = lock(&KEPT);
        for (at, name) in kept.as_ref().map_or(&[][..], |it| &it.names[..]).iter().enumerate() {
            mapping.log_name(names::KEPT_SYM | (at as u32 + 1), name);
        }
    }
    {
        let _taken = lock(&TAKEN);
        for (at, site) in SITES.iter().enumerate() {
            if let Some(site) = site.get() {
                mapping.site(
                    at as u16 + 1,
                    site.meta.name(),
                    site.meta.target(),
                    site.meta.level().as_str(),
                );
            }
        }
    }
    Ok(())
}

/// Where the rows of the new ring `number` go, decided under [`RINGS`]'s lock.
pub(super) enum StoreFor {
    /// The recorder is not persisted: a store on the heap.
    Heap,
    /// Its store in the mapping.
    Mapped(&'static Store),
    /// Past [`MAPPED_RINGS`]: no ring, its thread's spans counted as ringless, and the next
    /// flush says so ([`failed`]).
    Full,
}

/// See [`StoreFor`].
pub(super) fn store_for(number: u16) -> StoreFor {
    let Some(mapping) = MAPPING.get() else { return StoreFor::Heap };
    match mapping.store(number) {
        Ok(store) => StoreFor::Mapped(store),
        Err(error) => {
            failing(error);
            StoreFor::Full
        }
    }
}

/// Logs a kept name just given, under [`KEPT`]'s lock.
pub(super) fn named(sym: Sym, name: &str) {
    if let Some(mapping) = MAPPING.get() {
        mapping.log_name(sym, name);
    }
}

/// Writes the record of a site just registered, under [`TAKEN`].
pub(super) fn site_registered(site: u16, meta: &'static super::Meta) {
    if let Some(mapping) = MAPPING.get() {
        mapping.site(site, meta.name(), meta.target(), meta.level().as_str());
    }
}

/// See [`Mapping::mark_flush`]. Nothing until the recorder is persisted.
pub(super) fn mark_flush(seq: u64, held_builds: &[u32], marks: impl FnOnce(usize)) {
    if let Some(mapping) = MAPPING.get() {
        mapping.mark_flush(seq, held_builds, marks);
    }
}

/// See [`Mapping::flush_committed`].
pub(super) fn flush_committed(seq: u64, at_ns: i64) {
    if let Some(mapping) = MAPPING.get() {
        mapping.flush_committed(seq, at_ns);
    }
}

/// This process's pid and its mapping's nonce, once [`persist`] made it: what the history's
/// commits name the flushes of (`manifest::Recorder`).
pub fn recorder() -> Option<(u32, u64)> {
    MAPPING.get().map(|it| (std::process::id(), it.nonce))
}

/// Notes a build running, so a postmortem marks it `crashed` if the process dies first.
pub fn build_started(build: &RunningBuild<'_>) {
    if let Some(mapping) = MAPPING.get()
        && let Err(error) = mapping.build_started(build)
    {
        failing(error);
    }
}

/// Notes a build ended: a postmortem no longer names it.
pub fn build_ended(build: u64) {
    if let Some(mapping) = MAPPING.get() {
        mapping.build_ended(build);
    }
}

/// Deletes this process's mapping, its rows all written: the process goes on writing its pages,
/// which no file names any more. For a history that closes cleanly.
pub fn forget() {
    if let Some(mapping) = MAPPING.get() {
        _ = fs::remove_file(&mapping.path);
    }
}

// --- The reader -------------------------------------------------------------------------

/// The mapping of a process that died, its bytes as the history's core mapped them (the core
/// holds its lock meanwhile: no other reader takes it).
#[derive(Debug)]
pub struct Found {
    path: PathBuf,
    pid: u32,
    mapping: blaze_core::Mapping,
}

/// The mappings in `dir` of processes that died, by pid, read into memory: those whose lock no
/// one holds. A mapping a live process holds is left alone. For tests: the history's core finds
/// them in production.
#[cfg(test)]
fn dead(dir: &Path) -> Result<Vec<Found>, PostmortemError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(io_error(dir, "listing the history's directory for mappings")(error));
        }
    };
    // alloc: once a postmortem, a file a dead process
    let mut named = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_error(dir, "listing the history's directory for mappings"))?;
        let Some(pid) = entry.file_name().to_str().and_then(pid_of) else { continue };
        named.push((pid, entry.path()));
    }
    named.sort_unstable();
    // alloc: once a postmortem
    let mut found = Vec::new();
    for (pid, path) in named {
        let file = match fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            // Read and deleted by another reader since the listing.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error(&path, "opening")(error)),
        };
        match Locked::try_lock(file).map_err(io_error(&path, "locking"))? {
            Ok(file) => {
                let mut file: &File = &file;
                // alloc: a test's copy of a dead process's file
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).map_err(io_error(&path, "reading"))?;
                let mapping = blaze_core::Mapping::copy(&bytes);
                found.push(Found { path, pid, mapping });
            }
            // Its process is alive, or another reader holds it.
            Err(_held) => {}
        }
    }
    Ok(found)
}

/// A build of a dead process that was running as it died.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeadBuild {
    pub build_id: u64,
    pub started_ns: i64,
    pub started_unix_ns: Option<i64>,
    pub commit: Option<String>,
    pub workspace: String,
    pub command: String,
}

/// What a postmortem read back of a dead process. Its names are numbered in this process only
/// while it lives: a writer of tables reads them through [`super::name`] meanwhile. One at a
/// time in a process (each holds the postmortem's lock).
pub struct Recovered {
    pub path: PathBuf,
    pub pid: u32,
    /// Its mapping's nonce ([`H_NONCE`]); 0 for a mapping with no header (its process died
    /// making it), which holds no rows.
    pub nonce: u64,
    /// Its name, as its rows name their process.
    pub process: String,
    /// The hash of the blaze it ran (`builds.blaze`).
    pub blaze: String,
    /// Its rows past the history's last flush, with sites and names of this process.
    pub read: Read,
    pub builds: Vec<DeadBuild>,
    /// The time of its last row or flush, on the schema's clock: a crashed build ran to it.
    pub last_ns: i64,
    /// Rows no owner of their ring named (their thread's claim was not mirrored), not read.
    pub unowned: u64,
    /// Names and sites rows held that the file did not have (a log wrapped past them): read
    /// as none.
    pub names_lost: u64,
    pub sites_lost: u64,
    _session: Session,
}

static RECOVERING: Lock<()> = Lock::unrecorded("profile.postmortem.recovering", ());

/// A postmortem's hold on [`super::RECOVERED`]: the names it gave are let go as it drops.
struct Session(#[allow(dead_code, reason = "held for its drop")] LockGuard<'static, ()>);

impl Session {
    fn begin() -> Session {
        let held = RECOVERING.lock();
        *lock(&super::RECOVERED) = None;
        Session(held)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        *lock(&super::RECOVERED) = None;
    }
}

fn word(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_ne_bytes(word)
}

impl Found {
    /// The mapping of dead process `pid` at `path`, mapped by the history's core under its
    /// lock.
    pub(crate) fn of(path: PathBuf, pid: u32, mapping: blaze_core::Mapping) -> Found {
        Found { path, pid, mapping }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// `len` bytes of the file from `at`; a file that ends before them is torn, naming `what`
    /// was read.
    fn bytes(&self, at: usize, len: usize, what: &'static str) -> Result<&[u8], PostmortemError> {
        at.checked_add(len)
            .and_then(|end| self.mapping.bytes().get(at..end))
            .ok_or_else(|| self.torn(what))
    }

    fn refuse(&self, what: &'static str, found: u64, expected: u64) -> PostmortemError {
        // alloc: an error
        PostmortemError::Refused { path: self.path.clone(), what, found, expected }
    }

    fn torn(&self, what: &'static str) -> PostmortemError {
        // alloc: an error
        PostmortemError::Torn { path: self.path.clone(), what }
    }

    /// The header, checked to be of this layout; `None` for a file whose process died making
    /// it (no magic yet), which holds no rows. A file of another layout is refused, naming it.
    fn header(&self) -> Result<Option<&[u8]>, PostmortemError> {
        let len = self.mapping.bytes().len() as u64;
        if len < PAGE as u64 {
            return Ok(None);
        }
        let header = self.bytes(0, PAGE, "reading the header of")?;
        let h = |at: usize| word(header, at * 8);
        if h(H_MAGIC) == 0 {
            return Ok(None);
        }
        for (what, at, expected) in [
            ("magic", H_MAGIC, MAGIC),
            ("byte order mark", H_ENDIAN, ENDIAN),
            ("layout", H_LAYOUT, LAYOUT),
            ("rows' fingerprint", H_FINGERPRINT, ROWS_FINGERPRINT),
            ("rows a ring", H_CAP, CAP as u64),
            ("words a row", H_WORDS, WORDS as u64),
            ("bytes a store", H_STORE_BYTES, STORE_BYTES as u64),
        ] {
            if h(at) != expected {
                return Err(self.refuse(what, h(at), expected));
            }
        }
        let needed = RINGS_AT as u64 + h(H_RINGS) * STORE_BYTES as u64;
        if len < needed {
            return Err(self.refuse("length", len, needed));
        }
        Ok(Some(header))
    }

    /// The mapping's nonce, which the manifest names it by (`manifest::Recorder`); `None` for
    /// a file with no header, which holds no rows. A file of another layout is refused.
    pub fn nonce(&self) -> Result<Option<u64>, PostmortemError> {
        Ok(self.header()?.map(|header| word(header, H_NONCE * 8)))
    }

    /// Reads back the dead process's rows past its last committed flush, its running builds,
    /// and what names them. `committed` is the last flush of the process the manifest says
    /// committed (`manifest::Recorder::flushed`), `None` when it names none of this mapping:
    /// the rows are read past the marks of that flush when the mapping marked it pending,
    /// past the marks of the last flush the mapping saw committed otherwise, and from the
    /// first row a ring holds when it saw none. Before those marks, the DEBUG rows of the
    /// builds that flush held are read too: the history had not written them. A file of
    /// another layout is refused, naming it.
    pub fn read(&self, committed: Option<u64>) -> Result<Recovered, PostmortemError> {
        let session = Session::begin();
        let mut recovered = Recovered {
            // alloc: a path, once a dead process
            path: self.path.clone(),
            pid: self.pid,
            nonce: 0,
            process: String::new(),
            blaze: String::new(),
            read: Read::default(),
            builds: Vec::new(),
            last_ns: 0,
            unowned: 0,
            names_lost: 0,
            sites_lost: 0,
            _session: session,
        };
        // A process that died making its file wrote no magic: nothing of it to read.
        let Some(header) = self.header()? else { return Ok(recovered) };
        let h = |at: usize| word(header, at * 8);
        recovered.nonce = h(H_NONCE);
        let rings = h(H_RINGS);
        // The slot whose marks it reads from: the pending flush's once the manifest says it
        // committed, else the last the mapping saw committed, else none (every row).
        let pending = marked(h(H_PENDING));
        let slot = match pending {
            Some((seq, slot)) if committed.is_some_and(|it| it >= seq) => Some(slot),
            _ => marked(h(H_COMMITTED)).map(|it| it.1),
        };
        if slot.is_some_and(|it| it >= MARK_SLOTS) {
            return Err(self.torn("the slot of the history's flush marks"));
        }
        // alloc: once a postmortem, at most HOLD_BUILDS
        let mut held_builds: Vec<u32> = Vec::new();
        if let Some(slot) = slot {
            let base = H_HELD + slot * HELD_WORDS;
            let count = usize::try_from(h(base)).unwrap_or(usize::MAX);
            if count > HOLD_BUILDS {
                return Err(self.torn("the builds a flush held DEBUG rows of"));
            }
            held_builds.extend((0..count).map(|at| h(base + 1 + at) as u32));
        }
        let text = |at: usize, len_at: usize, max: usize| -> Result<String, PostmortemError> {
            let len = usize::try_from(h(len_at))
                .ok()
                .filter(|it| *it <= max)
                .ok_or_else(|| self.torn("the header's text"))?;
            let bytes = &header[at * 8..at * 8 + len];
            Ok(String::from_utf8_lossy(bytes).into_owned())
        };
        recovered.blaze = text(H_BLAZE, H_BLAZE_LEN, BLAZE_MAX)?;
        recovered.process = text(H_PROCESS, H_PROCESS_LEN, PROCESS_MAX)?;
        let clock_at = (h(H_CLOCK_AT) & 1) as usize;
        let line = [
            h(H_CLOCK + clock_at * 3),
            h(H_CLOCK + clock_at * 3 + 1),
            h(H_CLOCK + clock_at * 3 + 2),
        ];
        if rings != 0 && line[2] == 0 {
            return Err(self.torn("the clock's base"));
        }
        let scale = clock::Scale::of_line(line);
        let counting_copies = h(H_COPIES) != 0;
        recovered.last_ns = h(H_FLUSHED_NS) as i64;

        let kept = self.names(&header)?;
        let sites = self.sites()?;
        recovered.builds = self.builds()?;
        // Pass one: every name each ring's rows define, oldest to newest, so a link from one
        // ring to another resolves whichever ring is decoded first.
        let mut streams = names::Streams::default();
        for ring in 0..rings as usize {
            let (number, head, _, store) = self.dead_ring(ring)?;
            let oldest = (head + 1).saturating_sub(CAP as u64);
            // alloc: a ring's rows, once a ring of a dead process
            let rows: Vec<(u64, [u64; WORDS])> =
                (oldest..head).map(|i| (i, row_at(&store, i))).collect();
            streams.feed(number, oldest, head, rows.iter().map(|(i, row)| (*i, row)));
        }
        let mut namer = DeadNamer {
            streams,
            kept: &kept,
            // alloc: once a postmortem, a kept name its rows hold
            kept_syms: HashMap::new_in(Global),
            lost: 0,
            failed: false,
        };
        let mut read = Read::default();
        // The rows before the marks, of which only the held DEBUG rows are kept.
        let mut held = Read::default();
        for ring in 0..rings as usize {
            let (number, head, owners, store) = self.dead_ring(ring)?;
            let (mark, held_mark) = match slot {
                Some(slot) => {
                    let at = std::mem::offset_of!(Store, marks) + slot * 16;
                    (word(&store, at), word(&store, at + 8))
                }
                None => (0, 0),
            };
            // The row the head would be next is half written, perhaps over the oldest: both
            // left out, as a live read leaves them.
            let oldest = (head + 1).saturating_sub(CAP as u64);
            let from = mark.min(head).max(oldest);
            let held_from = held_mark.min(from).max(oldest);
            for (into, range) in [(&mut held, held_from..from), (&mut read, from..head)] {
                into.ring_begins();
                for i in range {
                    let row = row_at(&store, i);
                    let Some(by) = super::Ring::owner(&owners, i) else {
                        recovered.unowned += 1;
                        continue;
                    };
                    decode(
                        into,
                        (by.tid, by.name),
                        super::event_id(number, i),
                        &row,
                        &scale,
                        counting_copies,
                        &mut namer,
                    );
                }
                // A span whose CPU waited on a boundary row the process died before writing
                // reads as not counted, as a live read's at the end of a ring.
                into.ring_ends();
            }
        }
        if namer.failed {
            // alloc: an error
            return Err(PostmortemError::Full {
                path: self.path.clone(),
                what: "names of a dead process's rows",
                most: super::RECOVERED_MAX,
            });
        }
        recovered.names_lost = namer.lost + namer.streams.forgotten;
        // Of the rows before the marks, the DEBUG events and their fields of the builds the
        // flush held them for: in no file. Those of other builds were written or dropped.
        let debug_held = |site: u16, build: u32| {
            held_builds.contains(&build)
                && usize::from(site)
                    .checked_sub(1)
                    .and_then(|at| sites.get(at))
                    .and_then(Option::as_ref)
                    .is_some_and(|(_, _, level)| *level == "DEBUG")
        };
        read.events.extend(held.events.iter().filter(|it| debug_held(it.site, it.build)));
        read.fields.extend(held.fields.iter().filter(|it| debug_held(it.site, it.build)));
        self.renumber(&mut read, &sites, &mut recovered)?;
        let last = [
            read.spans.iter().map(|it| it.start_ns + it.dur_ns).max(),
            read.counters.iter().map(|it| it.t_ns).max(),
            read.switches.iter().map(|it| it.t_ns).max(),
            read.events.iter().map(|it| it.t_ns).max(),
            read.folds.iter().map(|it| it.to_ns).max(),
            read.ends.iter().map(|it| it.t_ns).max(),
        ];
        recovered.last_ns = last.into_iter().flatten().fold(recovered.last_ns, i64::max);
        read.at_ns = recovered.last_ns;
        recovered.read = read;
        Ok(recovered)
    }

    /// The names the logs hold, newest first winning: a [`Sym`] given again after its
    /// generation came round names what it named last.
    fn names(&self, header: &[u8]) -> Result<HashMap<Sym, String, Global>, PostmortemError> {
        // alloc: once a postmortem, a name a dead process gave
        let mut names = HashMap::new_in(Global);
        for log in 0..LOGS {
            let head = word(header, (H_LOG_HEADS + log) * 8);
            if head == 0 {
                continue;
            }
            let bytes =
                self.bytes(LOGS_AT + log * LOG_BYTES, LOG_BYTES, "reading a name log of")?;
            let at = |offset: u64| (offset % LOG_BYTES as u64) as usize;
            let read = |from: u64, len: usize| -> Vec<u8> {
                // alloc: a name, once a postmortem
                (0..len).map(|it| bytes[at(from + it as u64)]).collect()
            };
            let held = head.min(LOG_BYTES as u64);
            let (mut end, mut taken) = (head, 0u64);
            while taken + 8 <= held {
                let mut trailer = [0; 8];
                trailer.copy_from_slice(&read(end - 8, 8));
                let trailer = u64::from_ne_bytes(trailer);
                let len = (trailer & 0x7fff_ffff) as usize;
                let cut_short = trailer >> 31 & 1 == 1;
                let sym = (trailer >> 32) as Sym;
                if len > NAME_MAX {
                    return Err(self.torn("a name log's entry"));
                }
                let entry = (len.div_ceil(8) * 8 + 8) as u64;
                if taken + entry > held {
                    break;
                }
                let mut text = String::from_utf8_lossy(&read(end - entry, len)).into_owned();
                if cut_short {
                    text.push_str(" [cut]");
                }
                names.entry(sym).or_insert(text);
                end -= entry;
                taken += entry;
            }
        }
        Ok(names)
    }

    /// The site records: each site's name, target and level, by its site less one.
    fn sites(&self) -> Result<Vec<Option<(String, String, &'static str)>>, PostmortemError> {
        let bytes = self.bytes(SITES_AT, SLOTS * SITE_RECORD, "reading the callsites of")?;
        // alloc: once a postmortem, a slot a site
        let mut sites = Vec::with_capacity(SLOTS);
        for at in 0..SLOTS {
            let base = at * SITE_RECORD;
            let record = word(&bytes, base);
            if record >> 63 == 0 {
                sites.push(None);
                continue;
            }
            let (name_len, target_len) =
                ((record & 0xffff) as usize, (record >> 16 & 0xffff) as usize);
            let level = LEVELS
                .get((record >> 48 & 0x7) as usize)
                .copied()
                .ok_or_else(|| self.torn("a callsite's level"))?;
            if name_len + target_len > SITE_TEXT {
                return Err(self.torn("a callsite's record"));
            }
            let name = String::from_utf8_lossy(&bytes[base + 8..base + 8 + name_len]).into_owned();
            let target = String::from_utf8_lossy(
                &bytes[base + 8 + name_len..base + 8 + name_len + target_len],
            )
            .into_owned();
            sites.push(Some((name, target, level)));
        }
        Ok(sites)
    }

    /// The builds running as the process died, by build.
    fn builds(&self) -> Result<Vec<DeadBuild>, PostmortemError> {
        let bytes = self.bytes(BUILDS_AT, BUILDS_BYTES, "reading the builds running of")?;
        // alloc: once a postmortem
        let mut builds = Vec::new();
        for at in 0..BUILD_SLOTS {
            let base = at * BUILD_RECORD;
            let build_id = word(&bytes, base);
            if build_id == 0 {
                continue;
            }
            let lens = word(&bytes, base + 24);
            let (commit_len, workspace_len, command_len) = (
                (lens & 0xff) as usize,
                (lens >> 16 & 0xffff) as usize,
                (lens >> 32 & 0xffff) as usize,
            );
            if commit_len > BUILD_COMMIT_MAX
                || workspace_len > BUILD_WORKSPACE_MAX
                || command_len > BUILD_COMMAND_MAX
            {
                return Err(self.torn("a running build's record"));
            }
            let text = |from: usize, len: usize| {
                String::from_utf8_lossy(&bytes[base + 32 + from..base + 32 + from + len])
                    .into_owned()
            };
            let unix = word(&bytes, base + 16) as i64;
            builds.push(DeadBuild {
                build_id,
                started_ns: word(&bytes, base + 8) as i64,
                started_unix_ns: (unix != i64::MIN).then_some(unix),
                commit: (lens >> 8 & 1 == 1).then(|| text(0, commit_len)),
                workspace: text(commit_len, workspace_len),
                command: text(commit_len + workspace_len, command_len),
            });
        }
        builds.sort_unstable_by_key(|it| it.build_id);
        Ok(builds)
    }

    /// One ring's store of the dead process, by its place in the mapping: its number, its head,
    /// its owners and its bytes.
    fn dead_ring(&self, ring: usize) -> Result<(u16, u64, Vec<Owner>, &[u8]), PostmortemError> {
        let store = self.bytes(
            RINGS_AT + ring * STORE_BYTES,
            std::mem::size_of::<Store>(),
            "reading a ring's store of",
        )?;
        let field = |offset: usize| word(&store, offset);
        let head = field(std::mem::offset_of!(Store, head));
        let number = u16::try_from(field(std::mem::offset_of!(Store, number)))
            .map_err(|_| self.torn("a ring's number"))?;
        let owners_at = std::mem::offset_of!(Store, owners);
        let owners_len =
            usize::try_from(field(std::mem::offset_of!(Store, owners_len))).unwrap_or(usize::MAX);
        if owners_len > super::OWNERS_KEPT {
            return Err(self.torn("a ring's owners"));
        }
        // alloc: a ring's owners, once a ring of a dead process
        let owners: Vec<Owner> = (0..owners_len)
            .map(|at| {
                let (from, who) = (field(owners_at + at * 16), field(owners_at + at * 16 + 8));
                Owner { from, tid: (who >> 32) as u32, name: who as Sym }
            })
            .collect();
        Ok((number, head, owners, store))
    }

    /// Gives every site `read` holds a number of this process, for the process's life. Names
    /// were given theirs as the rows were decoded ([`DeadNamer`]).
    fn renumber(
        &self,
        read: &mut Read,
        sites: &[Option<(String, String, &'static str)>],
        recovered: &mut Recovered,
    ) -> Result<(), PostmortemError> {
        // alloc: once a postmortem, a site its rows hold
        let mut site_of: HashMap<u16, u16, Global> = HashMap::new_in(Global);
        let mut sites_lost = 0;
        let mut sites_failed = false;
        let mut site = |old: u16| -> u16 {
            if old == 0 {
                return 0;
            }
            if let Some(&new) = site_of.get(&old) {
                return new;
            }
            let new = match sites.get(usize::from(old) - 1).and_then(Option::as_ref) {
                Some((name, target, level)) => super::foreign_site(name, target, level)
                    .unwrap_or_else(|| {
                        sites_failed = true;
                        0
                    }),
                None => {
                    sites_lost += 1;
                    0
                }
            };
            site_of.insert(old, new);
            new
        };
        for row in &mut read.spans {
            row.site = site(row.site);
        }
        for row in &mut read.events {
            row.site = site(row.site);
        }
        for row in &mut read.fields {
            row.site = site(row.site);
        }
        for row in &mut read.folds {
            row.site = site(row.site);
            row.parent_site = site(row.parent_site);
        }
        recovered.sites_lost = sites_lost;
        if sites_failed {
            return Err(PostmortemError::Full {
                // alloc: an error
                path: self.path.clone(),
                what: "callsites of dead processes in one process",
                most: super::FOREIGN_MAX,
            });
        }
        Ok(())
    }

    /// Deletes the file, its rows written. For tests: the history's core deletes it in
    /// production.
    #[cfg(test)]
    fn remove(self) -> Result<(), PostmortemError> {
        fs::remove_file(&self.path).map_err(io_error(&self.path, "deleting, its rows written,"))
    }
}

/// Row `i` of a dead ring's store, as its bytes hold it.
fn row_at(store: &[u8], i: u64) -> [u64; WORDS] {
    let at = super::STORE_HEAD + (i as usize & (CAP - 1)) * WORDS * 8;
    std::array::from_fn(|it| word(store, at + it * 8))
}

/// How a postmortem gives the names of a dead process's rows numbers of this one: a ring's own
/// ids through the definitions its rings hold ([`names::Streams`]), kept names through its
/// kept log, each as [`super::recovered`] numbers it for the postmortem's session.
struct DeadNamer<'a> {
    streams: names::Streams,
    kept: &'a HashMap<Sym, String, Global>,
    kept_syms: HashMap<Sym, Sym, Global>,
    /// Kept names the log no longer held.
    lost: u64,
    /// Past [`super::RECOVERED_MAX`] names.
    failed: bool,
}

impl DeadNamer<'_> {
    fn recovered(&mut self, text: &str) -> Sym {
        super::recovered(text).unwrap_or_else(|| {
            self.failed = true;
            0
        })
    }
}

impl super::Namer for DeadNamer<'_> {
    fn sym(&mut self, ring: u16, at: u64, word: u64) -> Sym {
        let word = word as u32;
        if word == 0 {
            return 0;
        }
        if word & names::CLASS == names::KEPT_SYM {
            if let Some(&sym) = self.kept_syms.get(&word) {
                return sym;
            }
            let sym = match self.kept.get(&word) {
                Some(text) => super::recovered(text).unwrap_or_else(|| {
                    self.failed = true;
                    0
                }),
                None => {
                    self.lost += 1;
                    0
                }
            };
            self.kept_syms.insert(word, sym);
            return sym;
        }
        match self.streams.resolve(ring, at, word, &mut names::Fed) {
            Some(text) => self.recovered(&text),
            None => 0,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::flight::{
        DEPTH_BITS, KIND_PLOT, KIND_SPAN, Namer, Plot, Ring, SITE_BITS, SpanRow, Writing, name,
        site_text,
    };

    /// A namer that leaves every id as the row holds it: what the expected rows are decoded
    /// with, their names compared as text.
    struct AsWritten;

    impl Namer for AsWritten {
        fn sym(&mut self, _: u16, _: u64, word: u64) -> Sym {
            word as Sym
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("blaze-postmortem-{name}-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A pid no live process has: only the file's name, and a lock no one holds once the
    /// mapping drops, say it.
    const DEAD: u32 = 4_000_000_001;

    /// A span of build 7 at site `site`, ticks from `t0`.
    fn span_row(t0: u64, i: u64, site: u16, krate: Sym) -> [u64; WORDS] {
        let what = KIND_SPAN << 62
            | u64::from(site) << (62 - SITE_BITS)
            | 2 << (62 - SITE_BITS - DEPTH_BITS);
        [t0 + i * 10 + 5, t0 + i * 10, 100 + i, 0, what, u64::from(krate) << 32, 7 << 32, 0]
    }

    fn plot_row(t0: u64, i: u64) -> [u64; WORDS] {
        [t0 + i * 10, i, 0, 0, KIND_PLOT << 62 | Plot::Queued as u64, 0, 7, 0]
    }

    /// A process writes rows into its mapping and dies (its lock let go): a postmortem reads
    /// back exactly the rows past the history's last flush, as the process would have read
    /// them, with their names (a kept one from the log, a ring's from its definition, written
    /// before the flush's marks) and sites, and the build that was running.
    #[test]
    fn a_dead_process_s_rows_past_its_last_flush_read_back_whole() {
        let dir = scratch("rows");
        let mapping = Mapping::create(&dir, DEAD, "c0ffee", "blaze serve", false).unwrap();
        let scale = clock::Scale::now();
        mapping.set_clock(&scale);
        let worker = names::KEPT_SYM | 1;
        mapping.log_name(worker, "worker 1");
        mapping.site(9, "compile", "blaze::unit", "INFO");
        mapping
            .build_started(&RunningBuild {
                build_id: 7,
                started_ns: 500,
                started_unix_ns: Some(1_700_000_000_000_000_000),
                commit: Some("abc123"),
                workspace: "/w",
                command: "build --release",
            })
            .unwrap();
        mapping
            .build_started(&RunningBuild {
                build_id: 6,
                started_ns: 400,
                started_unix_ns: None,
                commit: None,
                workspace: "/w",
                command: "check",
            })
            .unwrap();
        mapping.build_ended(6);
        let store = mapping.store(1).unwrap();
        let ring = Ring::with_store(1, store);
        store.mirror(&[Owner { from: 0, tid: 42, name: worker }]);
        // `serde` is defined in the ring's own rows: a generation's start and its definition.
        let mut local = names::Local::new(1, 0);
        let mut words = [local.intern("serde", &mut Writing(&ring))];
        local.row_syms(&mut words, &mut Writing(&ring));
        let serde = words[0] as Sym;
        let base = store.head.load(Ordering::Relaxed);
        assert_eq!(base, 2, "a generation's row and one definition");
        let t0 = clock::ticks();
        let rows: Vec<[u64; WORDS]> = (0..40)
            .map(|i| if i % 2 == 0 { span_row(t0, i, 9, serde) } else { plot_row(t0, i) })
            .collect();
        for row in &rows {
            ring.put(*row);
        }
        // The history wrote the first 25 rows to its files, in a flush that committed; its last
        // flush read the rings a millisecond before the first row.
        mapping.mark_flush(1, &[], |slot| store.mark(slot, 25, 25));
        mapping.flush_committed(1, scale.mono(t0) - 1_000_000);
        drop(mapping);

        // What the dead process would have decoded of the rows past its flush.
        let mut expected = Read::default();
        let line = clock::Scale::of_line(
            // The clock as the mapping keeps it: the last `set_clock`, by `flushed_at`.
            {
                let found = dead(&dir).unwrap();
                let header = found[0].bytes(0, PAGE, "test").unwrap();
                let at = (word(&header, H_CLOCK_AT * 8) & 1) as usize;
                std::array::from_fn(|it| word(&header, (H_CLOCK + at * 3 + it) * 8))
            },
        );
        expected.ring_begins();
        for (i, row) in rows.iter().enumerate().skip(25 - base as usize) {
            decode(
                &mut expected,
                (42, worker),
                super::super::event_id(1, base + i as u64),
                row,
                &line,
                false,
                &mut AsWritten,
            );
        }
        expected.ring_ends();

        let found = dead(&dir).unwrap();
        assert_eq!(found.len(), 1, "the one file of a dead process");
        assert_eq!(found[0].pid(), DEAD);
        let recovered = found[0].read(None).unwrap();
        assert_eq!(
            (recovered.process.as_str(), recovered.blaze.as_str()),
            ("blaze serve", "c0ffee")
        );
        assert_eq!((recovered.unowned, recovered.names_lost, recovered.sites_lost), (0, 0, 0));
        assert_eq!(recovered.read.spans.len(), expected.spans.len());
        assert_eq!(
            recovered.read.counters, expected.counters,
            "counters hold no name: equal as written"
        );
        for (got, want) in recovered.read.spans.iter().zip(&expected.spans) {
            assert_eq!(
                site_text(got.site).map(|it| (it.name, it.target, it.level)),
                Some(("compile", "blaze::unit", "INFO"))
            );
            assert_eq!(name(got.thread_name).as_deref(), Some("worker 1"));
            assert_eq!(name(got.krate).as_deref(), Some("serde"));
            let renamed =
                SpanRow { site: got.site, thread_name: got.thread_name, krate: got.krate, ..*want };
            assert_eq!(*got, renamed, "every other column as the process wrote it");
        }
        assert_eq!(
            recovered.builds,
            vec![DeadBuild {
                build_id: 7,
                started_ns: 500,
                started_unix_ns: Some(1_700_000_000_000_000_000),
                commit: Some("abc123".to_owned()),
                workspace: "/w".to_owned(),
                command: "build --release".to_owned(),
            }],
            "the build still running, not the one that ended"
        );
        let last = expected
            .spans
            .iter()
            .map(|it| it.start_ns + it.dur_ns)
            .chain(expected.counters.iter().map(|it| it.t_ns))
            .max();
        assert_eq!(Some(recovered.last_ns), last);
        let thread_name = recovered.read.spans[0].thread_name;
        drop(recovered);
        assert_eq!(name(thread_name), None, "a postmortem's names go with it");
        found.into_iter().next().unwrap().remove().unwrap();
        assert!(dead(&dir).unwrap().is_empty(), "the file is gone once read");
        _ = fs::remove_dir_all(&dir);
    }

    /// A file of another layout is refused with an error naming it, and left in place.
    #[test]
    fn a_mapping_of_another_layout_is_refused_naming_its_file() {
        let dir = scratch("layout");
        let mapping = Mapping::create(&dir, DEAD, "c0ffee", "blaze serve", false).unwrap();
        mapping.set_clock(&clock::Scale::now());
        drop(mapping);
        let path = dir.join(file_name(DEAD));
        {
            use std::{io::Write as _, os::unix::fs::FileExt as _};
            let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.write_all_at(&(LAYOUT + 1).to_ne_bytes(), (H_LAYOUT * 8) as u64).unwrap();
            (&file).flush().unwrap();
        }
        let found = dead(&dir).unwrap();
        let refused = found[0].read(None).err().expect("refused");
        match &refused {
            PostmortemError::Refused { path: named, what, found, expected } => {
                assert_eq!(
                    (named, *what, *found, *expected),
                    (&path, "layout", LAYOUT + 1, LAYOUT)
                );
            }
            other => panic!("refused as another layout, not {other:?}"),
        }
        assert!(
            refused.to_string().contains(&path.display().to_string()),
            "the error names the file: {refused}"
        );
        drop(found);
        assert!(path.exists(), "a refused file stays");
        _ = fs::remove_dir_all(&dir);
    }

    /// A flush marked pending is read past only once the manifest says it committed: a crash
    /// between its commit and its mark never writes its rows twice, and one before its commit
    /// never loses them.
    #[test]
    fn a_pending_flush_is_read_past_only_once_the_manifest_says_it_committed() {
        let dir = scratch("pending");
        let mapping = Mapping::create(&dir, DEAD, "c0ffee", "blaze serve", false).unwrap();
        mapping.set_clock(&clock::Scale::now());
        mapping.site(9, "compile", "blaze::unit", "INFO");
        let store = mapping.store(1).unwrap();
        let ring = Ring::with_store(1, store);
        store.mirror(&[Owner { from: 0, tid: 42, name: 0 }]);
        let t0 = clock::ticks();
        for i in 0..30 {
            ring.put(plot_row(t0, i));
        }
        // Flush 4 took 10 rows and committed; flush 5 took 20, and the process died before
        // its commit was marked (or before it committed).
        mapping.mark_flush(4, &[], |slot| store.mark(slot, 10, 10));
        mapping.flush_committed(4, 0);
        mapping.mark_flush(5, &[], |slot| store.mark(slot, 20, 20));
        drop(mapping);
        let found = dead(&dir).unwrap();
        assert!(found[0].nonce().unwrap().is_some_and(|it| it != 0), "a mapping has a nonce");
        let rows = |committed| found[0].read(committed).unwrap().read.counters.len();
        assert_eq!(rows(None), 20, "no manifest names it: past the last commit the mapping saw");
        assert_eq!(rows(Some(4)), 20, "the manifest has flush 4: flush 5 did not commit");
        assert_eq!(rows(Some(5)), 10, "the manifest has flush 5: its rows are in files");
        _ = fs::remove_dir_all(&dir);
    }

    /// A mapping its process holds is not a dead one.
    #[test]
    fn a_live_mapping_is_left_alone() {
        let dir = scratch("live");
        let mapping = Mapping::create(&dir, DEAD, "c0ffee", "blaze serve", false).unwrap();
        assert!(dead(&dir).unwrap().is_empty());
        drop(mapping);
        assert_eq!(dead(&dir).unwrap().len(), 1);
        _ = fs::remove_dir_all(&dir);
    }
}
