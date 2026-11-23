//! Synchronous kernel vnode watcher for the resident per-checkout `blaze` daemon
//! (Wave 7, rule 70 & rule 78).
//!
//! **The seam ([`WatchBackend`], rule 78).**
//! - [`WatchBackend::Kqueue`] (macOS, when `leaves + unique_dirs <= KQUEUE_CAP`): opens every
//!   watched directory and file with `O_EVTONLY | O_CLOEXEC` and registers `EVFILT_VNODE`
//!   (`EV_ADD | EV_CLEAR`) on one `kqueue`. Each request runs ONE non-blocking
//!   `kevent(kq, &[], &mut buf, Some(Timespec::ZERO))` (~10 µs): `0` events proves zero
//!   changes synchronously without ever flushing FSEvents or talking to `journal-helper`.
//! - [`WatchBackend::FsEventsHeld`] (when `> KQUEUE_CAP` or non-macOS): flushes the in-process
//!   `HeldChanges` stream directly, still eliminating the `journal-helper` socket hop.
//!
//! **Zero-TOCTOU registration invariant (macOS `KqueueVnode`).**
//! 1. Register every existing ancestor directory (shallowest to deepest) on `kq` first.
//! 2. Open each existing leaf with `O_EVTONLY | O_CLOEXEC` and register `EV_ADD | EV_CLEAR`
//!    on `kq` second.
//! 3. AFTER `EV_ADD`, call `fstat(fd)` + `lstat(path)` to verify `(st_dev, st_ino)` match and
//!    the stamp matches `pointer.leaves` (and for `ABSENT` slots, verify `ENOENT` after the
//!    parent directory is watched). Any mismatch immediately marks that path dirty for the next
//!    poll, closing the window between the previous check's reads and watch registration.
//!
//! **Crash-safe FSEvents cursor (rule 70).**
//! A passive `Dir::hold_changes` stream is kept open in-process WITHOUT calling `held.flush()`
//! on `kqueue`-served requests. Before persisting any new `Pointer`, `take_crash_cursor` calls
//! non-blocking `held.take()` (~5 µs) and persists `held.cursor()` (guaranteed `<=` edit time,
//! so a `SIGKILL` at any point falls back to a guaranteed-superset FSEvents replay).

use core::time::Duration;

use blaze_drive::Key;
use blaze_own::{AString, Alloc, BTreeSet, Vec};
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::fs::{Changes, Cursor, Dir, HeldChanges, ReadWrite};
use bstd::mem::Frame;

use crate::sweep::{ABSENT, AT_MOD_RS, BOTH, FACT_MOUNT, LeafKind, Leaves, MODULE, Stamp};

/// Maximum total `O_EVTONLY` descriptors held by one checkout's `KqueueVnode` watcher before
/// switching to [`WatchBackend::FsEventsHeld`].
pub(crate) const KQUEUE_CAP: usize = 8_192;

/// Deadline for starting the passive in-process FSEvents stream at daemon startup.
const HOLD_START_WAIT: Duration = Duration::from_secs(5);

/// Deadline for an in-process `HeldChanges::flush` on the [`WatchBackend::FsEventsHeld`] path.
const HOLD_FLUSH_WAIT: Duration = Duration::from_secs(2);

/// Deadline for cleanly ending the passive FSEvents stream on daemon shutdown.
const HOLD_END_WAIT: Duration = Duration::from_secs(2);

/// What [`CheckoutWatcher::poll`] observed since the last check.
pub(crate) enum PollOutcome<'f> {
    /// Zero kernel vnode events, zero initial mismatches, zero unwatched rows: 100% proven clean.
    Clean { crash_cursor: Option<Cursor> },
    /// Exact checkout-relative dirty paths proven by `kqueue` (may be empty if only home/git/unwatched
    /// rows need checking via `Taken::marked`).
    Dirty {
        paths: Vec<&'f str, &'f Frame>,
        crash_cursor: Option<Cursor>,
        needs_refresh: bool,
    },
    /// Full sweep needed (watcher unarmed, checkout root moved/deleted, or backend fault).
    FullRescan { crash_cursor: Option<Cursor>, needs_refresh: bool },
    /// Fallback in-process FSEvents flush result (when `> KQUEUE_CAP` or non-macOS).
    FsEvents { changes: Changes, cursor: Cursor },
}

/// Active watching strategy for one checkout (rule 78).
pub(crate) enum WatchBackend {
    #[cfg(target_os = "macos")]
    Kqueue(KqueueVnode),
    FsEventsHeld,
}

impl WatchBackend {
    /// Human-readable backend name for profiles and diagnostics.
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            #[cfg(target_os = "macos")]
            WatchBackend::Kqueue(_) => "kqueue-vnode",
            WatchBackend::FsEventsHeld => "fsevents-held",
        }
    }
}

/// Per-checkout watcher held by the resident daemon across requests.
pub(crate) struct CheckoutWatcher<'d> {
    checkout_root: AString<'static>,
    home_blaze_root: Option<AString<'static>>,
    held: Option<HeldChanges<'d, ReadWrite>>,
    backend: WatchBackend,
    armed_pointer: Option<Key>,
    has_unwatched_rows: bool,
    has_git_rows: bool,
    heap: Alloc<'static>,
}

impl<'d> CheckoutWatcher<'d> {
    /// Opens a watcher for `checkout_root` (canonical absolute path) and optional `home_blaze_root`
    /// (`<HOME>/.blaze`), starting a passive FSEvents stream when the OS supports it.
    pub(crate) fn new(
        cx: &Cx,
        root: &'d Dir<ReadWrite>,
        clock: &Clock,
        checkout_root: &str,
        home_blaze_root: Option<&str>,
        heap: Alloc<'static>,
    ) -> CheckoutWatcher<'d> {
        let held = root.hold_changes(&cx.until(clock.after(HOLD_START_WAIT))).ok();
        let home_blaze_root = home_blaze_root.map(|s| AString::from_str_in(s, heap));
        CheckoutWatcher {
            checkout_root: AString::from_str_in(checkout_root, heap),
            home_blaze_root,
            held,
            backend: WatchBackend::FsEventsHeld,
            armed_pointer: None,
            has_unwatched_rows: false,
            has_git_rows: false,
            heap,
        }
    }

    /// Active strategy name (`"kqueue-vnode"` or `"fsevents-held"`).
    pub(crate) fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Whether the watcher is currently armed on `pointer_key` with `KqueueVnode`.
    pub(crate) fn is_armed_for(&self, pointer_key: Key) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.armed_pointer == Some(pointer_key) && matches!(self.backend, WatchBackend::Kqueue(_))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = pointer_key;
            false
        }
    }

    /// Disarms the current vnode watch set (e.g. after detecting an external store generation flip).
    pub(crate) fn disarm(&mut self) {
        self.backend = WatchBackend::FsEventsHeld;
        self.armed_pointer = None;
        self.has_unwatched_rows = false;
        self.has_git_rows = false;
    }

    /// Arms or refreshes the synchronous vnode watcher over `leaves` for `pointer_key`.
    /// On macOS, falls back cleanly to [`WatchBackend::FsEventsHeld`] if the watch count exceeds
    /// [`KQUEUE_CAP`] or descriptor limits are reached.
    pub(crate) fn arm(&mut self, pointer_key: Key, leaves: &Leaves<'_>) {
        self.has_unwatched_rows = has_unwatched_leaf(leaves);
        self.has_git_rows = has_git_leaf(leaves);
        #[cfg(target_os = "macos")]
        {
            match KqueueVnode::arm(self.checkout_root.as_str(), self.home_blaze_root.as_deref(), leaves, self.heap) {
                Some(kq) => {
                    self.backend = WatchBackend::Kqueue(kq);
                    self.armed_pointer = Some(pointer_key);
                }
                None => {
                    self.backend = WatchBackend::FsEventsHeld;
                    self.armed_pointer = None;
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (pointer_key, leaves);
            self.backend = WatchBackend::FsEventsHeld;
            self.armed_pointer = None;
        }
    }

    /// Non-blocking crash-safe FSEvents cursor (`~5 µs`): drains any already-delivered FSEvents
    /// batch via `held.take()` without ever calling `held.flush()`.
    pub(crate) fn take_crash_cursor(&mut self, cx: &Cx) -> Option<Cursor> {
        let held = self.held.as_mut()?;
        let _ = held.take(cx);
        let cursor = held.cursor();
        (!matches!(cursor, Cursor::None)).then_some(cursor)
    }

    /// Polls for filesystem changes since the last check.
    ///
    /// - Under [`WatchBackend::Kqueue`] (macOS): ONE non-blocking `kevent(0)` (~10 µs) plus
    ///   non-blocking `held.take()` (~5 µs) for the crash-safe FSEvents cursor.
    /// - Under [`WatchBackend::FsEventsHeld`]: flushes the in-process `HeldChanges` stream.
    pub(crate) fn poll<'f>(&mut self, cx: &Cx, clock: &Clock, frame: &'f Frame) -> PollOutcome<'f> {
        #[cfg(target_os = "macos")]
        if let WatchBackend::Kqueue(kq) = &mut self.backend {
            let crash_cursor = {
                let held = self.held.as_mut();
                match held {
                    Some(held) => {
                        let _ = held.take(cx);
                        let cursor = held.cursor();
                        (!matches!(cursor, Cursor::None)).then_some(cursor)
                    }
                    None => None,
                }
            };
            let poll = kq.poll_dirty(frame);
            if poll.full_rescan {
                return PollOutcome::FullRescan { crash_cursor, needs_refresh: true };
            }
            if poll.paths.is_empty()
                && !poll.home_dirty
                && !poll.git_dirty
                && !poll.needs_refresh
                && !self.has_unwatched_rows
                && !self.has_git_rows
            {
                return PollOutcome::Clean { crash_cursor };
            }
            return PollOutcome::Dirty {
                paths: poll.paths,
                crash_cursor,
                needs_refresh: poll.needs_refresh,
            };
        }

        // Fallback: in-process HeldChanges flush (no separate journal-helper socket hop).
        if let Some(held) = self.held.as_mut() {
            let flush_cx = cx.until(clock.after(HOLD_FLUSH_WAIT));
            if let Ok(changes) = held.flush(&flush_cx) {
                let cursor = held.cursor();
                return PollOutcome::FsEvents { changes, cursor };
            }
        }
        let crash_cursor = self.take_crash_cursor(cx);
        PollOutcome::FullRescan { crash_cursor, needs_refresh: true }
    }

    /// Cleanly closes the passive FSEvents stream on daemon shutdown.
    pub(crate) fn shutdown(mut self, cx: &Cx, clock: &Clock) {
        self.disarm();
        if let Some(held) = self.held.take() {
            let _ = held.end(&cx.until(clock.after(HOLD_END_WAIT)));
        }
    }
}

fn has_unwatched_leaf(leaves: &Leaves<'_>) -> bool {
    for i in 0..leaves.len() {
        let row = leaves.row(i);
        let path = leaves.path(i);
        if row.kind == LeafKind::ScriptFile && path.starts_with(FACT_MOUNT) {
            return true;
        }
        if matches!(row.kind, LeafKind::ScriptFile | LeafKind::ScriptStat | LeafKind::ScriptListing)
            && (row.flags & crate::sweep::DIRECT_PATH == 0)
        {
            return true;
        }
    }
    false
}

fn has_git_leaf(leaves: &Leaves<'_>) -> bool {
    for i in 0..leaves.len() {
        if leaves.row(i).kind == LeafKind::ScriptGit {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------------------------
// macOS synchronous kqueue EVFILT_VNODE watcher (`O_EVTONLY | O_CLOEXEC`)
// ---------------------------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub(crate) struct KqueueVnode {
    kq: blaze_sys::posix::io::OwnedFd,
    entries: Vec<WatchEntry, Alloc<'static>>,
    /// All checkout-relative leaf paths in `pointer.leaves` (used when a dir vnode fires to
    /// dirty every leaf in/under that dir without walking disk).
    leaf_paths: Vec<AString<'static>, Alloc<'static>>,
    /// Leaves whose direct parent dir did not exist at arm time (so creation of an intermediate
    /// directory under root or an ancestor must dirty them).
    orphan_leaves: Vec<AString<'static>, Alloc<'static>>,
    /// Dirty paths discovered during zero-TOCTOU post-`EV_ADD` verification.
    initial_dirty: Vec<AString<'static>, Alloc<'static>>,
    initial_home_dirty: bool,
    initial_git_dirty: bool,
    initial_full_rescan: bool,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    CheckoutFile,
    CheckoutDir,
    HomeDir,
    GitPath,
}

#[cfg(target_os = "macos")]
struct WatchEntry {
    _fd: blaze_sys::posix::io::OwnedFd,
    kind: EntryKind,
    rel: AString<'static>,
}

#[cfg(target_os = "macos")]
pub(crate) struct KqueuePoll<'f> {
    pub(crate) paths: Vec<&'f str, &'f Frame>,
    pub(crate) needs_refresh: bool,
    pub(crate) home_dirty: bool,
    pub(crate) git_dirty: bool,
    pub(crate) full_rescan: bool,
}

#[cfg(target_os = "macos")]
const GIT_WATCH_PATHS: [&str; 8] = [
    ".git",
    ".git/HEAD",
    ".git/index",
    ".git/packed-refs",
    ".git/refs",
    ".git/refs/heads",
    ".git/refs/tags",
    ".git/refs/remotes",
];

#[cfg(target_os = "macos")]
fn all_vnode_notes() -> blaze_sys::posix::io::NoteFlags {
    use blaze_sys::posix::io::NoteFlags;
    NoteFlags::DELETE
        .with(NoteFlags::WRITE)
        .with(NoteFlags::EXTEND)
        .with(NoteFlags::ATTRIB)
        .with(NoteFlags::LINK)
        .with(NoteFlags::RENAME)
        .with(NoteFlags::REVOKE)
}

#[cfg(target_os = "macos")]
fn stamp_of_stat(st: &blaze_sys::posix::proc::stat) -> Stamp {
    let ns = |secs: i64, nsec: i64| secs.cast_unsigned().wrapping_mul(1_000_000_000).wrapping_add(nsec.cast_unsigned());
    Stamp {
        len: st.st_size.cast_unsigned(),
        mtime_ns: ns(st.st_mtime, st.st_mtime_nsec),
        ctime_ns: ns(st.st_ctime, st.st_ctime_nsec),
        ino: st.st_ino,
        dev: i64::from(st.st_dev).cast_unsigned(),
    }
}

#[cfg(target_os = "macos")]
fn join_abs<'a>(base: &str, rel: &str, buf: &'a mut [u8; 4096]) -> Option<blaze_sys::Path<'a>> {
    let base_b = base.trim_end_matches('/').as_bytes();
    let rel_b = rel.trim_start_matches('/').as_bytes();
    let needed = if rel_b.is_empty() {
        base_b.len().checked_add(1)?
    } else {
        base_b.len().checked_add(1)?.checked_add(rel_b.len())?.checked_add(1)?
    };
    if needed > buf.len() {
        return None;
    }
    let mut pos = 0usize;
    buf.get_mut(..base_b.len())?.copy_from_slice(base_b);
    pos += base_b.len();
    if !rel_b.is_empty() {
        *buf.get_mut(pos)? = b'/';
        pos += 1;
        buf.get_mut(pos..pos + rel_b.len())?.copy_from_slice(rel_b);
        pos += rel_b.len();
    }
    *buf.get_mut(pos)? = 0;
    let cstr = core::ffi::CStr::from_bytes_with_nul(buf.get(..=pos)?).ok()?;
    Some(blaze_sys::Path::new(cstr))
}

#[cfg(target_os = "macos")]
fn depth_of(rel: &str) -> usize {
    if rel.is_empty() {
        return 0;
    }
    rel.bytes().filter(|&b| b == b'/').count() + 1
}

#[cfg(target_os = "macos")]
fn push_ancestors(rel: &str, dirs: &mut BTreeSet<AString<'static>, Alloc<'static>>, heap: Alloc<'static>) {
    dirs.insert(AString::from_str_in("", heap));
    let mut at = 0usize;
    for (idx, byte) in rel.bytes().enumerate() {
        if byte == b'/' && idx > at {
            if let Some(prefix) = rel.get(..idx) {
                dirs.insert(AString::from_str_in(prefix, heap));
            }
            at = idx;
        }
    }
}

#[cfg(target_os = "macos")]
impl KqueueVnode {
    /// Arms `EVFILT_VNODE` watches for all directories and leaves in `leaves` following the
    /// zero-TOCTOU invariant (parents first, leaves second, `fstat`+`lstat`+stamp verification
    /// after `EV_ADD`). Returns `None` if `> KQUEUE_CAP` or kernel fd limits are reached.
    pub(crate) fn arm(
        checkout_root: &str,
        home_blaze_root: Option<&str>,
        leaves: &Leaves<'_>,
        heap: Alloc<'static>,
    ) -> Option<KqueueVnode> {
        use blaze_sys::Errno;
        use blaze_sys::posix::io::{CWD, Filter, Kevent, KeventFlags, OpenFlags, kqueue, kevent, openat, set_close_on_exec};
        use blaze_sys::posix::proc::{AT_SYMLINK_NOFOLLOW, S_IFDIR, S_IFMT, stat_at, stat_fd};

        let mut dir_set: BTreeSet<AString<'static>, Alloc<'static>> = BTreeSet::new_in(heap);
        dir_set.insert(AString::from_str_in("", heap));

        let mut leaf_paths: Vec<AString<'static>, Alloc<'static>> = Vec::new_in(heap);
        let mut dir_stamps: blaze_own::BTreeMap<AString<'static>, Stamp, Alloc<'static>> = blaze_own::BTreeMap::new_in(heap);
        let mut watch_files: Vec<(AString<'static>, Option<Stamp>, bool), Alloc<'static>> = Vec::new_in(heap);
        let mut absent_checks: Vec<(AString<'static>, bool), Alloc<'static>> = Vec::new_in(heap);
        let mut home_leaves: Vec<(AString<'static>, Option<Stamp>, bool), Alloc<'static>> = Vec::new_in(heap);
        let mut saw_git = false;

        for i in 0..leaves.len() {
            let row = leaves.row(i);
            let path = leaves.path(i);
            match row.kind {
                LeafKind::MacroFiles => continue,
                LeafKind::HomeListing => {
                    home_leaves.push((AString::from_str_in(path, heap), row.stamp, row.flags & ABSENT != 0));
                    continue;
                }
                LeafKind::ScriptGit => {
                    saw_git = true;
                    continue;
                }
                LeafKind::ScriptFile if path.starts_with(FACT_MOUNT) => continue,
                _ => {}
            }

            leaf_paths.push(AString::from_str_in(path, heap));
            push_ancestors(path, &mut dir_set, heap);

            let is_absent = row.flags & ABSENT != 0;
            let is_module = row.kind == LeafKind::Slot && (row.flags & MODULE != 0);
            let is_at_mod = row.kind == LeafKind::Slot && (row.flags & AT_MOD_RS != 0);
            let is_both = row.kind == LeafKind::Slot && (row.flags & BOTH != 0);

            if is_module {
                let stem = path.strip_suffix(".rs").unwrap_or(path);
                // Watch `x/` directory if it exists so creating `x/mod.rs` fires NOTE_WRITE on `x/`.
                dir_set.insert(AString::from_str_in(stem, heap));
            }

            match row.kind {
                LeafKind::Listing | LeafKind::MacroFolder | LeafKind::ScriptListing => {
                    if !is_absent {
                        dir_set.insert(AString::from_str_in(path, heap));
                        if let Some(st) = row.stamp {
                            dir_stamps.insert(AString::from_str_in(path, heap), st);
                        }
                    } else {
                        absent_checks.push((AString::from_str_in(path, heap), false));
                    }
                }
                LeafKind::Slot => {
                    if is_absent {
                        absent_checks.push((AString::from_str_in(path, heap), is_module));
                    } else if is_both {
                        watch_files.push((AString::from_str_in(path, heap), None, false));
                        let stem = path.strip_suffix(".rs").unwrap_or(path);
                        let mut mod_rs = AString::from_str_in(stem, heap);
                        mod_rs.push_str("/mod.rs");
                        watch_files.push((mod_rs, None, false));
                    } else if is_at_mod {
                        let stem = path.strip_suffix(".rs").unwrap_or(path);
                        let mut mod_rs = AString::from_str_in(stem, heap);
                        mod_rs.push_str("/mod.rs");
                        watch_files.push((mod_rs, row.stamp, false));
                    } else {
                        watch_files.push((AString::from_str_in(path, heap), row.stamp, false));
                    }
                }
                LeafKind::Manifest | LeafKind::MacroFile | LeafKind::ScriptFile | LeafKind::ScriptStat => {
                    if is_absent {
                        absent_checks.push((AString::from_str_in(path, heap), false));
                    } else {
                        let maybe_dir = row.kind == LeafKind::ScriptStat;
                        watch_files.push((AString::from_str_in(path, heap), row.stamp, maybe_dir));
                    }
                }
                LeafKind::HomeListing | LeafKind::MacroFiles | LeafKind::ScriptGit => {}
            }
        }

        // Sort dirs shallowest-first so parents are ALWAYS registered before children.
        let mut sorted_dirs: Vec<AString<'static>, Alloc<'static>> = Vec::with_capacity_in(dir_set.len(), heap);
        for d in dir_set {
            sorted_dirs.push(d);
        }
        sorted_dirs.sort_unstable_by(|a, b| depth_of(a.as_str()).cmp(&depth_of(b.as_str())).then_with(|| a.as_str().cmp(b.as_str())));

        // Deduplicate file watches.
        watch_files.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        watch_files.dedup_by(|a, b| a.0.as_str() == b.0.as_str());

        let git_count = if saw_git { GIT_WATCH_PATHS.len() } else { 0 };
        let estimated = sorted_dirs.len().saturating_add(watch_files.len()).saturating_add(home_leaves.len()).saturating_add(git_count);
        if estimated > KQUEUE_CAP {
            return None;
        }

        let kq = kqueue().ok()?;
        set_close_on_exec(kq.fd(), true).ok()?;

        let mut entries: Vec<WatchEntry, Alloc<'static>> = Vec::with_capacity_in(estimated.min(KQUEUE_CAP), heap);
        let mut watched_dirs: BTreeSet<AString<'static>, Alloc<'static>> = BTreeSet::new_in(heap);
        let mut initial_dirty: Vec<AString<'static>, Alloc<'static>> = Vec::new_in(heap);
        let mut initial_home_dirty = false;
        let initial_git_dirty = false;
        let mut initial_full_rescan = false;
        let notes = all_vnode_notes();
        let evtonly = OpenFlags::EVTONLY.with(OpenFlags::CLOSE_ON_EXEC);
        let mut path_buf = [0u8; 4096];

        // PHASE 1: Register every existing ancestor & listing directory FIRST.
        for dir_rel in sorted_dirs {
            if entries.len() >= KQUEUE_CAP {
                return None;
            }
            let Some(cpath) = join_abs(checkout_root, dir_rel.as_str(), &mut path_buf) else {
                initial_full_rescan = true;
                continue;
            };
            let fd = match openat(CWD, cpath, evtonly, 0) {
                Ok(fd) => fd,
                Err(Errno::NoEnt | Errno::NotDir) => {
                    // Optional `stem` dir of a `MODULE` slot may not exist; if a Listing leaf
                    // expected it to exist (had a stamp), mark it dirty.
                    if dir_stamps.contains_key(dir_rel.as_str()) {
                        initial_dirty.push(dir_rel);
                    }
                    continue;
                }
                Err(Errno::MFile | Errno::NFile) => return None,
                Err(_) => {
                    if dir_rel.is_empty() {
                        return None;
                    }
                    initial_dirty.push(dir_rel);
                    continue;
                }
            };
            let idx = entries.len();
            let ident = match usize::try_from(fd.fd().raw()) {
                Ok(raw) => raw,
                Err(_) => return None,
            };
            let ev = Kevent {
                ident,
                filter: Filter::VNODE,
                flags: KeventFlags::ADD.with(KeventFlags::CLEAR),
                fflags: notes,
                data: 0,
                udata: idx,
            };
            if kevent(kq.fd(), &[ev], &mut [], None).is_err() {
                return None;
            }
            // Zero-TOCTOU post-EV_ADD verification: fstat(fd) + lstat(path) must match!
            let f_st = stat_fd(fd.fd());
            let l_st = stat_at(CWD, cpath, AT_SYMLINK_NOFOLLOW);
            let verified = match (f_st, l_st) {
                (Ok(f), Ok(l)) => {
                    let same = f.st_dev == l.st_dev && f.st_ino == l.st_ino && (f.st_mode & S_IFMT) == S_IFDIR;
                    let stamp_ok = match dir_stamps.get(dir_rel.as_str()) {
                        Some(&expected) => stamp_of_stat(&f) == expected,
                        None => true,
                    };
                    same && stamp_ok
                }
                _ => false,
            };
            if !verified {
                if dir_rel.is_empty() {
                    initial_full_rescan = true;
                } else {
                    initial_dirty.push(AString::from_str_in(dir_rel.as_str(), heap));
                }
            }
            watched_dirs.insert(AString::from_str_in(dir_rel.as_str(), heap));
            entries.push(WatchEntry { _fd: fd, kind: EntryKind::CheckoutDir, rel: dir_rel });
        }

        // Track any leaf whose immediate parent directory did not exist at arm time so that
        // creating an intermediate directory under root or an ancestor dirties it.
        let mut orphan_leaves: Vec<AString<'static>, Alloc<'static>> = Vec::new_in(heap);
        for leaf in leaf_paths.iter() {
            let parent = leaf.as_str().rsplit_once('/').map_or("", |(p, _)| p);
            if !watched_dirs.contains(parent) {
                orphan_leaves.push(AString::from_str_in(leaf.as_str(), heap));
            }
        }

        // PHASE 2: Register existing leaf files SECOND (after parent dirs are already watched).
        for (file_rel, expected_stamp, maybe_dir) in watch_files {
            if entries.len() >= KQUEUE_CAP {
                return None;
            }
            let Some(cpath) = join_abs(checkout_root, file_rel.as_str(), &mut path_buf) else {
                initial_dirty.push(file_rel);
                continue;
            };
            let fd = match openat(CWD, cpath, evtonly, 0) {
                Ok(fd) => fd,
                Err(Errno::MFile | Errno::NFile) => return None,
                Err(_) => {
                    // Deleted or inaccessible since check -> mark dirty immediately.
                    initial_dirty.push(file_rel);
                    continue;
                }
            };
            let idx = entries.len();
            let ident = match usize::try_from(fd.fd().raw()) {
                Ok(raw) => raw,
                Err(_) => return None,
            };
            let ev = Kevent {
                ident,
                filter: Filter::VNODE,
                flags: KeventFlags::ADD.with(KeventFlags::CLEAR),
                fflags: notes,
                data: 0,
                udata: idx,
            };
            if kevent(kq.fd(), &[ev], &mut [], None).is_err() {
                return None;
            }
            let f_st = stat_fd(fd.fd());
            let l_st = stat_at(CWD, cpath, AT_SYMLINK_NOFOLLOW);
            let verified = match (f_st, l_st) {
                (Ok(f), Ok(l)) => {
                    let same = f.st_dev == l.st_dev && f.st_ino == l.st_ino;
                    let stamp_ok = match expected_stamp {
                        Some(expected) => stamp_of_stat(&f) == expected,
                        None => true,
                    };
                    let kind_ok = maybe_dir || (f.st_mode & S_IFMT) != S_IFDIR;
                    same && stamp_ok && kind_ok
                }
                _ => false,
            };
            if !verified {
                initial_dirty.push(AString::from_str_in(file_rel.as_str(), heap));
            }
            entries.push(WatchEntry { _fd: fd, kind: EntryKind::CheckoutFile, rel: file_rel });
        }

        // Verify ABSENT leaves are still absent AFTER their parent directories are registered.
        for (absent_rel, is_module) in absent_checks {
            let mut still_absent = true;
            if let Some(cpath) = join_abs(checkout_root, absent_rel.as_str(), &mut path_buf) {
                if !matches!(stat_at(CWD, cpath, AT_SYMLINK_NOFOLLOW), Err(Errno::NoEnt | Errno::NotDir)) {
                    still_absent = false;
                }
            } else {
                still_absent = false;
            }
            if still_absent && is_module {
                let stem = absent_rel.as_str().strip_suffix(".rs").unwrap_or(absent_rel.as_str());
                let mut mod_rs = AString::from_str_in(stem, heap);
                mod_rs.push_str("/mod.rs");
                if let Some(cpath) = join_abs(checkout_root, mod_rs.as_str(), &mut path_buf) {
                    if !matches!(stat_at(CWD, cpath, AT_SYMLINK_NOFOLLOW), Err(Errno::NoEnt | Errno::NotDir)) {
                        still_absent = false;
                    }
                }
            }
            if !still_absent {
                initial_dirty.push(absent_rel);
            }
        }

        // PHASE 3a: Watch HomeListing directories under `<HOME>/.blaze`.
        if let Some(home_base) = home_blaze_root {
            for (rel, expected_stamp, is_absent) in home_leaves {
                if entries.len() >= KQUEUE_CAP {
                    return None;
                }
                // Walk up to nearest existing directory under `<HOME>/.blaze` when absent.
                let mut probe = rel.as_str();
                let mut watched_exact = true;
                let opened = loop {
                    let Some(cpath) = join_abs(home_base, probe, &mut path_buf) else {
                        break None;
                    };
                    match openat(CWD, cpath, evtonly, 0) {
                        Ok(fd) => break Some((fd, probe, watched_exact)),
                        Err(Errno::MFile | Errno::NFile) => return None,
                        Err(Errno::NoEnt | Errno::NotDir) => {
                            watched_exact = false;
                            if probe.is_empty() {
                                break None;
                            }
                            probe = probe.rsplit_once('/').map_or("", |(p, _)| p);
                        }
                        Err(_) => break None,
                    }
                };
                let Some((fd, watched_rel, exact)) = opened else {
                    initial_home_dirty = true;
                    continue;
                };
                let idx = entries.len();
                let ident = match usize::try_from(fd.fd().raw()) {
                    Ok(raw) => raw,
                    Err(_) => return None,
                };
                let ev = Kevent {
                    ident,
                    filter: Filter::VNODE,
                    flags: KeventFlags::ADD.with(KeventFlags::CLEAR),
                    fflags: notes,
                    data: 0,
                    udata: idx,
                };
                if kevent(kq.fd(), &[ev], &mut [], None).is_err() {
                    return None;
                }
                // Verify post-EV_ADD state of home listing dir.
                if is_absent {
                    if exact {
                        initial_home_dirty = true;
                    } else if let Some(cpath) = join_abs(home_base, rel.as_str(), &mut path_buf) {
                        if !matches!(stat_at(CWD, cpath, AT_SYMLINK_NOFOLLOW), Err(Errno::NoEnt | Errno::NotDir)) {
                            initial_home_dirty = true;
                        }
                    }
                } else if !exact {
                    initial_home_dirty = true;
                } else if let Some(expected) = expected_stamp {
                    match stat_fd(fd.fd()) {
                        Ok(f) if stamp_of_stat(&f) == expected => {}
                        _ => initial_home_dirty = true,
                    }
                }
                entries.push(WatchEntry { _fd: fd, kind: EntryKind::HomeDir, rel: AString::from_str_in(watched_rel, heap) });
            }
        } else if !home_leaves.is_empty() {
            initial_home_dirty = true;
        }

        // PHASE 3b: Watch `.git` files/dirs if any `ScriptGit` leaf exists.
        if saw_git {
            for git_rel in GIT_WATCH_PATHS {
                if entries.len() >= KQUEUE_CAP {
                    return None;
                }
                let Some(cpath) = join_abs(checkout_root, git_rel, &mut path_buf) else {
                    continue;
                };
                let fd = match openat(CWD, cpath, evtonly, 0) {
                    Ok(fd) => fd,
                    Err(Errno::MFile | Errno::NFile) => return None,
                    Err(_) => continue,
                };
                let idx = entries.len();
                let ident = match usize::try_from(fd.fd().raw()) {
                    Ok(raw) => raw,
                    Err(_) => return None,
                };
                let ev = Kevent {
                    ident,
                    filter: Filter::VNODE,
                    flags: KeventFlags::ADD.with(KeventFlags::CLEAR),
                    fflags: notes,
                    data: 0,
                    udata: idx,
                };
                if kevent(kq.fd(), &[ev], &mut [], None).is_err() {
                    return None;
                }
                entries.push(WatchEntry { _fd: fd, kind: EntryKind::GitPath, rel: AString::from_str_in(git_rel, heap) });
            }
        }

        Some(KqueueVnode {
            kq,
            entries,
            leaf_paths,
            orphan_leaves,
            initial_dirty,
            initial_home_dirty,
            initial_git_dirty,
            initial_full_rescan,
        })
    }

    /// Number of open `O_EVTONLY` vnode descriptors currently watched.
    #[cfg(test)]
    pub(crate) fn watched_count(&self) -> usize {
        self.entries.len()
    }

    /// Drains all pending `EVFILT_VNODE` events with a single non-blocking `kevent(0)` loop
    /// (~10 µs when clean) and returns exact dirty checkout-relative paths.
    pub(crate) fn poll_dirty<'f>(&mut self, frame: &'f Frame) -> KqueuePoll<'f> {
        use blaze_sys::Timespec;
        use blaze_sys::posix::io::{Filter, Kevent, KeventFlags, NoteFlags, kevent};

        let mut paths: Vec<&'f str, &'f Frame> = Vec::new_in(frame);
        let mut needs_refresh = false;
        let mut home_dirty = core::mem::take(&mut self.initial_home_dirty);
        let mut git_dirty = core::mem::take(&mut self.initial_git_dirty);
        let mut full_rescan = core::mem::take(&mut self.initial_full_rescan);

        if !self.initial_dirty.is_empty() {
            needs_refresh = true;
            for p in self.initial_dirty.drain(..) {
                paths.push(frame.alloc_str(p.as_str()));
            }
        }

        let blank = Kevent {
            ident: 0,
            filter: Filter::VNODE,
            flags: KeventFlags::from_bits(0),
            fflags: NoteFlags::from_bits(0),
            data: 0,
            udata: 0,
        };
        let mut buf = [blank; 256];
        let mut dirty_dirs: Vec<usize, &'f Frame> = Vec::new_in(frame);
        let mut root_dir_wrote = false;

        // Bounded drain loop: each iteration drains up to 256 events; capped at 64 iterations
        // (16,384 events, well above KQUEUE_CAP) to guarantee bounded progress (rule 18).
        for _ in 0..64 {
            let n = match kevent(self.kq.fd(), &[], &mut buf, Some(Timespec::ZERO)) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => {
                    full_rescan = true;
                    break;
                }
            };
            let slice = buf.get(..n).unwrap_or(&[]);
            for ev in slice {
                let fflags = ev.fflags;
                let Some(entry) = self.entries.get(ev.udata) else {
                    full_rescan = true;
                    continue;
                };
                let gone = fflags.contains(NoteFlags::DELETE)
                    || fflags.contains(NoteFlags::RENAME)
                    || fflags.contains(NoteFlags::REVOKE);
                match entry.kind {
                    EntryKind::CheckoutFile => {
                        paths.push(frame.alloc_str(entry.rel.as_str()));
                        if gone {
                            needs_refresh = true;
                        }
                    }
                    EntryKind::CheckoutDir => {
                        needs_refresh = true;
                        if entry.rel.is_empty() {
                            if gone {
                                full_rescan = true;
                            } else {
                                root_dir_wrote = true;
                            }
                        } else {
                            paths.push(frame.alloc_str(entry.rel.as_str()));
                            dirty_dirs.push(ev.udata);
                        }
                    }
                    EntryKind::HomeDir => {
                        home_dirty = true;
                        if gone {
                            needs_refresh = true;
                        }
                    }
                    EntryKind::GitPath => {
                        git_dirty = true;
                        if gone {
                            needs_refresh = true;
                        }
                    }
                }
            }
            if n < buf.len() {
                break;
            }
        }

        // Expand directory events into all leaves in/under the mutated dir so atomic renames
        // and newly created files in absent slots are always caught.
        if !full_rescan && (root_dir_wrote || !dirty_dirs.is_empty()) {
            dirty_dirs.sort_unstable();
            dirty_dirs.dedup();
            if root_dir_wrote {
                paths.push("");
                for leaf in self.leaf_paths.iter() {
                    let rel = leaf.as_str();
                    // Root-level file or root-level `MODULE` slot (`foo.rs` / `foo/mod.rs`).
                    let is_root_child = !rel.contains('/')
                        || rel.strip_suffix("/mod.rs").is_some_and(|stem| !stem.contains('/'));
                    if is_root_child {
                        paths.push(frame.alloc_str(rel));
                    }
                }
                for orphan in self.orphan_leaves.iter() {
                    paths.push(frame.alloc_str(orphan.as_str()));
                }
            }
            for &dir_idx in dirty_dirs.iter() {
                let Some(dir_entry) = self.entries.get(dir_idx) else {
                    continue;
                };
                let dir_rel = dir_entry.rel.as_str();
                for leaf in self.leaf_paths.iter() {
                    let rel = leaf.as_str();
                    let under = rel == dir_rel
                        || rel.strip_prefix(dir_rel).is_some_and(|rest| rest.starts_with('/'))
                        || rel.strip_suffix(".rs").is_some_and(|stem| stem == dir_rel);
                    if under {
                        paths.push(frame.alloc_str(rel));
                    }
                }
            }
        }

        paths.sort_unstable();
        paths.dedup();

        KqueuePoll {
            paths,
            needs_refresh,
            home_dirty,
            git_dirty,
            full_rescan,
        }
    }
}
