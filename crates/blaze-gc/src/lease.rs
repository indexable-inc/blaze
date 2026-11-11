//! A directory's lease: `.blaze-lease` in it, a file that holds a nonce written once when it is
//! made. Every process that uses the directory (a daemon's job building into a build dir, a
//! daemon using a standard library's store) holds a shared `flock` on it for as long as it uses
//! it ([`Lease::hold`]). The collector renames the directory away only while it holds an
//! exclusive lock on it, taken without waiting ([`take_exclusive`]), so never while anyone uses
//! it. The lock is the kernel's: it ends with the process that held it, so a crash leaves no
//! lease behind. Network file systems, whose emulated locks do not hold, are refused by the
//! driver ([`crate::fs::check_local`]).
//!
//! The invariant (docs/gc.md, "Leases"): a directory is renamed into trash only by a holder of
//! the exclusive lock on its lease and on every lease inside it, and those locks are held only
//! for the scan's probe (lock, then let go), for one rename of the tree (the collector's), or for
//! one rename and the unlink of what was renamed (`blaze clean`). An unlink of trash (the
//! collector's or `blaze clean`'s) holds the exclusive lock on every lease inside the entry
//! through the removal. A tree's leases are found by a walk that takes no lock, so no lock is
//! held across a walk of it but the one after the rename (`crate::fs::take_nested_leases`).
//! Nothing orders a nested build's shared lock after the outer exclusive one: a build in
//! another process may take a lease inside between that walk and the lock. The walk after the
//! rename, with the locks held, finds it held, and the tree is renamed back
//! (`crate::fs::recheck_renamed`). That build sees its directory gone from the rename to the
//! rename back, which spans the walk of the renamed tree: as long as that walk takes on a big
//! tree, and for good when the rename back fails (its path made again meanwhile; the tree then
//! stays in trash, `Swept::LeftInTrash`). Its build may fail then; nothing it holds is ever
//! unlinked, since every unlink first takes every lease inside exclusively (review rounds 7 and
//! 8, C1). So a
//! user of one of those directories waits on the collector for at most a rename, or a clean's
//! removal, and [`Lease::hold`] bounds the wait by [`SHARED_WAIT`], failing with an error naming
//! the directory past it; a lowering gives way and tries again for [`GIVE_WAY`].
//!
//! Either side may lock a lease file that is no longer its directory's: a user that opened it
//! just before the collector renamed the directory, or a collector that opened it before another
//! collector renamed the directory and someone made it again. Each finds out by comparing the
//! file it holds with the file at the path ([`verified`]): the same device and inode on unix
//! (a copy, `cp -a` or a CI cache restore, carries the nonce but not the inode), and the same
//! nonce, read whole. When they differ, or the path is gone, the lock is not the directory's. A
//! user then makes the directory afresh and takes the lease again, at most [`ATTEMPTS`] times;
//! a collector does not rename.
//!
//! The lease file is made with an exclusive create and its nonce written as one line ending in a
//! newline, so file systems without hard links (FAT, exFAT, some FUSE) hold leases too. A reader
//! that finds the line not yet whole reads it once, never spins: a user gives way and tries
//! again through its scheduler ([`Lease::try_hold`] returns [`Wait::Short`]), for at most
//! [`GIVE_WAY`], the collector leaves the directory. A lease file still without a whole nonce [`ABANDONED`] after it was last written
//! is one whose maker died between the create and the write: whoever holds it exclusively writes
//! a fresh nonce into it ([`rewrite_abandoned`]), so a crash never leaves a directory that can't
//! be leased (review C8).

use std::{
    fs::{File, OpenOptions},
    io::{Read as _, Seek as _, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use crossbeam_queue::SegQueue;
use stdx::file_lock::Locked;

/// The lease's file in its directory.
pub const LEASE: &str = ".blaze-lease";

/// How many times a lease is taken again after its directory was collected under it, before
/// [`Lease::hold`] gives up naming the directory: each takes a collection that ran between
/// opening the file and locking it.
pub const ATTEMPTS: u32 = 4;

/// The longest [`Lease::hold`] waits for a collector's exclusive lock to end: far past one
/// rename, short of anyone waiting on the build.
pub const SHARED_WAIT: Duration = Duration::from_secs(1);

/// A lease file without a whole nonce this long after it was last written is abandoned: its
/// maker writes the nonce right after the create, in microseconds.
pub const ABANDONED: Duration = Duration::from_secs(10);

/// How long a user gives way to a lease it cannot take now ([`Wait`]) before it fails naming
/// the directory and the state (review N8): an abandoned lease file is rewritten once
/// [`ABANDONED`] passed, and a collector's rename holds the lock far less than [`SHARED_WAIT`].
pub const GIVE_WAY: Duration = Duration::from_secs(ABANDONED.as_secs() + SHARED_WAIT.as_secs());

/// Why a lease could not be taken now, never waiting ([`Lease::try_hold`]): the caller gives way
/// and tries again, for at most [`GIVE_WAY`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wait {
    /// Held exclusively: a collector's one rename, or `blaze clean`'s.
    Busy,
    /// The lease file holds no whole nonce yet: its maker is writing it, or died before it did
    /// and it is rewritten once [`ABANDONED`].
    Short,
}

impl std::fmt::Display for Wait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Wait::Busy => write!(
                f,
                "its lease is held exclusively (blaze's collector or `blaze clean` renaming it)"
            ),
            Wait::Short => write!(
                f,
                "its lease file holds no whole nonce (its maker is writing it, or died before it did, and it is rewritten only {ABANDONED:?} after it was last written)"
            ),
        }
    }
}

/// The file in an area that one collector at a time holds exclusively: two daemons (or a
/// daemon and `blaze cache gc`) on one root never collect the same area at once.
pub const COLLECTOR: &str = ".blaze-collector";

/// Leases taken in this process, counted: a collector's rename asked for at count `n` is not
/// done once the count moved ([`crate::fs::Gate`]).
static USES: AtomicU64 = AtomicU64::new(0);
/// Whether a collector in this process takes [`take_used`]; until then nothing is queued.
static COLLECTING: AtomicBool = AtomicBool::new(false);
/// The directories leased in this process since the collector last took them.
static USED: SegQueue<PathBuf> = SegQueue::new();
/// Leases let go in this process so far: what one held may be collected now.
static RELEASED: AtomicU64 = AtomicU64::new(0);

/// Leases let go in this process so far ([`crate::Collector::lease_released`]).
pub fn releases() -> u64 {
    RELEASED.load(Ordering::SeqCst)
}

/// Holds `area`'s collector lock (`<area>/.blaze-collector`, exclusive, never waiting) for as
/// long as the file is kept, its holder `holder` (`blaze serve pid 123`) written into it:
/// `Err(holder)` while another collector holds it, whose area this one then leaves alone,
/// naming who holds it ([`collector_holder`], review N15).
pub fn collector_lock(area: &Path, holder: &str) -> Result<Result<Locked, String>, String> {
    std::fs::create_dir_all(area).map_err(|error| format!("making {}: {error}", area.display()))?;
    let path = area.join(COLLECTOR);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| format!("opening {}: {error}", path.display()))?;
    let mut file = match Locked::try_lock(file) {
        Ok(Ok(file)) => file,
        Ok(Err(_held)) => return Ok(Err(collector_holder(area))),
        Err(error) => return Err(format!("locking {}: {error}", path.display())),
    };
    let failed = |error: std::io::Error| format!("writing {}'s holder: {error}", path.display());
    file.set_len(0).map_err(failed)?;
    file.rewind().map_err(failed)?;
    file.write_all(format!("{holder}\n").as_bytes()).map_err(failed)?;
    Ok(Ok(file))
}

/// Who holds `area`'s collector lock, as its holder wrote it ([`collector_lock`]); "another
/// collector" when the file names none (a blaze before holders were written).
pub fn collector_holder(area: &Path) -> String {
    let named = std::fs::read_to_string(area.join(COLLECTOR)).ok();
    match named.as_deref().map(str::trim).filter(|it| !it.is_empty()) {
        Some(holder) => holder.to_owned(),
        None => "another collector".to_owned(),
    }
}

/// A collector in this process takes what is leased ([`take_used`]) from now on.
pub fn collecting() {
    COLLECTING.store(true, Ordering::SeqCst);
}

/// Leases taken in this process so far, and roots changed ([`note_use`]).
pub fn uses() -> u64 {
    USES.load(Ordering::SeqCst)
}

/// Counts a use that is no lease (a root changed): renames asked for before it are not done.
pub fn note_use() {
    USES.fetch_add(1, Ordering::SeqCst);
}

/// One directory leased since the last take, for the collector to mark used.
pub fn take_used() -> Option<PathBuf> {
    USED.pop()
}

/// A shared lock on a directory's lease, held until dropped: let go of then even while a child
/// another thread spawns still has the file open ([`Locked`]).
#[derive(Debug)]
pub struct Lease {
    _file: Locked,
    dir: PathBuf,
}

impl Lease {
    /// Holds `dir`'s lease shared, making `dir` and its lease when they are not there. Waits at
    /// most [`SHARED_WAIT`] for the collector's one rename, then fails naming `dir`.
    pub fn hold(dir: &Path) -> Result<Lease, String> {
        // Counted and queued before the lock: a collector that checks the count once it holds
        // the lock exclusively sees this use, or locked before it and is waited for.
        note_use();
        if COLLECTING.load(Ordering::SeqCst) {
            // alloc: one entry a lease, taken by the collector's next turn
            USED.push(dir.to_path_buf());
        }
        let mut attempts = 0;
        // A lease file without a whole nonce is waited on, bounded by [`GIVE_WAY`]: rewritten
        // once abandoned (review N8), never counted against the attempts.
        let mut short = None;
        while attempts < ATTEMPTS {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("making {}: {error}", dir.display()))?;
            let Some(file) = open_or_make(dir)? else {
                attempts += 1;
                continue;
            };
            let Some(mut file) = lock_shared(file, dir)? else {
                // Renamed away while it waited: made afresh.
                attempts += 1;
                continue;
            };
            match verified(&mut file, dir)? {
                Check::Own => return Ok(Lease::new(file, dir)),
                // Collected, and maybe made again, between the open and the lock.
                Check::NotOwn => attempts += 1,
                Check::Short => {
                    drop(file);
                    if !repair(dir)? {
                        wait_whole(dir, &mut short)?;
                    }
                }
            }
        }
        Err(format!(
            "{}'s lease was not taken in {ATTEMPTS} attempts: it was collected under its lease each time",
            dir.display()
        ))
    }

    /// Holds `dir`'s lease shared as [`Lease::hold`] does, but never waits: a [`Wait`] when it
    /// cannot be taken now, for the caller to give way and try again through its scheduler, for
    /// at most [`GIVE_WAY`], then fail naming the state. For a pool thread, which must not spin.
    pub fn try_hold(dir: &Path) -> Result<Result<Lease, Wait>, String> {
        note_use();
        if COLLECTING.load(Ordering::SeqCst) {
            // alloc: one entry a lease, taken by the collector's next turn
            USED.push(dir.to_path_buf());
        }
        for _ in 0..ATTEMPTS {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("making {}: {error}", dir.display()))?;
            let Some(file) = open_or_make(dir)? else { continue };
            let mut file = match Locked::try_lock_shared(file) {
                Ok(Ok(file)) => file,
                Ok(Err(_held)) => return Ok(Err(Wait::Busy)),
                Err(error) => return Err(format!("locking {}: {error}", lease_of(dir).display())),
            };
            match verified(&mut file, dir)? {
                Check::Own => return Ok(Ok(Lease::new(file, dir))),
                Check::NotOwn => {}
                Check::Short => {
                    // Never spun on: let go, rewritten if abandoned (then taken at once), and
                    // else tried again through the caller's scheduler.
                    drop(file);
                    if repair(dir)? {
                        continue;
                    }
                    return Ok(Err(Wait::Short));
                }
            }
        }
        Err(format!(
            "{} was collected under its lease {ATTEMPTS} times in a row while it was being taken",
            dir.display()
        ))
    }

    fn new(file: Locked, dir: &Path) -> Lease {
        Lease { _file: file, dir: dir.to_path_buf() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        RELEASED.fetch_add(1, Ordering::SeqCst);
    }
}

/// The shared lock on `file`, `dir`'s lease, waiting at most [`SHARED_WAIT`]: `None` when,
/// while it waited, the file stopped being `dir`'s lease (the collector renamed the directory),
/// so the caller makes it afresh rather than wait out the bound. For a thread that may wait (the
/// daemon's start); a pool thread takes [`Lease::try_hold`].
#[allow(
    clippy::disallowed_methods,
    reason = "a bounded wait on another process's lock, which has no wake to park on"
)]
fn lock_shared(mut file: File, dir: &Path) -> Result<Option<Locked>, String> {
    let _span = tracing::info_span!(target: "blaze", "waits on the gc lease", dir = %dir.display())
        .entered();
    let until = Instant::now() + SHARED_WAIT;
    let mut spin = blaze_spin::SpinWait::new();
    loop {
        match Locked::try_lock_shared(file) {
            Ok(Ok(locked)) => return Ok(Some(locked)),
            Ok(Err(back)) => file = back,
            Err(error) => return Err(format!("locking {}: {error}", lease_of(dir).display())),
        }
        if !same_file(&file, dir)? {
            return Ok(None);
        }
        if Instant::now() >= until {
            return Err(format!(
                "{} is being collected: its lease {} was held exclusively for {SHARED_WAIT:?}, longer than a rename takes",
                dir.display(),
                lease_of(dir).display()
            ));
        }
        if !spin.spin() {
            spin = blaze_spin::SpinWait::new();
        }
    }
}

/// Waits a little for `dir`'s lease file to hold a whole nonce, `since` when it was first seen
/// without one; past [`GIVE_WAY`] an error naming the directory and the state. For a thread that
/// may wait ([`Lease::hold`], the daemon's start).
#[allow(
    clippy::disallowed_methods,
    reason = "a bounded wait on another process's write, which has no wake to park on"
)]
fn wait_whole(
    dir: &Path,
    since: &mut Option<(Instant, blaze_spin::SpinWait)>,
) -> Result<(), String> {
    let _span =
        tracing::info_span!(target: "blaze", "waits on a lease file's nonce", dir = %dir.display())
            .entered();
    let (first, spin) = since.get_or_insert_with(|| (Instant::now(), blaze_spin::SpinWait::new()));
    if first.elapsed() >= GIVE_WAY {
        return Err(format!(
            "{}'s lease was not taken in {GIVE_WAY:?}: {}",
            dir.display(),
            Wait::Short
        ));
    }
    if !spin.spin() {
        *spin = blaze_spin::SpinWait::new();
    }
    Ok(())
}

pub fn lease_of(dir: &Path) -> PathBuf {
    dir.join(LEASE)
}

/// Whether `dir` has a lease file: only a directory whose users take leases is collected.
pub fn has_lease(dir: &Path) -> bool {
    lease_of(dir).is_file()
}

/// The nonce `file` holds, whole (one line ending in a newline); `None` while its maker has not
/// finished writing it, or never will ([`ABANDONED`]).
fn nonce_of(file: &mut File) -> std::io::Result<Option<String>> {
    let mut text = String::new();
    file.rewind()?;
    file.read_to_string(&mut text)?;
    Ok(text.ends_with('\n').then_some(text))
}

/// [`nonce_of`], read once, its error naming the file.
fn whole_nonce(file: &mut File, dir: &Path) -> Result<Option<String>, String> {
    nonce_of(file).map_err(|error| format!("reading {}: {error}", lease_of(dir).display()))
}

/// Takes `dir`'s lease file exclusively, never waiting, and writes a fresh nonce into it when it
/// is still without one and abandoned ([`rewrite_abandoned`]). `false` when it is held, gone, or
/// not abandoned yet.
fn repair(dir: &Path) -> Result<bool, String> {
    let path = lease_of(dir);
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("opening {}: {error}", path.display())),
    };
    match Locked::try_lock(file) {
        Ok(Ok(mut file)) => rewrite_abandoned(&mut file, dir),
        Ok(Err(_held)) => Ok(false),
        Err(error) => Err(format!("locking {}: {error}", path.display())),
    }
}

/// With `dir`'s lease held exclusively (by `file` or another handle of this process), writes a
/// fresh nonce into `file`, a writable handle of the lease file, when it is still the file at
/// the path, still holds no whole nonce, and was last written [`ABANDONED`] ago or more. `true`
/// when the file holds a whole nonce after it.
fn rewrite_abandoned(file: &mut File, dir: &Path) -> Result<bool, String> {
    let path = lease_of(dir);
    let failed = |error: std::io::Error| format!("repairing {}: {error}", path.display());
    if !same_file(file, dir)? {
        return Ok(false);
    }
    if whole_nonce(file, dir)?.is_some() {
        return Ok(true);
    }
    let written = file.metadata().and_then(|it| it.modified()).map_err(failed)?;
    // A clock behind the file's stamp says nothing of its age: left for a later look.
    match written.elapsed() {
        Ok(age) if age >= ABANDONED => {}
        _ => return Ok(false),
    }
    file.set_len(0).map_err(failed)?;
    file.rewind().map_err(failed)?;
    file.write_all(format!("{}\n", nonce()).as_bytes()).map_err(failed)?;
    // durable: a lease's nonce says who holds it: a crash must not leave a lease file with none, which reads as abandoned
    file.sync_all().map_err(failed)?;
    tracing::warn!(target: "blaze", lease = %path.display(), "a lease file its maker left without a nonce was given a fresh one");
    Ok(true)
}

/// How a locked lease file compares with its directory's ([`verified`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Check {
    /// The directory's own: the same file, with the same whole nonce.
    Own,
    /// Not the directory's: the path is gone, is another file (a copy too), or holds another
    /// nonce.
    NotOwn,
    /// No whole nonce yet: its maker is writing it, or died before it did ([`ABANDONED`]).
    Short,
}

/// Whether `file` is the file at `dir`'s lease path now: the same device and inode on unix.
/// Elsewhere (no inode in std) the nonces are compared ([`verified`]).
fn same_file(file: &File, dir: &Path) -> Result<bool, String> {
    let there = match std::fs::metadata(lease_of(dir)) {
        Ok(it) => it,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("reading {}: {error}", lease_of(dir).display())),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let held = file
            .metadata()
            .map_err(|error| format!("reading {}: {error}", lease_of(dir).display()))?;
        Ok(held.dev() == there.dev() && held.ino() == there.ino())
    }
    #[cfg(not(unix))]
    {
        _ = (file, there);
        Ok(true)
    }
}

/// Whether `file`, a lease file locked, is still `dir`'s: the same file as at `dir`'s lease
/// path ([`same_file`]), with the same whole nonce. Each nonce is read once, never waited for.
pub fn verified(file: &mut File, dir: &Path) -> Result<Check, String> {
    if !same_file(file, dir)? {
        return Ok(Check::NotOwn);
    }
    let Some(held) = whole_nonce(file, dir)? else { return Ok(Check::Short) };
    let mut there = match File::open(lease_of(dir)) {
        Ok(it) => it,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Check::NotOwn),
        Err(error) => return Err(format!("opening {}: {error}", lease_of(dir).display())),
    };
    Ok(match whole_nonce(&mut there, dir)? {
        Some(it) if it == held => Check::Own,
        Some(_) => Check::NotOwn,
        None => Check::Short,
    })
}

/// Makes `dir`'s lease file when it has none, taking no lock: for `blaze clean` on a build dir a
/// blaze before leases made, which then takes it exclusively.
pub fn make(dir: &Path) -> Result<(), String> {
    match open_or_make(dir)? {
        Some(_) => Ok(()),
        None => Err(format!("{} went away while its lease was being made", dir.display())),
    }
}

/// The lease file of `dir`, made with a fresh nonce when there is none: created exclusively
/// (one maker wins; no hard link, which FAT and exFAT lack), its nonce written as one line, so a
/// reader that sees no newline yet waits for it ([`whole_nonce`]). `None` when `dir` went away
/// meanwhile (collected): the caller makes it again.
fn open_or_make(dir: &Path) -> Result<Option<File>, String> {
    let path = lease_of(dir);
    match File::open(&path) {
        Ok(file) => return Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("opening {}: {error}", path.display())),
    }
    let made = (|| -> std::io::Result<()> {
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            // Another process made it first: its nonce is the lease's.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            Err(error) => return Err(error),
        };
        file.write_all(format!("{}\n", nonce()).as_bytes())?;
        // durable: a lease's nonce says who holds it: a crash must not leave a lease file with none, which reads as abandoned
        file.sync_all()
    })();
    match made {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("making {}: {error}", path.display())),
    }
    match File::open(&path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("opening {}: {error}", path.display())),
    }
}

/// A nonce no other lease file on the machine has: this process, the time, a count.
fn nonce() -> String {
    static MADE: AtomicU64 = AtomicU64::new(0);
    let count = MADE.fetch_add(1, Ordering::Relaxed);
    let id = blaze_ids::Hasher::new("blaze gc lease nonce 1")
        .part(&std::process::id().to_le_bytes())
        .part(&crate::record::now_ns().to_le_bytes())
        .part(&count.to_le_bytes())
        .finish();
    format!("{id:x}")
}

/// What the collector found of a lease it tried to take exclusively.
#[derive(Debug)]
pub enum Taken {
    /// Taken, and verified to be the directory's own: no one uses the directory while this is
    /// held, and it is let go of when dropped ([`Locked`]).
    Exclusive(Locked),
    /// Someone holds it shared.
    Held,
    /// The lease locked is no longer the directory's ([`verified`]): it was renamed away, and
    /// what is at the path now, if anything, is another directory.
    Moved,
    /// There is no lease file.
    Missing,
}

/// Takes `dir`'s lease exclusively, never waiting, and checks it is still `dir`'s.
pub fn take_exclusive(dir: &Path) -> Result<Taken, String> {
    let path = lease_of(dir);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Taken::Missing),
        Err(error) => return Err(format!("opening {}: {error}", path.display())),
    };
    // Locked from here: the probe's lock ([`held`]) and one that moved are let go of as they drop.
    let mut file = match Locked::try_lock(file) {
        Ok(Ok(file)) => file,
        Ok(Err(_held)) => return Ok(Taken::Held),
        Err(error) => return Err(format!("locking {}: {error}", path.display())),
    };
    match verified(&mut file, dir)? {
        Check::Own => Ok(Taken::Exclusive(file)),
        Check::NotOwn => Ok(Taken::Moved),
        Check::Short => {
            // Held exclusively, so no one uses it: an abandoned lease file gets a fresh nonce
            // through a writable handle. The directory is left this time either way (as a lease
            // that moved is); the next run finds it whole.
            match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(mut writable) => _ = rewrite_abandoned(&mut writable, dir)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("opening {}: {error}", path.display())),
            }
            Ok(Taken::Moved)
        }
    }
}

/// Whether someone holds `dir`'s lease now: the exclusive lock taken and let go at once. A lease
/// that moved counts as held: the scan leaves what it cannot be sure of.
pub fn held(dir: &Path) -> Result<bool, String> {
    Ok(matches!(take_exclusive(dir)?, Taken::Held | Taken::Moved))
}
