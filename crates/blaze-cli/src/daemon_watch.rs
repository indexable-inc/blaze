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

use crate::sweep::{ABSENT, AT_MOD_RS, BOTH, DIRECT_PATH, FACT_MOUNT, LeafKind, Leaves, MODULE, SHIM_FILE, Stamp};

/// Maximum total `O_EVTONLY` descriptors held by one checkout's `KqueueVnode` watcher before
/// switching to [`WatchBackend::FsEventsHeld`].
pub(crate) const KQUEUE_CAP: usize = 32_768;

/// Maximum total Phase-1 directory probes (real dirs + optional `MODULE` stem dirs) before
/// switching to [`WatchBackend::FsEventsHeld`] so monorepo-scale absent `MODULE` stems cannot
/// cause unbounded synchronous `openat` probes.
const MAX_DIR_PROBES: usize = KQUEUE_CAP * 2;

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
    /// Exact checkout-relative dirty paths proven by `kqueue` (may be empty if only home/git/fact/unwatched
    /// rows need checking via `Taken::marked`).
    Dirty {
        paths: Vec<&'f str, &'f Frame>,
        crash_cursor: Option<Cursor>,
        needs_refresh: bool,
        force_script_asks: bool,
        facts_clean: bool,
    },
    /// Full sweep needed (watcher unarmed, checkout root moved/deleted, or backend fault).
    FullRescan { crash_cursor: Option<Cursor>, needs_refresh: bool },
    /// Fallback in-process FSEvents flush result (when `> KQUEUE_CAP` or non-macOS).
    /// `valid_since` is the cursor of the previous un-drained flush on this same `HeldChanges`
    /// stream (`None` if the stream was just created or drained by `take()`).
    FsEvents { changes: Changes, cursor: Cursor, valid_since: Option<Cursor> },
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
    exe_dir: Option<AString<'static>>,
    held: Option<HeldChanges<'d, ReadWrite>>,
    /// The cursor returned by the last `held.flush()` when no `held.take()` has drained events
    /// since: `FsEventsHeld` may only vouch for a stored pointer whose cursor equals this one.
    held_valid_since: Option<Cursor>,
    backend: WatchBackend,
    armed_pointer: Option<Key>,
    /// True when any `ScriptFile`/`ScriptStat`/`ScriptListing` leaf lacks `DIRECT_PATH` (e.g.
    /// traverses a symlink) or any `ScriptGit` leaf asks worktree status (`status`/`describe --dirty`).
    has_unwatched_scripts: bool,
    /// True when `KqueueVnode` successfully watched all `FACT_MOUNT` inputs (or none exist).
    facts_watched: bool,
    /// True when `KqueueVnode` successfully watched all non-status `ScriptGit` `.git` paths (or none exist).
    git_watched: bool,
    heap: Alloc<'static>,
}

impl<'d> CheckoutWatcher<'d> {
    /// Opens a watcher for `checkout_root` (canonical absolute path), optional `home_blaze_root`
    /// (`<HOME>/.blaze`), and optional `exe_dir` (parent folder of the `blaze` binary, for watching
    /// `blaze-fact-shim`), starting a passive FSEvents stream when the OS supports it.
    pub(crate) fn new(
        cx: &Cx,
        root: &'d Dir<ReadWrite>,
        clock: &Clock,
        checkout_root: &str,
        home_blaze_root: Option<&str>,
        exe_dir: Option<&str>,
        heap: Alloc<'static>,
    ) -> CheckoutWatcher<'d> {
        let held = root.hold_changes(&cx.until(clock.after(HOLD_START_WAIT))).ok();
        let home_blaze_root = home_blaze_root.map(|s| AString::from_str_in(s, heap));
        let exe_dir = exe_dir.map(|s| AString::from_str_in(s, heap));
        CheckoutWatcher {
            checkout_root: AString::from_str_in(checkout_root, heap),
            home_blaze_root,
            exe_dir,
            held,
            held_valid_since: None,
            backend: WatchBackend::FsEventsHeld,
            armed_pointer: None,
            has_unwatched_scripts: false,
            facts_watched: false,
            git_watched: false,
            heap,
        }
    }

    /// Active strategy name (`"kqueue-vnode"` or `"fsevents-held"`).
    pub(crate) fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Whether the watcher is currently armed with `KqueueVnode` (regardless of pointer key).
    pub(crate) fn is_armed(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.armed_pointer.is_some() && matches!(self.backend, WatchBackend::Kqueue(_))
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
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

    /// Advances the recorded armed pointer key without tearing down or reopening `KqueueVnode`
    /// (used after an in-place edit whose leaf set and watched inodes did not change).
    pub(crate) fn set_armed_pointer(&mut self, pointer_key: Key) {
        #[cfg(target_os = "macos")]
        if matches!(self.backend, WatchBackend::Kqueue(_)) {
            self.armed_pointer = Some(pointer_key);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = pointer_key;
        }
    }

    /// Disarms the current vnode watch set (e.g. after detecting an external store generation flip).
    pub(crate) fn disarm(&mut self) {
        self.backend = WatchBackend::FsEventsHeld;
        self.armed_pointer = None;
        self.held_valid_since = None;
        self.has_unwatched_scripts = false;
        self.facts_watched = false;
        self.git_watched = false;
    }

    /// Arms or refreshes the synchronous vnode watcher over `leaves` for `pointer_key`.
    /// On macOS, falls back cleanly to [`WatchBackend::FsEventsHeld`] if the watch count exceeds
    /// [`KQUEUE_CAP`] or descriptor limits are reached.
    pub(crate) fn arm(&mut self, cx: &Cx, root: &Dir<ReadWrite>, pointer_key: Key, leaves: &Leaves<'_>, frame: &Frame) {
        self.has_unwatched_scripts = has_unwatched_script_leaf(leaves);
        #[cfg(target_os = "macos")]
        {
            let tags_has_subdirs = git_tags_has_subdirs(cx, root, leaves, frame);
            match KqueueVnode::arm_with_env(
                self.checkout_root.as_str(),
                self.home_blaze_root.as_deref(),
                self.exe_dir.as_deref(),
                leaves,
                !tags_has_subdirs,
                frame,
                self.heap,
            ) {
                Some(mut kq) => {
                    kq.verify_root_listing(cx, root, leaves, frame);
                    self.facts_watched = kq.facts_watched;
                    self.git_watched = kq.git_watched;
                    self.backend = WatchBackend::Kqueue(kq);
                    self.armed_pointer = Some(pointer_key);
                }
                None => {
                    self.backend = WatchBackend::FsEventsHeld;
                    self.armed_pointer = None;
                    self.facts_watched = false;
                    self.git_watched = false;
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (cx, root, pointer_key, leaves, frame);
            self.backend = WatchBackend::FsEventsHeld;
            self.armed_pointer = None;
            self.facts_watched = false;
            self.git_watched = false;
        }
    }

    /// Non-blocking crash-safe FSEvents cursor (`~5 µs`): drains any already-delivered FSEvents
    /// batch via `held.take()` without ever calling `held.flush()`.
    pub(crate) fn take_crash_cursor(&mut self, cx: &Cx) -> Option<Cursor> {
        let held = self.held.as_mut()?;
        self.held_valid_since = None;
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
                        self.held_valid_since = None;
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
            let facts_clean = self.facts_watched && !poll.facts_dirty;
            let git_clean = self.git_watched && !poll.git_dirty;
            if poll.paths.is_empty()
                && !poll.home_dirty
                && git_clean
                && facts_clean
                && !poll.needs_refresh
                && !self.has_unwatched_scripts
            {
                return PollOutcome::Clean { crash_cursor };
            }
            return PollOutcome::Dirty {
                paths: poll.paths,
                crash_cursor,
                needs_refresh: poll.needs_refresh,
                force_script_asks: !git_clean || self.has_unwatched_scripts,
                facts_clean,
            };
        }

        // Fallback: in-process HeldChanges flush (no separate journal-helper socket hop).
        if let Some(held) = self.held.as_mut() {
            let valid_since = self.held_valid_since;
            let flush_cx = cx.until(clock.after(HOLD_FLUSH_WAIT));
            if let Ok(changes) = held.flush(&flush_cx) {
                let cursor = held.cursor();
                self.held_valid_since = (!matches!(cursor, Cursor::None)).then_some(cursor);
                return PollOutcome::FsEvents { changes, cursor, valid_since };
            }
            self.held_valid_since = None;
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

/// Returns `true` if `path` (from a `LeafKind::ScriptGit` row) queries worktree status (which
/// can depend on untracked files in arbitrary unwatched directories).
fn is_status_git_leaf(path: &str) -> bool {
    match path.split_once('\n') {
        None => true,
        Some((_, command)) => crate::engine_script_reads::reads_status(command),
    }
}

/// Returns `true` if `leaves` contains a non-`FACT_MOUNT` script read without `DIRECT_PATH`
/// (e.g. traversing a symlink) or a worktree-status `ScriptGit` leaf.
fn has_unwatched_script_leaf(leaves: &Leaves<'_>) -> bool {
    for i in 0..leaves.len() {
        let row = leaves.row(i);
        let path = leaves.path(i);
        if row.kind == LeafKind::ScriptFile && path.starts_with(FACT_MOUNT) {
            continue;
        }
        if matches!(row.kind, LeafKind::ScriptFile | LeafKind::ScriptStat | LeafKind::ScriptListing)
            && (row.flags & DIRECT_PATH == 0)
        {
            return true;
        }
        if row.kind == LeafKind::ScriptGit && is_status_git_leaf(path) {
            return true;
        }
    }
    false
}

/// If any `ScriptGit` leaf runs `describe`, checks whether `<repo>/.git/refs/tags` for the
/// nearest enclosing repository of that leaf's folder contains nested subdirectories (in which
/// case watching `refs/tags` alone wouldn't see writes in subdirs).
#[cfg(target_os = "macos")]
fn git_tags_has_subdirs(cx: &Cx, root: &Dir<ReadWrite>, leaves: &Leaves<'_>, frame: &Frame) -> bool {
    for i in 0..leaves.len() {
        let row = leaves.row(i);
        if row.kind != LeafKind::ScriptGit {
            continue;
        }
        let path = leaves.path(i);
        let Some((folder, cmd)) = path.split_once('\n') else {
            return true;
        };
        if !cmd.starts_with("describe ") {
            continue;
        }
        // Walk up from `folder` to checkout root `""` to locate the enclosing `.git` directory.
        let mut cur = folder.trim_matches('/');
        let mut found_repo = false;
        for _ in 0..64 {
            let dot_git = if cur.is_empty() {
                bstd::mem::format_in!(frame, ".git")
            } else {
                bstd::mem::format_in!(frame, "{cur}/.git")
            };
            if let Ok(gp) = bstd::path::Path::new(dot_git.as_str())
                && matches!(root.stat(cx, &gp), Ok(Some(_)))
            {
                found_repo = true;
                let tags_rel = bstd::mem::format_in!(frame, "{}/refs/tags", dot_git.as_str());
                let Ok(tp) = bstd::path::Path::new(tags_rel.as_str()) else {
                    return true;
                };
                match root.entries(cx, &tp) {
                    Ok(Some(listed)) => {
                        if (0..listed.len()).any(|j| listed.kind(j) == bstd::fs::EntryKind::Dir) {
                            return true;
                        }
                    }
                    Ok(None) => {}
                    Err(_) => return true,
                }
                break;
            }
            if cur.is_empty() {
                break;
            }
            cur = cur.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
        if !found_repo {
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
    /// Root-level watched directories that are symlinks (e.g. `member -> target-a`): atomic
    /// retargeting fires `NOTE_WRITE` on the checkout root without touching the old target inode,
    /// so `root_dir_wrote` must dirty every leaf beneath these prefixes.
    root_symlink_dirs: Vec<AString<'static>, Alloc<'static>>,
    /// Dirty paths discovered during zero-TOCTOU post-`EV_ADD` verification.
    initial_dirty: Vec<AString<'static>, Alloc<'static>>,
    /// True only when a structural/inode mismatch or absent-file appearance during `arm` requires
    /// rebuilding the kqueue descriptors (NOT set for stamp-only mismatches on live watched inodes).
    initial_needs_refresh: bool,
    initial_home_dirty: bool,
    initial_git_dirty: bool,
    initial_facts_dirty: bool,
    initial_full_rescan: bool,
    /// True when all `FACT_MOUNT` inputs are watched (or no `FACT_MOUNT` leaves exist).
    facts_watched: bool,
    /// True when all non-status `ScriptGit` `.git` paths are watched (or no `ScriptGit` leaves exist).
    git_watched: bool,
    /// When a `SHIM_FILE` leaf existed and `<exe_dir>/blaze-fact-shim` was absent at arm time,
    /// its absolute path so `poll_dirty` verifies `ENOENT` in one `stat_at` (~1 µs).
    shim_absent_path: Option<AString<'static>>,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    CheckoutFile,
    CheckoutDir,
    HomeDir,
    GitPath,
    FactPath,
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
    pub(crate) facts_dirty: bool,
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

/// Zero-TOCTOU post-`EV_ADD` verification that `fd` still refers to the inode currently at `cpath`.
/// `openat(O_EVTONLY)` without `O_SYMLINK` follows symlinks, so `fstat(fd)` reports the target's
/// `(st_dev, st_ino)` while `lstat` reports the link's own; when `lstat` sees `S_IFLNK`, verify
/// against `stat_at(..., 0)` so symlinks verify cleanly instead of looping `initial_needs_refresh`.
#[cfg(target_os = "macos")]
fn fd_matches_path(fd: blaze_sys::Fd, cpath: blaze_sys::Path<'_>) -> Option<blaze_sys::posix::proc::stat> {
    use blaze_sys::posix::io::CWD;
    use blaze_sys::posix::proc::{AT_SYMLINK_NOFOLLOW, S_IFLNK, S_IFMT, stat_at, stat_fd};
    let f = stat_fd(fd).ok()?;
    let l = stat_at(CWD, cpath, AT_SYMLINK_NOFOLLOW).ok()?;
    if f.st_dev == l.st_dev && f.st_ino == l.st_ino {
        return Some(f);
    }
    if (l.st_mode & S_IFMT) == S_IFLNK {
        let t = stat_at(CWD, cpath, 0).ok()?;
        if f.st_dev == t.st_dev && f.st_ino == t.st_ino {
            return Some(f);
        }
    }
    None
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
fn cpath_of<'a>(abs: &str, buf: &'a mut [u8; 4096]) -> Option<blaze_sys::Path<'a>> {
    let bytes = abs.as_bytes();
    if bytes.is_empty() || bytes.len() >= buf.len() || bytes.contains(&0) {
        return None;
    }
    buf.get_mut(..bytes.len())?.copy_from_slice(bytes);
    *buf.get_mut(bytes.len())? = 0;
    let cstr = core::ffi::CStr::from_bytes_with_nul(buf.get(..=bytes.len())?).ok()?;
    Some(blaze_sys::Path::new(cstr))
}

#[cfg(target_os = "macos")]
fn read_git_head_ref<'a>(checkout_root: &str, repo_rel: &str, out: &'a mut [u8; 512], frame: &Frame) -> Option<&'a str> {
    use blaze_sys::posix::io::{CWD, OpenFlags, openat, read};
    let mut path_buf = [0u8; 4096];
    let head_rel = if repo_rel.is_empty() {
        bstd::mem::format_in!(frame, ".git/HEAD")
    } else {
        bstd::mem::format_in!(frame, "{repo_rel}/.git/HEAD")
    };
    let cpath = join_abs(checkout_root, head_rel.as_str(), &mut path_buf)?;
    let fd = openat(CWD, cpath, OpenFlags::READ.with(OpenFlags::CLOSE_ON_EXEC), 0).ok()?;
    let mut filled = 0usize;
    while filled < out.len() {
        let Some(slot) = out.get_mut(filled..) else {
            break;
        };
        match read(fd.fd(), slot) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(blaze_sys::Errno::Intr) => continue,
            Err(_) => return None,
        }
    }
    let text = core::str::from_utf8(out.get(..filled)?).ok()?.trim();
    let target = text.strip_prefix("ref:")?.trim();
    if target.is_empty() || target.starts_with('/') || target.split('/').any(|p| p.is_empty() || p == "." || p == "..") {
        return None;
    }
    Some(target)
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
    /// Convenience wrapper for unit tests without `exe_dir` or `Frame`.
    #[cfg(test)]
    pub(crate) fn arm(
        checkout_root: &str,
        home_blaze_root: Option<&str>,
        leaves: &Leaves<'_>,
        heap: Alloc<'static>,
    ) -> Option<KqueueVnode> {
        let frame = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).ok()?;
        KqueueVnode::arm_with_env(checkout_root, home_blaze_root, None, leaves, true, &frame, heap)
    }

    /// Arms `EVFILT_VNODE` watches for all directories and leaves in `leaves` following the
    /// zero-TOCTOU invariant (parents first, leaves second, `fstat`+`lstat`+stamp verification
    /// after `EV_ADD`). Returns `None` if `> KQUEUE_CAP` or kernel fd limits are reached.
    pub(crate) fn arm_with_env(
        checkout_root: &str,
        home_blaze_root: Option<&str>,
        exe_dir: Option<&str>,
        leaves: &Leaves<'_>,
        git_tags_flat: bool,
        frame: &Frame,
        heap: Alloc<'static>,
    ) -> Option<KqueueVnode> {
        use blaze_sys::Errno;
        use blaze_sys::posix::io::{CWD, Filter, Kevent, KeventFlags, OpenFlags, kqueue, kevent, openat, set_close_on_exec};
        use blaze_sys::posix::proc::{AT_SYMLINK_NOFOLLOW, S_IFDIR, S_IFLNK, S_IFMT, stat_at, stat_fd};

        let mut dir_set: BTreeSet<AString<'static>, Alloc<'static>> = BTreeSet::new_in(heap);
        dir_set.insert(AString::from_str_in("", heap));
        // Optional `x/` stem directories for `MODULE` slots (`x.rs`): most do not exist on disk,
        // so track them apart from `dir_set` until after the pre-open `KQUEUE_CAP` estimate.
        let mut stem_dirs: BTreeSet<AString<'static>, Alloc<'static>> = BTreeSet::new_in(heap);

        let mut leaf_paths: Vec<AString<'static>, Alloc<'static>> = Vec::new_in(heap);
        let mut dir_stamps: blaze_own::BTreeMap<AString<'static>, Option<Stamp>, Alloc<'static>> = blaze_own::BTreeMap::new_in(heap);
        let mut watch_files: Vec<(AString<'static>, Option<Stamp>, bool), Alloc<'static>> = Vec::new_in(heap);
        let mut absent_checks: Vec<(AString<'static>, bool), Alloc<'static>> = Vec::new_in(heap);
        let mut home_leaves: Vec<(AString<'static>, Option<Stamp>, bool), Alloc<'static>> = Vec::new_in(heap);
        let mut git_folders: BTreeSet<AString<'static>, Alloc<'static>> = BTreeSet::new_in(heap);
        let mut saw_git = false;
        let mut has_status_git = false;
        let mut saw_facts = false;
        let mut saw_shim = false;

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
                    if is_status_git_leaf(path) {
                        has_status_git = true;
                    } else if let Some((folder, _)) = path.split_once('\n') {
                        git_folders.insert(AString::from_str_in(folder.trim_matches('/'), heap));
                    } else {
                        has_status_git = true;
                    }
                    continue;
                }
                LeafKind::ScriptFile if path.starts_with(FACT_MOUNT) => {
                    saw_facts = true;
                    if path == SHIM_FILE {
                        saw_shim = true;
                    }
                    continue;
                }
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
                // Kept in `stem_dirs` so nonexistent stem dirs do not double-count against `KQUEUE_CAP`.
                stem_dirs.insert(AString::from_str_in(stem, heap));
            }

            match row.kind {
                LeafKind::Listing | LeafKind::MacroFolder | LeafKind::ScriptListing => {
                    let script_none = row.kind == LeafKind::ScriptListing && row.id == crate::engine_script_reads::NONE;
                    let script_not_dir = row.kind == LeafKind::ScriptListing && row.id == crate::engine_script_reads::NOT_A_FOLDER;
                    if script_not_dir {
                        watch_files.push((AString::from_str_in(path, heap), row.stamp, false));
                    } else if is_absent || script_none {
                        absent_checks.push((AString::from_str_in(path, heap), false));
                    } else {
                        dir_set.insert(AString::from_str_in(path, heap));
                        dir_stamps
                            .entry(AString::from_str_in(path, heap))
                            .and_modify(|existing| {
                                if *existing != row.stamp {
                                    *existing = None;
                                }
                            })
                            .or_insert(row.stamp);
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
                        if is_module {
                            absent_checks.push((AString::from_str_in(path, heap), false));
                        }
                    } else {
                        watch_files.push((AString::from_str_in(path, heap), row.stamp, false));
                        if is_module {
                            let stem = path.strip_suffix(".rs").unwrap_or(path);
                            let mut mod_rs = AString::from_str_in(stem, heap);
                            mod_rs.push_str("/mod.rs");
                            absent_checks.push((mod_rs, false));
                        }
                    }
                }
                LeafKind::Manifest | LeafKind::MacroFile | LeafKind::ScriptFile | LeafKind::ScriptStat => {
                    let script_none = matches!(row.kind, LeafKind::ScriptFile | LeafKind::ScriptStat)
                        && row.id == crate::engine_script_reads::NONE;
                    if is_absent || script_none {
                        absent_checks.push((AString::from_str_in(path, heap), false));
                    } else if path.is_empty() {
                        // Checkout root `""` is already watched in Phase 1 (`dir_set`); mark its
                        // stamp `None` so `verify_root_listing` checks every root leaf post-`EV_ADD`.
                        dir_stamps.insert(AString::from_str_in("", heap), None);
                    } else {
                        let maybe_dir = row.kind == LeafKind::ScriptStat
                            || (row.kind == LeafKind::ScriptFile && row.id == crate::engine_script_reads::NOT_A_FILE);
                        watch_files.push((AString::from_str_in(path, heap), row.stamp, maybe_dir));
                    }
                }
                LeafKind::HomeListing | LeafKind::MacroFiles | LeafKind::ScriptGit => {}
            }
        }

        // Deduplicate file watches soundly: if ANY row for a path has `None` or a different stamp,
        // merge to `None` (never let `Some(stamp)` hide an unsettled/unstamped `None` row!).
        watch_files.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        watch_files.dedup_by(|a, b| {
            if a.0.as_str() == b.0.as_str() {
                if b.1 != a.1 {
                    b.1 = None;
                }
                b.2 = b.2 && a.2;
                true
            } else {
                false
            }
        });

        let git_count = if saw_git { git_folders.len().max(1).saturating_mul(GIT_WATCH_PATHS.len() + 8) } else { 0 };
        let fact_count = if saw_facts { 2 } else { 0 };
        // Pre-reject only on real directories + files + home/git/fact watches (NOT nonexistent
        // `MODULE` stem directories); actual opened FDs are still hard-capped at `KQUEUE_CAP`
        // inside every registration loop below, and total Phase-1 probes (including optional
        // `MODULE` stem dirs) are bounded by `MAX_DIR_PROBES`.
        let estimated = dir_set
            .len()
            .saturating_add(watch_files.len())
            .saturating_add(home_leaves.len())
            .saturating_add(git_count)
            .saturating_add(fact_count);
        if estimated > KQUEUE_CAP || dir_set.len().saturating_add(stem_dirs.len()) > MAX_DIR_PROBES {
            return None;
        }

        // Merge optional `MODULE` stem directories into `dir_set` and sort shallowest-first so
        // every existing directory (including existing stem dirs) is registered BEFORE Phase 2.
        for stem in stem_dirs {
            dir_set.insert(stem);
        }
        let mut sorted_dirs: Vec<AString<'static>, Alloc<'static>> = Vec::with_capacity_in(dir_set.len(), heap);
        for d in dir_set {
            sorted_dirs.push(d);
        }
        sorted_dirs.sort_unstable_by(|a, b| depth_of(a.as_str()).cmp(&depth_of(b.as_str())).then_with(|| a.as_str().cmp(b.as_str())));

        let kq = kqueue().ok()?;
        set_close_on_exec(kq.fd(), true).ok()?;

        let mut entries: Vec<WatchEntry, Alloc<'static>> = Vec::with_capacity_in(estimated.min(KQUEUE_CAP), heap);
        let mut watched_dirs: BTreeSet<AString<'static>, Alloc<'static>> = BTreeSet::new_in(heap);
        let mut root_symlink_dirs: Vec<AString<'static>, Alloc<'static>> = Vec::new_in(heap);
        let mut initial_dirty: Vec<AString<'static>, Alloc<'static>> = Vec::new_in(heap);
        let mut initial_needs_refresh = false;
        let mut initial_home_dirty = false;
        let mut initial_git_dirty = false;
        let mut initial_facts_dirty = false;
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
                    // expected it to exist, mark it dirty + refresh needed.
                    if dir_stamps.contains_key(dir_rel.as_str()) {
                        initial_needs_refresh = true;
                        initial_dirty.push(dir_rel);
                    }
                    continue;
                }
                Err(Errno::MFile | Errno::NFile) => return None,
                Err(_) => {
                    if dir_rel.is_empty() {
                        return None;
                    }
                    initial_needs_refresh = true;
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
            // Zero-TOCTOU post-EV_ADD verification: fstat(fd) + path stat must match!
            let (same_inode, stamp_ok) = match fd_matches_path(fd.fd(), cpath) {
                Some(f) if (f.st_mode & S_IFMT) == S_IFDIR => {
                    let stamp_ok = match dir_stamps.get(dir_rel.as_str()) {
                        Some(Some(expected)) => stamp_of_stat(&f) == *expected,
                        // Listing leaf had `stamp == None` (unsettled dir or checkout root `""`):
                        // NEVER assume clean!
                        Some(None) => false,
                        // Ancestor/module-stem dir only (not a Listing leaf itself).
                        None => true,
                    };
                    (true, stamp_ok)
                }
                _ => (false, false),
            };
            if !same_inode {
                initial_needs_refresh = true;
                if dir_rel.is_empty() {
                    initial_full_rescan = true;
                } else {
                    initial_dirty.push(AString::from_str_in(dir_rel.as_str(), heap));
                }
            } else if !stamp_ok {
                // Live dir inode IS watched on `kq`; mark dirty for the next poll without
                // triggering a full kqueue rebuild.
                initial_dirty.push(AString::from_str_in(dir_rel.as_str(), heap));
            }
            if !dir_rel.is_empty()
                && !dir_rel.as_str().contains('/')
                && stat_at(CWD, cpath, AT_SYMLINK_NOFOLLOW).is_ok_and(|l| (l.st_mode & S_IFMT) == S_IFLNK)
            {
                root_symlink_dirs.push(AString::from_str_in(dir_rel.as_str(), heap));
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
                initial_needs_refresh = true;
                initial_dirty.push(file_rel);
                continue;
            };
            let fd = match openat(CWD, cpath, evtonly, 0) {
                Ok(fd) => fd,
                Err(Errno::MFile | Errno::NFile) => return None,
                Err(_) => {
                    // Deleted or inaccessible since check -> mark dirty + refresh needed.
                    initial_needs_refresh = true;
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
            let (same_inode, stamp_ok, kind_ok) = match fd_matches_path(fd.fd(), cpath) {
                Some(f) => {
                    // NEVER assume `expected_stamp == None` (unsettled/racy-window file or `BOTH`)
                    // is clean: always force verification on the next poll!
                    let stamp_ok = match expected_stamp {
                        Some(expected) => stamp_of_stat(&f) == expected,
                        None => false,
                    };
                    let kind_ok = maybe_dir || (f.st_mode & S_IFMT) != S_IFDIR;
                    (true, stamp_ok, kind_ok)
                }
                None => (false, false, false),
            };
            if !same_inode || !kind_ok {
                initial_needs_refresh = true;
                initial_dirty.push(AString::from_str_in(file_rel.as_str(), heap));
            } else if !stamp_ok {
                // Live file inode IS watched on `kq`; mark dirty for the next poll without
                // tearing down and rebuilding all kqueue FDs.
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
                initial_needs_refresh = true;
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
                // Verify post-EV_ADD state of home listing dir (never assume `expected_stamp == None` is clean).
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
                } else {
                    match (expected_stamp, stat_fd(fd.fd())) {
                        (Some(expected), Ok(f)) if stamp_of_stat(&f) == expected => {}
                        _ => initial_home_dirty = true,
                    }
                }
                entries.push(WatchEntry { _fd: fd, kind: EntryKind::HomeDir, rel: AString::from_str_in(watched_rel, heap) });
            }
        } else if !home_leaves.is_empty() {
            initial_home_dirty = true;
        }

        // PHASE 3b: Resolve and watch each `ScriptGit` leaf folder's enclosing `.git` repository
        // (including nested repos and intermediate directories where a new `.git` could appear).
        let mut git_watched = !saw_git;
        if saw_git && !has_status_git && git_tags_flat && !git_folders.is_empty() {
            let mut git_paths: BTreeSet<AString<'static>, Alloc<'static>> = BTreeSet::new_in(heap);
            let mut required_git_paths: Vec<AString<'static>, Alloc<'static>> = Vec::new_in(heap);
            let mut repos_ok = true;
            for folder in git_folders.iter() {
                let mut cur = folder.as_str();
                let mut found_repo: Option<&str> = None;
                for _ in 0..64 {
                    let dot_git = if cur.is_empty() {
                        bstd::mem::format_in!(frame, ".git")
                    } else {
                        bstd::mem::format_in!(frame, "{cur}/.git")
                    };
                    if let Some(cpath) = join_abs(checkout_root, dot_git.as_str(), &mut path_buf)
                        && let Ok(st) = stat_at(CWD, cpath, AT_SYMLINK_NOFOLLOW)
                    {
                        if (st.st_mode & S_IFMT) == S_IFDIR {
                            found_repo = Some(cur);
                        } else {
                            // Gitlink file or symlink `.git` -> keep on safe re-ask path.
                            repos_ok = false;
                        }
                        break;
                    }
                    // Watch this intermediate directory so creating a closer `.git` invalidates.
                    git_paths.insert(AString::from_str_in(cur, heap));
                    if cur.is_empty() {
                        break;
                    }
                    cur = cur.rsplit_once('/').map_or("", |(parent, _)| parent);
                }
                let Some(repo) = found_repo else {
                    repos_ok = false;
                    break;
                };
                let prefix_path = |sub: &str| -> AString<'static> {
                    if repo.is_empty() {
                        AString::from_str_in(sub, heap)
                    } else {
                        let mut s = AString::from_str_in(repo, heap);
                        s.push_str("/");
                        s.push_str(sub);
                        s
                    }
                };
                for p in GIT_WATCH_PATHS {
                    git_paths.insert(prefix_path(p));
                }
                required_git_paths.push(prefix_path(".git"));
                required_git_paths.push(prefix_path(".git/HEAD"));
                let mut head_buf = [0u8; 512];
                if let Some(ref_rel) = read_git_head_ref(checkout_root, repo, &mut head_buf, frame) {
                    let mut full = prefix_path(".git/");
                    full.push_str(ref_rel);
                    // Add all ancestor directories under `<repo>/.git/`.
                    let mut at = 0usize;
                    for (idx, byte) in full.as_str().bytes().enumerate() {
                        if byte == b'/' && idx > at {
                            if let Some(prefix) = full.as_str().get(..idx) {
                                git_paths.insert(AString::from_str_in(prefix, heap));
                            }
                            at = idx;
                        }
                    }
                    git_paths.insert(full);
                }
            }
            if repos_ok {
                let mut watched_git_set: BTreeSet<AString<'static>, Alloc<'static>> = BTreeSet::new_in(heap);
                for git_rel in git_paths {
                    if entries.len() >= KQUEUE_CAP {
                        return None;
                    }
                    let Some(cpath) = join_abs(checkout_root, git_rel.as_str(), &mut path_buf) else {
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
                    watched_git_set.insert(AString::from_str_in(git_rel.as_str(), heap));
                    entries.push(WatchEntry { _fd: fd, kind: EntryKind::GitPath, rel: git_rel });
                }
                if required_git_paths.iter().all(|req| watched_git_set.contains(req.as_str())) {
                    git_watched = true;
                    // Drain once on the next poll so `CheckoutSweep` proves the post-EV_ADD git
                    // answers match the stored Pointer without rebuilding `KqueueVnode`.
                    initial_git_dirty = true;
                }
            }
        }

        // PHASE 3c: Watch `FACT_MOUNT` host inputs (`SystemVersion.plist` + `<exe_dir>/blaze-fact-shim`)
        // and verify all `FACT_MOUNT` leaves post-`EV_ADD` so warm no-ops hit `try_fast_certified`
        // without re-hashing the ~800 KB `blaze-fact-shim` on every request.
        let mut facts_watched = !saw_facts;
        let mut shim_absent_path: Option<AString<'static>> = None;
        if saw_facts {
            facts_watched = true;
            // 1. Watch `/System/Library/CoreServices/SystemVersion.plist`.
            let plist_path = blaze_build_script::fact_pins::SYSTEM_VERSION_PLIST;
            if let Some(cpath) = cpath_of(plist_path, &mut path_buf) {
                match openat(CWD, cpath, evtonly, 0) {
                    Ok(fd) if entries.len() < KQUEUE_CAP => {
                        let idx = entries.len();
                        if let Ok(ident) = usize::try_from(fd.fd().raw()) {
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
                            entries.push(WatchEntry {
                                _fd: fd,
                                kind: EntryKind::FactPath,
                                rel: AString::from_str_in(plist_path, heap),
                            });
                        } else {
                            facts_watched = false;
                        }
                    }
                    Err(Errno::MFile | Errno::NFile) => return None,
                    _ => facts_watched = false,
                }
            } else {
                facts_watched = false;
            }

            // 2. Watch `<exe_dir>/blaze-fact-shim` if any leaf depends on `SHIM_FILE`.
            if saw_shim && facts_watched {
                if let Some(dir) = exe_dir {
                    let shim_name = blaze_build_script::fact_shim::SHIM_NAME;
                    if let Some(cpath) = join_abs(dir, shim_name, &mut path_buf) {
                        match openat(CWD, cpath, evtonly, 0) {
                            Ok(fd) if entries.len() < KQUEUE_CAP => {
                                let idx = entries.len();
                                if let Ok(ident) = usize::try_from(fd.fd().raw()) {
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
                                    if fd_matches_path(fd.fd(), cpath).is_none() {
                                        initial_facts_dirty = true;
                                        initial_needs_refresh = true;
                                    }
                                    entries.push(WatchEntry {
                                        _fd: fd,
                                        kind: EntryKind::FactPath,
                                        rel: AString::from_str_in(shim_name, heap),
                                    });
                                } else {
                                    facts_watched = false;
                                }
                            }
                            Err(Errno::NoEnt | Errno::NotDir) => {
                                let mut abs = AString::from_str_in(dir.trim_end_matches('/'), heap);
                                abs.push_str("/");
                                abs.push_str(shim_name);
                                shim_absent_path = Some(abs);
                            }
                            Err(Errno::MFile | Errno::NFile) => return None,
                            _ => facts_watched = false,
                        }
                    } else {
                        facts_watched = false;
                    }
                } else {
                    facts_watched = false;
                }
            }

            // 3. Zero-TOCTOU post-EV_ADD verification: every `FACT_MOUNT` leaf must match today's value!
            if facts_watched && !crate::sweep::verify_fact_leaves(leaves, exe_dir, frame) {
                initial_facts_dirty = true;
            }
        }

        Some(KqueueVnode {
            kq,
            entries,
            leaf_paths,
            orphan_leaves,
            root_symlink_dirs,
            initial_dirty,
            initial_needs_refresh,
            initial_home_dirty,
            initial_git_dirty,
            initial_facts_dirty,
            initial_full_rescan,
            facts_watched,
            git_watched,
            shim_absent_path,
        })
    }

    /// Post-`EV_ADD` verification for checkout root (`""`) leaves (`Listing`, `ScriptStat`,
    /// `ScriptFile`, `ScriptListing`, which always store `row.stamp = None`): if EVERY root leaf
    /// still matches today's value, clears `""` from `initial_dirty` so even the very first warm
    /// request after arming can hit `PollOutcome::Clean`.
    pub(crate) fn verify_root_listing(&mut self, cx: &Cx, root: &Dir<ReadWrite>, leaves: &Leaves<'_>, frame: &Frame) {
        if !self.initial_dirty.iter().any(|p| p.as_str().is_empty()) {
            return;
        }
        let mut saw_root = false;
        for i in 0..leaves.len() {
            let row = leaves.row(i);
            if !leaves.path(i).is_empty() {
                continue;
            }
            match row.kind {
                LeafKind::Listing => {
                    if row.flags & ABSENT != 0 {
                        return;
                    }
                    let Ok(entries) = crate::sweep::list_dir(cx, root, "", frame) else {
                        return;
                    };
                    let (flags, id) = crate::sweep::listing_id(entries.as_ref().map(crate::sweep::Listing::entries));
                    if flags != 0 || id != row.id {
                        return;
                    }
                    saw_root = true;
                }
                LeafKind::ScriptStat => {
                    if row.id != blaze_proc_macro::script_sandbox::stat_answer(true, 0) {
                        return;
                    }
                    saw_root = true;
                }
                LeafKind::ScriptFile => {
                    if row.id != crate::engine_script_reads::NOT_A_FILE {
                        return;
                    }
                    saw_root = true;
                }
                LeafKind::ScriptListing => {
                    let Ok(id) = crate::engine_script_reads::root_listing_id(cx, root, frame) else {
                        return;
                    };
                    if id != row.id {
                        return;
                    }
                    saw_root = true;
                }
                _ => return,
            }
        }
        if saw_root {
            self.initial_dirty.retain(|p| !p.as_str().is_empty());
        }
    }

    /// Number of open `O_EVTONLY` vnode descriptors currently watched.
    #[cfg(test)]
    pub(crate) fn watched_count(&self) -> usize {
        self.entries.len()
    }

    /// Drains all pending `EVFILT_VNODE` events with a single non-blocking `kevent(0)` loop
    /// (~10 µs when clean) and returns exact dirty checkout-relative paths.
    pub(crate) fn poll_dirty<'f>(&mut self, frame: &'f Frame) -> KqueuePoll<'f> {
        use blaze_sys::Errno;
        use blaze_sys::Timespec;
        use blaze_sys::posix::io::{CWD, Filter, Kevent, KeventFlags, NoteFlags, kevent};
        use blaze_sys::posix::proc::{AT_SYMLINK_NOFOLLOW, stat_at};

        let mut paths: Vec<&'f str, &'f Frame> = Vec::new_in(frame);
        let mut needs_refresh = core::mem::take(&mut self.initial_needs_refresh);
        let mut home_dirty = core::mem::take(&mut self.initial_home_dirty);
        let mut git_dirty = core::mem::take(&mut self.initial_git_dirty);
        let mut facts_dirty = core::mem::take(&mut self.initial_facts_dirty);
        let mut full_rescan = core::mem::take(&mut self.initial_full_rescan);

        for p in self.initial_dirty.drain(..) {
            paths.push(frame.alloc_str(p.as_str()));
        }

        // If `blaze-fact-shim` was absent at arm time, verify it is still absent in 1 stat (~1 µs).
        if let Some(shim_abs) = self.shim_absent_path.as_ref() {
            let mut path_buf = [0u8; 4096];
            let still_absent = cpath_of(shim_abs.as_str(), &mut path_buf)
                .is_some_and(|cpath| matches!(stat_at(CWD, cpath, AT_SYMLINK_NOFOLLOW), Err(Errno::NoEnt | Errno::NotDir)));
            if !still_absent {
                facts_dirty = true;
                needs_refresh = true;
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

        // Bounded drain loop: each iteration drains up to 256 events; capped at 256 iterations
        // (65,536 events, well above KQUEUE_CAP) to guarantee bounded progress (rule 18). If all
        // iterations fill `buf`, fall back to a full rescan so no pending event is missed.
        let mut drained = false;
        for _ in 0..256 {
            let n = match kevent(self.kq.fd(), &[], &mut buf, Some(Timespec::ZERO)) {
                Ok(0) => {
                    drained = true;
                    break;
                }
                Ok(n) => n,
                Err(_) => {
                    full_rescan = true;
                    drained = true;
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
                        needs_refresh = true;
                    }
                    EntryKind::FactPath => {
                        facts_dirty = true;
                        needs_refresh = true;
                    }
                }
            }
            if n < buf.len() {
                drained = true;
                break;
            }
        }
        if !drained {
            full_rescan = true;
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
                    // Root-level file or root-level `MODULE` slot (`foo.rs` / `foo/mod.rs`), or
                    // any leaf beneath a root-level symlinked directory (whose atomic retarget
                    // only fires `NOTE_WRITE` on the checkout root).
                    let is_root_child = !rel.contains('/')
                        || rel.strip_suffix("/mod.rs").is_some_and(|stem| !stem.contains('/'));
                    let under_root_symlink = self.root_symlink_dirs.iter().any(|sym| {
                        let s = sym.as_str();
                        rel == s || rel.strip_prefix(s).is_some_and(|rest| rest.starts_with('/'))
                    });
                    if is_root_child || under_root_symlink {
                        paths.push(frame.alloc_str(rel));
                    }
                }
                for sym in self.root_symlink_dirs.iter() {
                    paths.push(frame.alloc_str(sym.as_str()));
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
            facts_dirty,
            full_rescan,
        }
    }
}
