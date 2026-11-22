//! A walk over folders that is bounded by construction ([`Dir::walk`]): it replaces the
//! whole-depth listing (`Dir::list`), which answered with every file beneath a directory as
//! 72-byte `(Path, Meta)` pairs in the caller's frame. Called on a checkout with a build
//! folder (374,012 entries) that ran for seconds and then overflowed its frame ("arena
//! bstd::fs list: 72 more bytes would pass its limit"). Nothing in blaze's design walks a
//! checkout: the files a command reads are named by manifests and by `mod` lines, so a walk
//! here is always of a folder the caller chose, as deep as it chose, under limits it names.
//!
//! - The CALLER decides per folder which sub-folders to enter: the visitor is handed each
//!   folder's [`Entries`] (the driver's columns, moved whole: no row becomes a pair) and an
//!   [`Enter`] on which it marks the sub-folders to walk next. "Everything under `src/`" is
//!   `enter.enter_all()` with `src` as the start; "the whole checkout" has to be written as
//!   that on purpose, with limits that admit it.
//! - The limits ([`WalkLimits`]) have no default: the most folders listed, the most entries
//!   in all, the deepest level. Passing one stops the walk with [`WalkError::Limit`] naming
//!   the folder where it was passed and the limit; nothing partial is returned (what the
//!   visitor kept before the error is the caller's to drop), never a truncated answer, never
//!   a panic.
//! - The order is deterministic: depth first, pre-order, each folder's sub-folders in the
//!   names' byte order (the listing's own order). A folder is visited before anything under
//!   it, and everything under one sub-folder before the next sub-folder: the folders come in
//!   the order of their paths compared part by part (`a`, `a/x`, `a-b`: `a` < `a-b`, where
//!   the joined bytes would put `a-b` first), and a visitor that collects each folder's files
//!   as it is handed them has them ordered by (folder, name): a folder's own files before
//!   those of its sub-folders.
//! - No recursion: an explicit stack of the folders still to list, in the caller's frame.
//!   A tree 10,000 levels deep is walked in constant native stack.
//! - Symbolic links are not followed: [`Entries::kind`] reports one as
//!   [`EntryKind::Link`](super::EntryKind) (stamped itself, never its target), and asking to
//!   enter it is [`WalkError::NotAFolder`].
//! - One driver call per folder listed: an `Effect::Entries` of it, through the same `call`
//!   as [`Dir::entries`] (the cx's deadline and the call's span, as every file call). The
//!   start alone is first stat'ed when it is a named folder (it may not be there yet);
//!   a sub-folder is known to be a folder from its parent's listing and is listed at once.
//!   A folder gone between its parent's listing and its own fails naming it (not retried).

use core::fmt;

use blaze_core::DirName;
use blaze_mem::Frame;

use super::{Dir, Entries, EntryKind, FsError, FsKind, FsOp, Meta, Rights, stat};
use crate::{Stopped, cx::Cx, path::Path, path::PathError};

/// The bounds of one [`Dir::walk`], given by its caller: there is no default.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WalkLimits {
    /// The most folders listed, the start included.
    pub folders: u32,
    /// The most entries of all listed folders together (files, folders, links, the rest).
    pub entries: u64,
    /// The deepest level entered: the start is level 0, its sub-folders level 1.
    pub depth: u32,
}

/// Which limit a walk passed, with the value it was given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WalkLimit {
    Folders(u32),
    Entries(u64),
    Depth(u32),
}

impl fmt::Display for WalkLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalkLimit::Folders(most) => write!(f, "its limit of {most} folders"),
            WalkLimit::Entries(most) => write!(f, "its limit of {most} entries"),
            WalkLimit::Depth(most) => write!(f, "its limit of {most} levels deep"),
        }
    }
}

/// What a whole walk did: how many folders it listed and how many entries they held.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Walked {
    pub folders: u32,
    pub entries: u64,
}

/// How a walk stopped. Every folder is named under the driver's root, as [`FsError::path`]
/// is (the root shows as `.`).
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum WalkError<E> {
    /// A listing failed (the folder is in it), or the walk's frame had no room for its stack
    /// ([`FsKind::Stopped`] with [`Stopped::OutOfMemory`]).
    Fs(FsError),
    /// `limit` was passed at `folder`: the folder that would have been listed (folders), the
    /// folder whose listing passed the count (entries), or the sub-folder that would have
    /// been entered (depth).
    Limit { folder: DirName, limit: WalkLimit },
    /// The visitor asked to enter `entry`, which is no folder: a file, a symbolic link (never
    /// followed), or another kind.
    NotAFolder { entry: DirName, kind: EntryKind },
    /// The visitor asked to enter index `index` of `folder`, which has fewer entries.
    NoSuchEntry { folder: DirName, index: usize },
    /// An entry to enter has a name no [`Path`] holds (one with a `\`).
    Name(PathError),
    /// The visitor's own error: the walk stopped there.
    Visit(E),
}

/// The text of a folder's name: `.` for the driver's root.
fn shown(folder: &DirName) -> &str {
    folder.name().map_or(".", |name| name.as_str())
}

impl<E: fmt::Display> fmt::Display for WalkError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalkError::Fs(error) => write!(f, "a walk: {error}"),
            WalkError::Limit { folder, limit } => {
                write!(f, "{}: a walk passed {limit} here (rule bounded-progress: a walk is bounded by its caller)", shown(folder))
            }
            WalkError::NotAFolder { entry, kind } => {
                write!(f, "{}: a walk was asked to enter it and it is no folder ({kind:?}; links are never followed)", shown(entry))
            }
            WalkError::NoSuchEntry { folder, index } => write!(f, "{}: a walk was asked to enter entry {index}, which it has not", shown(folder)),
            WalkError::Name(error) => write!(f, "a walk: an entry's name is no path: {error:?}"),
            WalkError::Visit(error) => write!(f, "a walk's visitor: {error}"),
        }
    }
}

/// What the visitor of one folder marks to walk next: sub-folders by their index in that
/// folder's [`Entries`]. Nothing marked is nothing entered.
pub struct Enter<'s, 'f> {
    chosen: &'s mut blaze_own::Vec<usize, &'f Frame>,
    all: bool,
    full: bool,
}

impl Enter<'_, '_> {
    /// Enter entry `i` of this folder: it must be a folder ([`EntryKind::Dir`]), or the walk
    /// stops with [`WalkError::NotAFolder`] naming it. Marking one twice enters it once.
    pub fn enter(&mut self, i: usize) {
        if self.chosen.try_push(i).is_err() {
            self.full = true;
        }
    }

    /// Enter every sub-folder of this folder (each entry of kind [`EntryKind::Dir`]; links
    /// are not folders).
    pub fn enter_all(&mut self) {
        self.all = true;
    }
}

/// The planted bug of the walk's proof ([`Dir::walk_in`]): the walk enters every sub-folder,
/// the declined ones too. Only the test that must catch it plants it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WalkPlant {
    None,
    EntersDeclined,
}

impl<P: Rights> Dir<P> {
    /// What is directly in this directory itself, one level, as [`Dir::own_entries`]; `None`
    /// when it is not there yet (no file was made under it). The driver's root is always
    /// there. A file in its place fails naming it.
    pub fn own_entries_if_there(&self, cx: &Cx) -> Result<Option<Entries>, FsError> {
        if let DirName::Named(base) = &self.base {
            match stat(&self.io, cx, FsOp::Entries, base)? {
                None => return Ok(None),
                Some(Meta { dir: false, .. }) => return Err(FsError::new(FsOp::Entries, base, FsKind::NotADirectory)),
                Some(_) => {}
            }
        }
        // clone: the directory's name, a refcount.
        self.list_one(cx, self.base.clone(), None).map(Some)
    }

    /// A walk of the folders beneath `start` (this directory itself for `None`), as deep as
    /// `visit` enters, within `limits` (module docs). `visit` is handed each listed folder's
    /// [`Entries`] in depth-first pre-order, sub-folders in name order, and marks on its
    /// [`Enter`] which of that folder's sub-folders to walk; its error stops the walk as
    /// [`WalkError::Visit`]. A start that is not there is walked as nothing (zero folders); a
    /// file in its place fails naming it. The stack of folders still to list is in `frame`.
    pub fn walk<E>(
        &self,
        cx: &Cx,
        start: Option<&Path>,
        limits: WalkLimits,
        frame: &Frame,
        visit: impl FnMut(&Entries, &mut Enter<'_, '_>) -> Result<(), E>,
    ) -> Result<Walked, WalkError<E>> {
        self.walk_in(cx, start, limits, frame, WalkPlant::None, visit)
    }

    /// [`Dir::walk`], with the proof's planted bug when `plant` says so.
    pub(crate) fn walk_in<E>(
        &self,
        cx: &Cx,
        start: Option<&Path>,
        limits: WalkLimits,
        frame: &Frame,
        plant: WalkPlant,
        mut visit: impl FnMut(&Entries, &mut Enter<'_, '_>) -> Result<(), E>,
    ) -> Result<Walked, WalkError<E>> {
        let mut walked = Walked { folders: 0, entries: 0 };
        if limits.folders == 0 {
            return Err(WalkError::Limit { folder: self.folder_name(start), limit: WalkLimit::Folders(limits.folders) });
        }
        let first = match start {
            None => self.own_entries_if_there(cx),
            Some(path) => self.entries(cx, path),
        }
        .map_err(WalkError::Fs)?;
        let Some(first) = first else { return Ok(walked) };
        // The folders still to list, the next on top, each with its level.
        let mut stack: blaze_own::Vec<(Path, u32), &Frame> = blaze_own::Vec::new_in(frame);
        // One folder's marked indices, reused.
        let mut chosen: blaze_own::Vec<usize, &Frame> = blaze_own::Vec::new_in(frame);
        let mut current = (first, 0u32);
        loop {
            let (entries, depth) = current;
            let folder = self.folder_name(entries.folder());
            walked.folders += 1;
            walked.entries += entries.len() as u64;
            if walked.entries > limits.entries {
                return Err(WalkError::Limit { folder, limit: WalkLimit::Entries(limits.entries) });
            }
            chosen.clear();
            let mut enter = Enter { chosen: &mut chosen, all: false, full: false };
            visit(&entries, &mut enter).map_err(WalkError::Visit)?;
            let (all, mut full) = (enter.all, enter.full);
            if all || plant == WalkPlant::EntersDeclined {
                for (i, kind) in entries.kinds().iter().enumerate() {
                    if *kind == EntryKind::Dir && chosen.try_push(i).is_err() {
                        full = true;
                    }
                }
            }
            if full {
                return Err(out_of_room(&folder, size_of::<usize>()));
            }
            chosen.sort_unstable();
            let below = stack.len();
            let mut last = None;
            for &i in chosen.iter() {
                if last == Some(i) {
                    continue;
                }
                last = Some(i);
                if i >= entries.len() {
                    // clone: the error's name of the folder, a refcount.
                    return Err(WalkError::NoSuchEntry { folder: folder.clone(), index: i });
                }
                let sub = entries.path(i).map_err(WalkError::Name)?;
                let kind = entries.kind(i);
                if kind != EntryKind::Dir {
                    return Err(WalkError::NotAFolder { entry: DirName::Named(self.beneath(&sub)), kind });
                }
                if depth + 1 > limits.depth {
                    return Err(WalkError::Limit { folder: DirName::Named(self.beneath(&sub)), limit: WalkLimit::Depth(limits.depth) });
                }
                if stack.try_push((sub, depth + 1)).is_err() {
                    return Err(out_of_room(&folder, size_of::<(Path, u32)>()));
                }
            }
            // Pushed in name order; reversed, the first name is on top and is listed next.
            stack[below..].reverse();
            let Some((next, level)) = stack.pop() else { return Ok(walked) };
            let name = self.beneath(&next);
            if walked.folders >= limits.folders {
                return Err(WalkError::Limit { folder: DirName::Named(name), limit: WalkLimit::Folders(limits.folders) });
            }
            let listed = self.list_one(cx, DirName::Named(name), Some(next)).map_err(WalkError::Fs)?;
            current = (listed, level);
        }
    }

    /// The name under the driver's root of `folder` beneath this directory (this one for
    /// `None`).
    fn folder_name(&self, folder: Option<&Path>) -> DirName {
        match folder {
            // clone: the directory's name, a refcount.
            None => self.base.clone(),
            Some(path) => DirName::Named(self.beneath(path)),
        }
    }
}

/// The walk's frame had no room for `asked` more bytes of its stack, at `folder`.
fn out_of_room<E>(folder: &DirName, asked: usize) -> WalkError<E> {
    let stopped = Stopped::OutOfMemory { arena: "bstd::fs walk", asked };
    WalkError::Fs(FsError::new(FsOp::Entries, folder, FsKind::Stopped(stopped)))
}
