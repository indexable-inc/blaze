//! The read checks the warm launcher keeps ([`super::serve`]): a path seen covered for the
//! sandbox's readers (the container's SID and ALL APPLICATION PACKAGES, with [`READ`]) is not
//! checked again until a watch of its volume says it may have changed, so a run whose reads were
//! all granted before makes no call for them. `blaze sandbox --v2` keeps none: it runs once.
//!
//! What a path's coverage is a function of is its own DACL, inherited entries included (Windows
//! copies them into it), and which object the path names. So a kept check is let go when its
//! volume's watch (`ReadDirectoryChangesW` over the whole volume, with
//! `FILE_NOTIFY_CHANGE_SECURITY`, `_FILE_NAME` and `_DIR_NAME`) says:
//! - the path's security changed (a propagated change to a directory above it rewrites its DACL,
//!   and is told for it as well);
//! - the path, or a directory above it, was removed or renamed from or to, which is the only way
//!   another object comes to be at the path;
//! - the watch lost changes (its buffer overflowed): every check of the volume is let go;
//! - the watch ended: the same, and the volume's paths are checked every run from then on, which
//!   is said on stderr.
//!
//! Only a path named as the file system names it is kept: through no link, junction, short name
//! or `.` (its final path, `GetFinalPathNameByHandleW`, is the path itself, ASCII case aside), and
//! on the volume its watch is on (`GetVolumePathNameW`), so what a watch tells is under the name
//! the check is kept by.
//!
//! A check is kept as pending before the DACL is read, and published only if it is still pending
//! after: a change the watch tells in between lets go of the pending check, so what is kept is
//! never older than a change told. A kept check that is wrong for a moment (a change not yet
//! told) can only leave out a grant, which makes the program's read fail; it never widens what
//! the sandbox allows, which is the DACL's to say.
//!
//! The table is open-addressed and append-only: a path's key is published once, with one release
//! store, and never moved or freed while the launcher runs; a check's state is one word, so a run
//! looking a path up does plain loads, takes no lock and writes nothing shared.

use std::{
    hash::BuildHasher as _,
    os::windows::ffi::OsStrExt as _,
    path::Path,
    ptr::{null, null_mut},
    sync::OnceLock,
};

use parking_lot::Mutex;

// The table's atomics are the model checker's in a model build (`--cfg blaze_check`), so its
// model tests explore the same lines a launcher runs.
#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use blaze_hash::FixedState;
#[cfg(not(blaze_check))]
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use windows_sys::Win32::{
    Foundation::{ERROR_NOTIFY_ENUM_DIR, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        CreateFileW, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED,
        FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_SECURITY,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFinalPathNameByHandleW,
        GetVolumePathNameW, OPEN_EXISTING, ReadDirectoryChangesW, VOLUME_NAME_DOS,
    },
    System::{
        IO::{GetOverlappedResult, OVERLAPPED},
        Threading::CreateEventW,
    },
};

use super::{GRANTING_READ, Handle, PSID, READ, Seen, failed, inspect, open, wide};

/// The distinct paths, and separately the directories above them, one launcher keeps: a path
/// past it is checked every run.
const SLOTS: usize = 1 << 16;

/// The bytes of changes one read of a watch takes, `u32`s for the 4-byte alignment of its
/// records: 64 KiB, the most a watch of a network volume takes.
const CHANGES: usize = (64 << 10) / 4;

/// What a watch is told of.
const TOLD: u32 = FILE_NOTIFY_CHANGE_SECURITY | FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME;

/// A check's state: 0 not known; [`COVERED`], and [`DIR`] for a directory; or pending, a token
/// shifted past both bits.
const COVERED: u64 = 0b10;
const DIR: u64 = 0b01;
const PENDING: u32 = 2;

const BACKSLASH: u16 = b'\\' as u16;

/// `\\?\`, which a final path starts with.
const VERBATIM: [u16; 4] = [BACKSLASH, BACKSLASH, b'?' as u16, BACKSLASH];

static KEPT: OnceLock<Reads> = OnceLock::new();

/// The checks the warm launcher keeps, once [`keep`] made them; none in a launcher of one run.
pub(super) fn kept() -> Option<&'static Reads> {
    KEPT.get()
}

/// Makes the process keep its read checks: the warm launcher, as it starts.
pub(super) fn keep() -> &'static Reads {
    KEPT.get_or_init(Reads::new)
}

pub(super) struct Reads {
    paths: Table,
    /// Every directory between a kept path and its volume's root, so a removal or rename that
    /// names none of them is let go of with two lookups.
    above: Table,
    /// The volumes watched, or whose watch ended; touched only by a check not kept.
    roots: Mutex<Vec<&'static Root>>,
    tokens: AtomicU64,
}

/// A volume's root, by its key (with its last `\`), and whether its watch ended.
struct Root {
    key: Box<[u16]>,
    dead: AtomicBool,
}

impl Reads {
    fn new() -> Reads {
        Reads {
            paths: Table::new(SLOTS),
            above: Table::new(SLOTS),
            // alloc: once per launcher; a volume is pushed once.
            roots: Mutex::new(Vec::with_capacity(8)),
            tokens: AtomicU64::new(1),
        }
    }

    /// What [`super::covered`] says of `path` for `readers`, the container's SID and ALL
    /// APPLICATION PACKAGES', with [`READ`]: kept when it was seen covered before and nothing
    /// told since. `key` is the caller's buffer for the path's key.
    pub(super) fn covered(&'static self, path: &Path, readers: &[PSID; 2], key: &mut Vec<u16>) -> Result<Option<Seen>, String> {
        fold_into(key, path.as_os_str().encode_wide());
        let hash = hash(key);
        if let Some(slot) = self.paths.find(hash, key) {
            let state = slot.state.load(Ordering::Acquire);
            if state & COVERED != 0 {
                return Ok(Some(Seen { dir: state & DIR != 0, covered: true }));
            }
        }
        // Not there: skipped as `covered` skips it, and looked for again next run.
        let Some(file) = open(path, GRANTING_READ)? else { return Ok(None) };
        let pending = self.pending(&file, path, key, hash);
        let seen = inspect(&file, path, readers, READ, GRANTING_READ)?;
        if let Some((slot, token, root)) = pending {
            let to = match seen.covered {
                true => COVERED | u64::from(seen.dir),
                false => 0,
            };
            slot.publish(token, to, &root.dead);
        }
        Ok(Some(seen))
    }

    /// The check of `path` (`key`) made pending, before its DACL is read, when it can be kept:
    /// its slot, its token and its volume, whose watch is on.
    fn pending(&'static self, file: &Handle, path: &Path, key: &[u16], hash: u64) -> Option<(&'static Slot, u64, &'static Root)> {
        if !named_as_it_is(file, key) {
            return None;
        }
        let root = self.root(path)?;
        if key.len() <= root.key.len() || !key.starts_with(&root.key) {
            return None;
        }
        let slot = self.paths.claim(hash, key)?;
        for (at, unit) in key.iter().enumerate().skip(root.key.len()) {
            if *unit == BACKSLASH {
                let above = &key[..at];
                self.above.claim(self::hash(above), above)?;
            }
        }
        let token = self.tokens.fetch_add(1, Ordering::Relaxed) << PENDING;
        if !slot.pend(token, &root.dead) {
            return None;
        }
        Some((slot, token, root))
    }

    /// The volume `path` is on, its watch made the first time; `None` where it can't be watched,
    /// or its watch ended.
    fn root(&'static self, path: &Path) -> Option<&'static Root> {
        let name = wide(path.as_os_str(), GRANTING_READ).ok()?;
        // alloc: a check not kept: the volume's path, no longer than the path on it.
        let mut volume = vec![0u16; name.len() + 1];
        // SAFETY: `name` ends in NUL (`wide`) and is only read; `volume` is `len` units, the
        // length the call is told, live and exclusive for it, which it writes no further than.
        let found = unsafe { GetVolumePathNameW(name.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) };
        if found == 0 {
            return None;
        }
        let end = volume.iter().position(|it| *it == 0)?;
        volume.truncate(end + 1);
        // alloc: as above.
        let mut key = Vec::with_capacity(end);
        fold_into(&mut key, volume[..end].iter().copied());
        if key.last() != Some(&BACKSLASH) {
            key.push(BACKSLASH);
        }
        let mut roots = self.roots.lock();
        if let Some(root) = roots.iter().find(|it| *it.key == *key) {
            return (!root.dead.load(Ordering::SeqCst)).then_some(*root);
        }
        // alloc: once per volume, which lives as long as the launcher.
        let root: &'static Root = Box::leak(Box::new(Root { key: key.into(), dead: AtomicBool::new(false) }));
        roots.push(root);
        let watched = Watch::start(&volume).and_then(|watch| {
            std::thread::Builder::new()
                // alloc: once per volume.
                .name(format!("blaze-sandbox-watch-{}", roots.len()))
                .spawn(move || follow(watch, root, self))
                // alloc: the error, once per volume.
                .map_err(|error| format!("starting its thread: {error}"))
        });
        match watched {
            Ok(_) => Some(root),
            Err(why) => {
                root.dead.store(true, Ordering::SeqCst);
                eprintln!("blaze sandbox: {} is not watched ({why}): its paths are checked every run", shown(&root.key));
                None
            }
        }
    }

    /// What `changes` (`got` bytes of FILE_NOTIFY_INFORMATION records, the watch of `root`'s)
    /// let go of. Records it can't read are read as changes lost.
    fn told(&self, root: &Root, changes: &[u32], got: usize, key: &mut Vec<u16>) {
        let mut at = 0;
        loop {
            let word = |offset: usize| changes.get(at / 4 + offset).copied();
            let (Some(next), Some(action), Some(bytes)) = (word(0), word(1), word(2)) else {
                return self.forget_under(&root.key);
            };
            let first = (at + 12) / 2;
            if at + 12 + bytes as usize > got {
                return self.forget_under(&root.key);
            }
            key.clear();
            key.extend_from_slice(&root.key);
            for unit in first..first + bytes as usize / 2 {
                // Windows is little-endian: the unit at an even place is the word's low half.
                let Some(word) = changes.get(unit / 2) else { return self.forget_under(&root.key) };
                let unit = if unit % 2 == 0 { *word as u16 } else { (*word >> 16) as u16 };
                key.push(fold(unit));
            }
            self.forget(key, action);
            if next == 0 {
                return;
            }
            at += next as usize;
            if at % 4 != 0 || at >= got {
                return self.forget_under(&root.key);
            }
        }
    }

    /// Lets go of `key`'s check, and of every check below it when `action` removed or renamed it.
    fn forget(&self, key: &[u16], action: u32) {
        let hash = hash(key);
        if let Some(slot) = self.paths.find(hash, key) {
            slot.forget();
        }
        let moved = matches!(action, FILE_ACTION_REMOVED | FILE_ACTION_RENAMED_OLD_NAME | FILE_ACTION_RENAMED_NEW_NAME);
        if moved && self.above.find(hash, key).is_some() {
            self.forget_under(key);
        }
    }

    /// Lets go of every check of a path below `dir` (a key, a volume's with its last `\`).
    fn forget_under(&self, dir: &[u16]) {
        self.paths.each(|path, slot| {
            let below = match dir.last() {
                Some(&BACKSLASH) => path.starts_with(dir),
                _ => path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == BACKSLASH,
            };
            if below {
                slot.forget();
            }
        });
    }

    /// `root`'s watch ended, for `why`: what it held is let go of, and its paths are checked
    /// every run from now on.
    fn lose(&self, root: &Root, why: &str) {
        root.dead.store(true, Ordering::SeqCst);
        self.forget_under(&root.key);
        eprintln!("blaze sandbox: the watch of {} ended ({why}): its paths are checked every run", shown(&root.key));
    }
}

/// `path`'s units into `key`: `/` as `\`, ASCII letters upper-cased.
fn fold_into(key: &mut Vec<u16>, path: impl Iterator<Item = u16>) {
    key.clear();
    key.extend(path.map(fold));
}

fn fold(unit: u16) -> u16 {
    match unit {
        0x61..=0x7a => unit - 0x20,
        0x2f => BACKSLASH,
        _ => unit,
    }
}

fn hash(key: &[u16]) -> u64 {
    FixedState.hash_one(key)
}

fn shown(key: &[u16]) -> String {
    // alloc: an error path, said once.
    String::from_utf16_lossy(key)
}

/// Whether what `file` has open is named `key` by the file system itself: its final path, which
/// resolves every link, junction, short name and `.`, is `\\?\` and `key`, folded alike.
fn named_as_it_is(file: &Handle, key: &[u16]) -> bool {
    // alloc: a check not kept: the final path, its length about the given one's.
    let mut name = vec![0u16; key.len() + VERBATIM.len() + 2];
    for _ in 0..2 {
        // SAFETY: `file` owns an open handle (`open`); `name` is `len` units, the length the
        // call is told, live and exclusive for it, which it writes no further than.
        let wrote = unsafe {
            GetFinalPathNameByHandleW(file.0, name.as_mut_ptr(), name.len() as u32, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS)
        } as usize;
        if wrote == 0 {
            return false;
        }
        // Too short: `wrote` is the length it needs, with its NUL.
        if wrote >= name.len() {
            name.resize(wrote + 1, 0);
            continue;
        }
        let final_path = &name[..wrote];
        let Some(rest) = final_path.strip_prefix(&VERBATIM[..]) else { return false };
        return rest.len() == key.len() && rest.iter().zip(key).all(|(it, want)| fold(*it) == *want);
    }
    false
}

/// An open-addressed table of keys, each claimed once and never moved, changed or freed while the
/// table is, with a state word beside it.
struct Table {
    slots: Box<[Slot]>,
}

struct Slot {
    key: AtomicPtr<Key>,
    state: AtomicU64,
}

impl Slot {
    /// Makes the check pending with `token`, before its DACL is read: whether it still may be
    /// kept, which it may not once the watch of its volume ended (`dead`).
    fn pend(&self, token: u64, dead: &AtomicBool) -> bool {
        self.state.store(token, Ordering::SeqCst);
        !dead.load(Ordering::SeqCst)
    }

    /// Publishes `to` as the check pending with `token`, if nothing told let go of it since
    /// [`Slot::pend`]; and lets go of it again if the watch ended (`dead`) meanwhile, maybe
    /// after its own letting go of everything ran.
    fn publish(&self, token: u64, to: u64, dead: &AtomicBool) {
        let published = self.state.compare_exchange(token, to, Ordering::SeqCst, Ordering::SeqCst).is_ok();
        if published && dead.load(Ordering::SeqCst) {
            self.forget();
        }
    }

    /// Lets go of the check: a change was told, or the watch ended.
    fn forget(&self) {
        self.state.store(0, Ordering::SeqCst);
    }
}

struct Key {
    hash: u64,
    units: Box<[u16]>,
}

impl Table {
    fn new(capacity: usize) -> Table {
        assert!(capacity.is_power_of_two());
        // alloc: once per launcher, the table its checks are kept in.
        let slots = (0..capacity).map(|_| Slot { key: AtomicPtr::new(null_mut()), state: AtomicU64::new(0) }).collect();
        Table { slots }
    }

    /// The slot of `units`, whose hash is `hash`, where it was claimed.
    fn find(&self, hash: u64, units: &[u16]) -> Option<&Slot> {
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        for _ in 0..self.slots.len() {
            let slot = &self.slots[at];
            let key = slot.key.load(Ordering::Acquire);
            if key.is_null() {
                return None;
            }
            // SAFETY: a key that is not null was published by `claim`'s compare-exchange (release on success) from a
            // `Box` it made whole first, and is never changed or freed while the table lives
            // (only `drop`, with `&mut self`, frees it), so this shared borrow of it is sound.
            let key = unsafe { &*key };
            if key.hash == hash && *key.units == *units {
                return Some(slot);
            }
            at = (at + 1) & mask;
        }
        None
    }

    /// The slot of `units`, claimed by this call or an earlier one; `None` when the table is
    /// full.
    fn claim(&self, hash: u64, units: &[u16]) -> Option<&Slot> {
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        let mut ours: *mut Key = null_mut();
        for _ in 0..self.slots.len() {
            let slot = &self.slots[at];
            let mut key = slot.key.load(Ordering::Acquire);
            if key.is_null() {
                if ours.is_null() {
                    // alloc: once per path kept, which stays as long as the launcher.
                    ours = Box::into_raw(Box::new(Key { hash, units: units.into() }));
                }
                match slot.key.compare_exchange(null_mut(), ours, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => return Some(slot),
                    Err(theirs) => key = theirs,
                }
            }
            // SAFETY: as in `find`: `key` is not null here, published whole and never freed
            // while the table lives.
            let theirs = unsafe { &*key };
            if theirs.hash == hash && *theirs.units == *units {
                free_unpublished(ours);
                return Some(slot);
            }
            at = (at + 1) & mask;
        }
        free_unpublished(ours);
        None
    }

    /// `each` of every key claimed, with its slot.
    fn each(&self, mut each: impl FnMut(&[u16], &Slot)) {
        for slot in &self.slots {
            let key = slot.key.load(Ordering::Acquire);
            if key.is_null() {
                continue;
            }
            // SAFETY: as in `find`.
            let key = unsafe { &*key };
            each(&key.units, slot);
        }
    }
}

/// Frees a key `claim` made and did not publish.
fn free_unpublished(key: *mut Key) {
    if !key.is_null() {
        // SAFETY: `key` came from `Box::into_raw` in `claim`, and every swap that could have
        // published it failed, so no slot and no reader has it: this is its one owner.
        drop(unsafe { Box::from_raw(key) });
    }
}

impl Drop for Table {
    fn drop(&mut self) {
        for slot in self.slots.iter_mut() {
            let key = *slot.key.get_mut();
            if !key.is_null() {
                // SAFETY: a published key came from `Box::into_raw` in `claim` and is in this one
                // slot; `&mut self` means no reader holds it, and the table frees it once.
                drop(unsafe { Box::from_raw(key) });
            }
        }
    }
}

/// A watch of a whole volume: its root open for listing, the read of changes in flight, and its
/// event. The read writes into `changes` and `overlapped` until it completes, so both are on the
/// heap, where moving the `Watch` leaves them, and the `Watch` is dropped only by its thread once
/// no read is in flight.
struct Watch {
    dir: Handle,
    /// The event the read in flight sets, open while `overlapped` names it.
    _event: Handle,
    overlapped: Box<OVERLAPPED>,
    changes: Box<[u32]>,
}

// SAFETY: a `Watch`'s handles, its OVERLAPPED and its buffer are its own, shared with no other
// value: it is made on one thread and moved, whole, to the one thread that follows it.
unsafe impl Send for Watch {}

impl Watch {
    /// A watch of the volume whose root is `volume` (ending in NUL), its first read asked for:
    /// what changes from here on is told.
    fn start(volume: &[u16]) -> Result<Watch, String> {
        // SAFETY: `volume` ends in NUL (`Reads::root`) and is only read; null security and
        // template are allowed. The handle it gives, or INVALID_HANDLE_VALUE, is the `Handle`'s.
        let dir = Handle(unsafe {
            CreateFileW(
                volume.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        });
        if dir.0 == INVALID_HANDLE_VALUE {
            return Err(failed("opening it", "CreateFileW"));
        }
        // SAFETY: null security and name are allowed; a manual-reset event, not set. The handle
        // it gives, or null, is the `Handle`'s.
        let event = Handle(unsafe { CreateEventW(null(), 1, 0, null()) });
        if event.0.is_null() {
            return Err(failed("making its event", "CreateEventW"));
        }
        // alloc: once per volume, where the read in flight writes.
        let mut overlapped = Box::new(OVERLAPPED::default());
        overlapped.hEvent = event.0;
        // alloc: as above.
        let changes = vec![0u32; CHANGES].into_boxed_slice();
        let mut watch = Watch { dir, _event: event, overlapped, changes };
        watch.ask()?;
        Ok(watch)
    }

    /// Asks for the next changes; the kernel keeps what changes until then.
    fn ask(&mut self) -> Result<(), String> {
        // SAFETY: `dir` is open with FILE_LIST_DIRECTORY and FILE_FLAG_OVERLAPPED; `changes` is
        // `CHANGES` 4-byte aligned words, the byte length the call is told, and `overlapped` has
        // the watch's event; both are on the heap and stay, unread, until `GetOverlappedResult`
        // says the read completed (`follow`). A null byte count is allowed for an overlapped call.
        let asked = unsafe {
            ReadDirectoryChangesW(
                self.dir.0,
                self.changes.as_mut_ptr().cast(),
                (CHANGES * 4) as u32,
                1,
                TOLD,
                null_mut(),
                &mut *self.overlapped,
                None,
            )
        };
        match asked {
            0 => Err(failed("asking for its changes", "ReadDirectoryChangesW")),
            _ => Ok(()),
        }
    }
}

/// The thread of `root`'s watch: what it is told let go of, until it ends.
fn follow(mut watch: Watch, root: &'static Root, reads: &'static Reads) {
    // alloc: once per volume, the key of each change told.
    let mut key = Vec::with_capacity(512);
    loop {
        let mut got = 0u32;
        // SAFETY: `overlapped` is the one of the read `ask` asked for on `dir`, on the heap and
        // live; `got` is a live `u32` for the call to set. It waits for the read to complete,
        // after which the kernel writes no more into `changes`.
        let done = unsafe { GetOverlappedResult(watch.dir.0, &*watch.overlapped, &mut got, 1) };
        if done == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_NOTIFY_ENUM_DIR as i32) {
                // alloc: an error path, once per volume.
                return reads.lose(root, &format!("GetOverlappedResult: {error}"));
            }
            reads.forget_under(&root.key);
        } else if got == 0 {
            // More changed than the buffer holds: what changed is not known.
            reads.forget_under(&root.key);
        } else {
            reads.told(root, &watch.changes, got as usize, &mut key);
        }
        if let Err(why) = watch.ask() {
            return reads.lose(root, &why);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    /// A table of its own, as the launcher's static is, so a test's checks are its own.
    fn reads() -> &'static Reads {
        Box::leak(Box::new(Reads::new()))
    }

    fn key(text: &str) -> Vec<u16> {
        let mut key = Vec::new();
        fold_into(&mut key, OsStr::new(text).encode_wide());
        key
    }

    /// `names` as the records of one read of a watch: each name and its action.
    fn records(names: &[(&str, u32)]) -> (Vec<u32>, usize) {
        let mut bytes = Vec::new();
        for (at, (name, action)) in names.iter().enumerate() {
            let units: Vec<u16> = name.encode_utf16().collect();
            let len = 12 + units.len() * 2;
            let next = if at + 1 == names.len() { 0 } else { len.div_ceil(4) * 4 };
            bytes.extend((next as u32).to_le_bytes());
            bytes.extend(action.to_le_bytes());
            bytes.extend(((units.len() * 2) as u32).to_le_bytes());
            bytes.extend(units.iter().flat_map(|it| it.to_le_bytes()));
            if next != 0 {
                bytes.resize(bytes.len().div_ceil(4) * 4, 0);
            }
        }
        let got = bytes.len();
        bytes.resize(bytes.len().div_ceil(4) * 4, 0);
        (bytes.chunks(4).map(|it| u32::from_le_bytes(it.try_into().unwrap())).collect(), got)
    }

    /// `path` kept as covered, as a check that found it so publishes it.
    fn kept_covered(reads: &Reads, path: &str, root: usize) {
        let key = key(path);
        let slot = reads.paths.claim(hash(&key), &key).unwrap();
        for (at, unit) in key.iter().enumerate().skip(root) {
            if *unit == BACKSLASH {
                reads.above.claim(hash(&key[..at]), &key[..at]).unwrap();
            }
        }
        slot.state.store(COVERED | DIR, Ordering::SeqCst);
    }

    fn is_kept(reads: &Reads, path: &str) -> bool {
        let key = key(path);
        reads.paths.find(hash(&key), &key).is_some_and(|it| it.state.load(Ordering::SeqCst) & COVERED != 0)
    }

    #[test]
    fn a_key_is_claimed_once_and_found_by_any_case_and_slash() {
        let table = Table::new(8);
        let (a, b) = (key(r"C:\Src\a"), key("c:/src/A"));
        assert_eq!(a, b);
        let first = table.claim(hash(&a), &a).unwrap() as *const Slot;
        assert_eq!(table.claim(hash(&b), &b).unwrap() as *const Slot, first);
        assert_eq!(table.find(hash(&a), &a).unwrap() as *const Slot, first);
        assert!(table.find(hash(&key(r"C:\src\b")), &key(r"C:\src\b")).is_none());
        // Every slot taken, by keys that collide on purpose: the next is refused, and each found.
        for n in 0..7 {
            let other = key(&format!(r"C:\{n}"));
            assert!(table.claim(0, &other).is_some());
        }
        assert!(table.claim(0, &key(r"C:\full")).is_none());
        for n in 0..7 {
            let other = key(&format!(r"C:\{n}"));
            assert!(table.find(0, &other).is_some());
        }
    }

    #[test]
    fn a_change_told_lets_go_of_the_path_and_a_move_of_what_is_above_it() {
        let reads = reads();
        let root = Root { key: key(r"C:\").into(), dead: AtomicBool::new(false) };
        for path in [r"C:\ws\src", r"C:\ws\src\lib", r"C:\ws\other", r"C:\elsewhere"] {
            kept_covered(reads, path, 3);
        }
        let mut buffer = Vec::new();
        // A security change of `C:\ws` is its own: what is below it is told for itself.
        let (changes, got) = records(&[(r"ws", 3), (r"tmp\x", FILE_ACTION_REMOVED)]);
        reads.told(&root, &changes, got, &mut buffer);
        assert!(is_kept(reads, r"C:\ws\src") && is_kept(reads, r"C:\ws\other"));
        let (changes, got) = records(&[(r"WS\SRC\LIB", 3)]);
        reads.told(&root, &changes, got, &mut buffer);
        assert!(!is_kept(reads, r"C:\ws\src\lib") && is_kept(reads, r"C:\ws\src"));
        // `src` renamed away: it and what is below it go, its sibling stays.
        kept_covered(reads, r"C:\ws\src\lib", 3);
        let (changes, got) = records(&[(r"ws\src", FILE_ACTION_RENAMED_OLD_NAME), (r"ws\gone", FILE_ACTION_RENAMED_NEW_NAME)]);
        reads.told(&root, &changes, got, &mut buffer);
        assert!(!is_kept(reads, r"C:\ws\src") && !is_kept(reads, r"C:\ws\src\lib"));
        assert!(is_kept(reads, r"C:\ws\other") && is_kept(reads, r"C:\elsewhere"));
        // `C:\ws\o` is no directory above `C:\ws\other`.
        let (changes, got) = records(&[(r"ws\o", FILE_ACTION_REMOVED)]);
        reads.told(&root, &changes, got, &mut buffer);
        assert!(is_kept(reads, r"C:\ws\other"));
        // Records that run past what was read: every check of the volume goes.
        let (changes, got) = records(&[(r"unrelated", 3)]);
        reads.told(&root, &changes, got - 2, &mut buffer);
        assert!(!is_kept(reads, r"C:\ws\other") && !is_kept(reads, r"C:\elsewhere"));
    }

    #[test]
    fn a_watch_that_ends_lets_go_of_its_volume_alone() {
        let reads = reads();
        let root = Root { key: key(r"C:\").into(), dead: AtomicBool::new(false) };
        kept_covered(reads, r"C:\a", 3);
        kept_covered(reads, r"D:\b", 3);
        reads.lose(&root, "test");
        assert!(root.dead.load(Ordering::SeqCst));
        assert!(!is_kept(reads, r"C:\a") && is_kept(reads, r"D:\b"));
    }

    /// The kept check against the one `covered` makes each run, on a real directory: the same
    /// answer, and a second look at a covered path makes no call (it is kept), until a change to
    /// its DACL is told.
    #[test]
    fn a_kept_check_is_the_check_and_goes_when_its_dacl_changes() {
        if let Err(why) = super::super::probe() {
            eprintln!("no sandbox can be made here, so this is not run: {why}");
            return;
        }
        let reads = reads();
        let base = std::env::temp_dir().join(format!("blaze-windows-sandbox-reads-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&base);
        let dir = base.join("d");
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // `canonicalize` gives `\\?\C:\...`; the launcher is given plain paths.
        let plain = std::path::PathBuf::from(dir.to_str().unwrap().trim_start_matches(r"\\?\"));
        let made = super::super::make(false, super::super::Given::own()).unwrap();
        let readers = [super::super::sid(&made.kept.container), super::super::sid(&made.kept.any)];
        let mut buffer = Vec::new();
        let want = super::super::covered(&plain, &readers, READ, "test").unwrap();
        assert_eq!(reads.covered(&plain, &readers, &mut buffer).unwrap(), want);
        {
            let _lock = super::super::Lock::take().unwrap();
            super::super::grant(&plain, true, readers[0], &readers, READ, "test").unwrap();
        }
        // The grant is a change to its DACL, told a moment later, which lets go of a check
        // published before it is: looked at again until one is kept past the change.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let seen = loop {
            let seen = reads.covered(&plain, &readers, &mut buffer).unwrap();
            assert_eq!(seen, Some(Seen { dir: true, covered: true }));
            std::thread::yield_now();
            if is_kept(reads, plain.to_str().unwrap()) {
                break seen;
            }
            assert!(std::time::Instant::now() < deadline, "a covered path is never kept");
        };
        assert_eq!(reads.covered(&plain, &readers, &mut buffer).unwrap(), seen);
        // Removed and made again: the kept check goes with it.
        std::fs::remove_dir_all(&dir).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while is_kept(reads, plain.to_str().unwrap()) && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(!is_kept(reads, plain.to_str().unwrap()), "a removal is told");
        assert_eq!(reads.covered(&plain, &readers, &mut buffer).unwrap(), None);
        _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_path_through_a_dot_is_not_kept_and_its_case_is_no_difference() {
        let base = std::env::temp_dir().join(format!("blaze-windows-sandbox-named-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("d")).unwrap();
        let base = std::path::PathBuf::from(std::fs::canonicalize(&base).unwrap().to_str().unwrap().trim_start_matches(r"\\?\"));
        let as_is = base.join("d");
        let file = super::super::open(&as_is, "test").unwrap().unwrap();
        assert!(named_as_it_is(&file, &key(as_is.to_str().unwrap())));
        assert!(named_as_it_is(&file, &key(&as_is.to_str().unwrap().to_lowercase())));
        let dotted = base.join(".").join("d");
        let file = super::super::open(&dotted, "test").unwrap().unwrap();
        assert!(!named_as_it_is(&file, &key(dotted.to_str().unwrap())));
        _ = std::fs::remove_dir_all(&base);
    }

    /// [`Table`] as a sequential open-addressed table: slots of `(hash, unit)` keys, each
    /// claimed in the first slot from its hash that is free or has it.
    #[cfg(blaze_check)]
    #[derive(Clone, PartialEq, Eq, Hash)]
    struct Slots(Vec<Option<(u64, u16)>>);

    #[cfg(blaze_check)]
    #[derive(Debug)]
    enum Op {
        Claim(u64, u16),
        Find(u64, u16),
    }

    #[cfg(blaze_check)]
    impl blaze_check::linear::Spec for Slots {
        type Op = Op;
        type Ret = Option<usize>;

        fn apply(&mut self, op: &Op) -> Option<usize> {
            let (Op::Claim(hash, unit) | Op::Find(hash, unit)) = *op;
            let mask = self.0.len() - 1;
            let mut at = hash as usize & mask;
            for _ in 0..self.0.len() {
                match self.0[at] {
                    None if matches!(op, Op::Claim(..)) => {
                        self.0[at] = Some((hash, unit));
                        return Some(at);
                    }
                    None => return None,
                    Some(key) if key == (hash, unit) => return Some(at),
                    Some(_) => at = (at + 1) & mask,
                }
            }
            None
        }
    }

    /// `op` on `table`, recorded in `history`: the place of the slot it gave.
    #[cfg(blaze_check)]
    fn run(table: &Table, history: &blaze_check::linear::History<Slots>, op: Op) -> Option<usize> {
        history.record(op, |op| {
            let (Op::Claim(hash, unit) | Op::Find(hash, unit)) = *op;
            let slot = match op {
                Op::Claim(..) => table.claim(hash, &[unit]),
                Op::Find(..) => table.find(hash, &[unit]),
            };
            slot.map(|slot| table.slots.iter().position(|it| std::ptr::eq(it, slot)).expect("a slot of the table"))
        })
    }

    /// Two threads claim keys whose hashes collide, one of them twice, while each also finds
    /// the other's: each key takes one slot, a claim of it again gives that slot, and a find
    /// gives it or nothing, as some order of the calls would on [`Slots`].
    #[cfg(blaze_check)]
    #[test]
    fn model_colliding_claims_take_a_slot_each_and_are_found_there() {
        blaze_check::model(|| {
            let table = std::sync::Arc::new(Table::new(4));
            let history = std::sync::Arc::new(blaze_check::linear::History::<Slots>::new());
            let other = {
                let (table, history) = (table.clone(), history.clone());
                blaze_check::thread::spawn(move || {
                    run(&table, &history, Op::Claim(1, 1));
                    run(&table, &history, Op::Find(1, 2));
                    run(&table, &history, Op::Claim(1, 1));
                })
            };
            run(&table, &history, Op::Claim(1, 2));
            run(&table, &history, Op::Find(1, 1));
            other.join().unwrap();
            history.check(Slots(vec![None; 4]));
        });
    }

    /// A check made pending as the watch of its volume ends (`lose`: the watch is marked dead,
    /// then every check of it let go) is never left kept, whichever runs first.
    #[cfg(blaze_check)]
    #[test]
    fn model_a_check_pending_as_its_watch_ends_is_not_kept() {
        blaze_check::model(|| {
            let slot = std::sync::Arc::new(Slot { key: AtomicPtr::new(null_mut()), state: AtomicU64::new(0) });
            let dead = std::sync::Arc::new(AtomicBool::new(false));
            let watch = {
                let (slot, dead) = (slot.clone(), dead.clone());
                blaze_check::thread::spawn(move || {
                    dead.store(true, Ordering::SeqCst);
                    slot.forget();
                })
            };
            let token = 1 << PENDING;
            if slot.pend(token, &dead) {
                slot.publish(token, COVERED | DIR, &dead);
            }
            watch.join().unwrap();
            assert_eq!(slot.state.load(Ordering::SeqCst) & COVERED, 0, "a check kept past the end of its watch");
        });
    }

    /// A change told between a check's pending and its publishing lets go of it: the check
    /// published is never one the change came after, so it is kept only when the change was
    /// told before the check was made pending.
    #[cfg(blaze_check)]
    #[test]
    fn model_a_change_told_while_a_check_is_pending_is_not_lost() {
        blaze_check::model(|| {
            let slot = std::sync::Arc::new(Slot { key: AtomicPtr::new(null_mut()), state: AtomicU64::new(0) });
            let dead = AtomicBool::new(false);
            let pended = std::sync::Arc::new(AtomicBool::new(false));
            let told = {
                let (slot, pended) = (slot.clone(), pended.clone());
                blaze_check::thread::spawn(move || {
                    let after = pended.load(Ordering::SeqCst);
                    slot.forget();
                    after
                })
            };
            let token = 1 << PENDING;
            assert!(slot.pend(token, &dead));
            pended.store(true, Ordering::SeqCst);
            slot.publish(token, COVERED, &dead);
            let after = told.join().unwrap();
            if after {
                assert_eq!(slot.state.load(Ordering::SeqCst), 0, "a change told after the check was pending was lost");
            }
        });
    }
}
