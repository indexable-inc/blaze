//! The sweep's change detection from the system's journal of changes (plan row S1): a repeat
//! request costs the changes, not the checkout, with no process alive in between (rule 70).
//!
//! **The rule.** Each request, BEFORE any file of the checkout is read, replays the journal of
//! each unit (the checkout's folder, or each top-level folder of it that holds what the last
//! answer read when the checkout is the driver's root; and blaze's folder in the home, where
//! the [`LeafKind::HomeListing`] rows are) from the place the stored answer's pointer saved:
//!
//! - the place is there and the journal names nothing: no row is statted, and the certificate
//!   path is taken from the pointer alone;
//! - it names changes: only the rows they reach are statted ([`Taken::marked`]): a row whose
//!   path, or a folder above it, is named; a listing whose folder had a name it reads made or
//!   removed; a slot `x.rs` whose `x/mod.rs` is named. A subtree the journal lost
//!   (`MustScanSubDirs`, dropped events, a moved root: blaze-core fsevents.rs maps the flags)
//!   has every row under it statted, and the folder itself lost is all of the unit's;
//! - the journal was reset (another volume UUID, wrapped IDs), no longer reaches the place, no
//!   place was saved, or the replay was refused (its deadline, a driver failure): every row of
//!   that unit is statted, and the profile says which and why ([`JournalUse::Refused`]);
//! - the system keeps no journal across a restart (Linux): every row is statted, by the same
//!   serial stats as before, and the profile says `none on this OS`.
//!
//! **The place is taken before the files are read.** A file written after the replay and
//! before its read has an event after the saved place: the next replay names it. Taken after
//! the reads, that write would fall between the read and the place, never seen again.
//!
//! **Why an unmarked row is kept as stored, racy stamp or not.** Its bytes were read after the
//! place its answer saved, and nothing has been written to it since (else the journal names
//! it): the row is today's. The racy rule (a stamp within the clock step of its sweep proves
//! nothing) is for stamps, and no stamp is consulted here.
//!
//! **Settled classes** (the toolchain's shelf, the registry sources) have no row to stat: a
//! source is a record under `<home>/.blaze/v2/sources/<key>` keyed from the lock's text and
//! checked by its digest when fetched (fetch.rs), the lock is a [`LeafKind::Manifest`] row,
//! and the toolchain is in the answer's root name (engine_answer.rs `root_name`). So no
//! dependency or toolchain file is ever statted, and the classes checked are 0.
//!
//! Nothing of the journal (IDs, UUIDs, places) enters a record key or a task row: the places
//! are the pointer's own sweep state, read only here.

use core::cmp::Ordering;
use core::fmt;
use core::time::Duration;

use blaze_core::DirName;
use blaze_own::{Texts, Vec};
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::fs::{Changes, Cursor, Dir, JournalError, Lost, ReadWrite};
use bstd::mem::Frame;
use bstd::path::Path;

use crate::CliError;
use crate::sweep::{self, LeafKind, Leaves, MODULE};

/// The most units one request replays: past it every row is statted (a checkout at the
/// driver's root with that many top-level folders read).
pub(crate) const UNITS_MOST: usize = 1024;

/// How long one request waits for all its replays: past it the replay is refused and the unit
/// swept whole. A replay on APFS is a stream started with its history, a few ms.
const JOURNAL_WAIT: Duration = Duration::from_secs(5);

/// Why a unit's rows were all statted though the system keeps a journal.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a count of a request, kept for its tests
pub(crate) enum Refused {
    /// The pointer saved no place for it (the first request, a new top-level folder).
    NoCursor,
    /// The journal could not say what changed since the place: reset, truncated.
    Lost(Lost),
    /// The replay failed or passed its deadline.
    Failed(JournalError),
    /// More units than [`UNITS_MOST`].
    Units { had: usize },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::NoCursor => f.write_str("no place in the journal was saved"),
            Refused::Lost(why) => write!(f, "{why}"),
            Refused::Failed(error) => write!(f, "{error}"),
            Refused::Units { had } => write!(f, "{had} folders to replay, past the cap of {UNITS_MOST} (journal_sweep.rs `UNITS_MOST`)"),
        }
    }
}

/// Whether the journal decided what the sweep stats.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: as `Refused`
pub(crate) enum JournalUse {
    /// Every unit replayed from its saved place.
    Used,
    /// At least one unit's rows were all statted: the first one's why.
    Refused(Refused),
    /// The system keeps no journal across a restart: every row is statted.
    NoneOnThisOs,
}

impl fmt::Display for JournalUse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JournalUse::Used => f.write_str("used"),
            JournalUse::Refused(why) => write!(f, "refused: {why}"),
            JournalUse::NoneOnThisOs => f.write_str("none on this OS"),
        }
    }
}

/// What a request's sweep cost: the profile's `sweep:` line, and the tests' counts.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: as `Refused`
pub(crate) struct SweepCounts {
    pub(crate) journal: JournalUse,
    /// The names and lost subtrees the journal told in the checkout (blaze's own folder,
    /// git's and cargo's in its root not counted: no row is there). The home's replay is not
    /// counted: the store writes there every request.
    pub(crate) events: u64,
    /// The rows looked at (statted, and read where the stamp did not prove them).
    pub(crate) statted: u32,
    /// The rows of the stored answer's leaf table.
    pub(crate) leaves: u32,
    /// The settled classes checked: 0, each is covered by a key (the module's doc).
    pub(crate) settled: u32,
}

impl fmt::Display for SweepCounts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sweep: journal {}; events {}; paths statted {} of {}; settled classes checked {}", self.journal, self.events, self.statted, self.leaves, self.settled)
    }
}

/// Which folder a unit's journal is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Where {
    Checkout = 0,
    Home = 1,
}

/// One replayed folder: its name (relative to the checkout, or `""` for the checkout or the
/// home folder itself), the place to save, and what it heard.
struct Unit {
    at: Where,
    /// Index of the name in [`Taken::names`].
    name: usize,
    /// The place to save: `None` where the replay was refused (the next request sweeps it).
    cursor: Option<Cursor>,
    /// What changed: `None` when every row of the unit is statted.
    heard: Option<Changes>,
}

/// A request's replays, taken before any file of the checkout is read.
pub(crate) struct Taken<'f> {
    names: Texts<&'f Frame>,
    units: Vec<Unit, &'f Frame>,
    pub(crate) journal: JournalUse,
    pub(crate) events: u64,
}

/// The places the pointer saved, read in place:
///
/// ```text
/// units u16 | units x (where u8 | name bytes u16 | name | cursor [Cursor::BYTES])
/// ```
#[derive(Clone, Copy)]
pub(crate) struct Stored<'m> {
    bytes: &'m [u8],
}

/// Bytes of one unit's fixed part beside its name.
const UNIT_FIXED: usize = 1 + 2 + Cursor::BYTES;

impl<'m> Stored<'m> {
    /// No place saved.
    pub(crate) const NONE: Stored<'static> = Stored { bytes: &[0, 0] };

    /// The places in `bytes`, all of them: `None` when they are not whole (an older pointer).
    pub(crate) fn of(bytes: &'m [u8]) -> Option<Stored<'m>> {
        let stored = Stored { bytes };
        let mut at = 2;
        let units = usize::from(u16::from_le_bytes([*bytes.first()?, *bytes.get(1)?]));
        for _ in 0..units {
            let len = usize::from(u16::from_le_bytes([*bytes.get(at + 1)?, *bytes.get(at + 2)?]));
            let name = bytes.get(at + 3..at + 3 + len)?;
            core::str::from_utf8(name).ok()?;
            let cursor = bytes.get(at + 3 + len..at + UNIT_FIXED + len)?;
            Cursor::decode(cursor)?;
            if *bytes.get(at)? > Where::Home as u8 {
                return None;
            }
            at += UNIT_FIXED + len;
        }
        (at == bytes.len()).then_some(stored)
    }

    /// Each unit's (where, name, place), in the order saved.
    fn units(&self) -> impl Iterator<Item = (u8, &'m str, Cursor)> + 'm {
        let bytes = self.bytes;
        let units = usize::from(u16::from_le_bytes([bytes[0], bytes[1]]));
        let mut at = 2;
        (0..units).map(move |_| {
            let len = usize::from(u16::from_le_bytes([bytes[at + 1], bytes[at + 2]]));
            // Checked whole by `of`.
            let name = core::str::from_utf8(&bytes[at + 3..at + 3 + len]).expect("checked by Stored::of");
            let cursor = Cursor::decode(&bytes[at + 3 + len..]).expect("checked by Stored::of");
            let kind = bytes[at];
            at += UNIT_FIXED + len;
            (kind, name, cursor)
        })
    }

    fn cursor_of(&self, at: Where, name: &str) -> Option<Cursor> {
        self.units().find(|(kind, it, _)| *kind == at as u8 && *it == name).map(|(_, _, cursor)| cursor)
    }
}

/// The replays of a request (the module's rule), each under one deadline from `clock`, from
/// the places in `stored`; `last`: the stored answer's leaf table, whose top-level folders are
/// the units when the checkout is the driver's root. Taken BEFORE any file is read.
pub(crate) fn take<'f>(cx: &Cx, root: &Dir<ReadWrite>, home: &Dir<ReadWrite>, clock: &Clock, stored: Stored<'_>, last: Option<&Leaves<'_>>, frame: &'f Frame) -> Result<Taken<'f>, CliError> {
    let cx = cx.span("sweep: the journal of changes replayed from the stored answer's place");
    let cx = cx.until(clock.after(JOURNAL_WAIT));
    let mut taken = Taken { names: Texts::new_in(frame), units: Vec::new_in(frame), journal: JournalUse::Used, events: 0 };
    // The checkout's units: its folder, or the top-level folders the leaves are in.
    let mut tops: Vec<&str, &Frame> = Vec::new_in(frame);
    match root.name() {
        DirName::Named(_) => tops.push(""),
        DirName::Root => {
            if let Some(last) = last {
                for i in 0..last.len() {
                    if last.row(i).kind == LeafKind::HomeListing {
                        continue;
                    }
                    if let Some((top, _)) = last.path(i).split_once('/') {
                        tops.push(top);
                    }
                }
            }
            tops.sort_unstable();
        }
    }
    // Sorted: a name equal to the one before it is the same folder.
    let distinct = tops.iter().enumerate().filter(|(i, top)| *i == 0 || tops[i - 1] != **top).count();
    if distinct > UNITS_MOST {
        taken.journal = JournalUse::Refused(Refused::Units { had: distinct });
        return Ok(taken);
    }
    for (i, top) in tops.iter().enumerate() {
        if i > 0 && tops[i - 1] == *top {
            continue;
        }
        let dir = match top.is_empty() {
            true => root.clone(),
            false => root.subdir(&Path::new(top)?),
        };
        if !taken.replay(&cx, &dir, Where::Checkout, top, stored) {
            return Ok(taken);
        }
    }
    // blaze's folder in the home, where the `HomeListing` rows are: replayed on every request,
    // the first included, so the second has its place.
    taken.replay(&cx, home, Where::Home, "", stored);
    Ok(taken)
}

impl Taken<'_> {
    /// One unit replayed; `false` when the system keeps no journal (nothing more is asked).
    fn replay(&mut self, cx: &Cx, dir: &Dir<ReadWrite>, at: Where, name: &str, stored: Stored<'_>) -> bool {
        let name_at = self.names.push(name);
        let since = stored.cursor_of(at, name);
        let changes = match dir.changes_since(cx, since.unwrap_or(Cursor::None)) {
            Ok(changes) => changes,
            Err(JournalError::NoJournal { .. }) => {
                self.journal = JournalUse::NoneOnThisOs;
                self.units.clear();
                return false;
            }
            Err(error) => {
                self.refuse(Refused::Failed(error));
                self.units.push(Unit { at, name: name_at, cursor: None, heard: None });
                return true;
            }
        };
        let cursor = Some(changes.cursor);
        // The folder itself lost because the journal cannot reach the place: refused, by name.
        let whole = changes.lost().find(|(under, _)| under.is_empty()).map(|(_, why)| why);
        let heard = match (since, whole) {
            (None, _) => {
                self.refuse(Refused::NoCursor);
                None
            }
            (Some(_), Some(why @ (Lost::Reset | Lost::Truncated | Lost::NoJournal))) => {
                self.refuse(Refused::Lost(why));
                None
            }
            (Some(_), _) => Some(changes),
        };
        if let Some(changes) = &heard {
            // The checkout's alone: blaze's folder in the home is the store's too, written by
            // every request after its replay, so its names say nothing of the edit.
            let counted = |path: &str| at == Where::Checkout && (!name.is_empty() || !own(path));
            let told = changes.files().filter(|it| counted(it)).count() + changes.lost().filter(|(it, _)| counted(it)).count();
            self.events = self.events.saturating_add(u64::try_from(told).unwrap_or(u64::MAX));
        }
        self.units.push(Unit { at, name: name_at, cursor, heard });
        true
    }

    /// A unit's rows are all statted for `why`: the profile names the first such why.
    fn refuse(&mut self, why: Refused) {
        if self.journal == JournalUse::Used {
            self.journal = JournalUse::Refused(why);
        }
    }

    /// Which rows of `last` the sweep looks at: `None` for all (no journal, or too many
    /// units); else one mark a row, in its order ([`sweep::Follow::follow_marked`]).
    pub(crate) fn marked<'f>(&self, last: &Leaves<'_>, frame: &'f Frame) -> Option<Vec<bool, &'f Frame>> {
        if matches!(self.journal, JournalUse::NoneOnThisOs | JournalUse::Refused(Refused::Units { .. })) {
            return None;
        }
        // Each unit's names and lost subtrees, relative to it, as sorted columns.
        let mut indexes: Vec<Index<'_>, &Frame> = Vec::with_capacity_in(self.units.len(), frame);
        for unit in self.units.iter() {
            indexes.push(Index::of(unit, self.names.text(unit.name), frame));
        }
        let mut marks = Vec::with_capacity_in(last.len(), frame);
        for i in 0..last.len() {
            let row = last.row(i);
            let path = last.path(i);
            let at = if row.kind == LeafKind::HomeListing { Where::Home } else { Where::Checkout };
            let found = self.units.iter().zip(indexes.iter()).find_map(|(unit, index)| {
                if unit.at != at {
                    return None;
                }
                let name = self.names.text(unit.name);
                match name.is_empty() {
                    true => Some((index, path)),
                    false => path.strip_prefix(name).and_then(|rest| rest.strip_prefix('/')).map(|rel| (index, rel)),
                }
            });
            // A row in no replayed unit (a root-level row of a checkout at the driver's root,
            // or of a unit the request did not replay) is looked at.
            let mark = match found {
                None => true,
                Some((index, rel)) => index.reaches(row.kind, row.flags, rel),
            };
            marks.push(mark);
        }
        Some(marks)
    }

    /// The places to save in the pointer (the [`Stored`] form): each unit replayed.
    pub(crate) fn encode(&self, out: &mut Vec<u8, &Frame>) {
        let saved = self.units.iter().filter(|unit| unit.cursor.is_some());
        let count = u16::try_from(saved.clone().count()).expect("at most UNITS_MOST units");
        out.extend_from_slice(&count.to_le_bytes());
        let mut cursor_bytes = std::vec::Vec::with_capacity_in(Cursor::BYTES, std::alloc::Global);
        for unit in saved {
            let name = self.names.text(unit.name);
            out.push(unit.at as u8);
            // A unit's name is one part of a path: under 64 KiB.
            out.extend_from_slice(&u16::try_from(name.len()).unwrap_or(u16::MAX).to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            cursor_bytes.clear();
            unit.cursor.expect("filtered above").encode(&mut cursor_bytes);
            out.extend_from_slice(&cursor_bytes);
        }
    }

    /// The counts of a sweep that looked at `statted` rows of `leaves`.
    pub(crate) fn counts(&self, statted: u32, leaves: u32) -> SweepCounts {
        // clone: the request's one count of its journal, kept for its tests.
        SweepCounts { journal: self.journal.clone(), events: self.events, statted, leaves, settled: 0 }
    }
}

/// A name in the checkout's root that no row reads: blaze's folder, git's and cargo's.
fn own(rel: &str) -> bool {
    let first = rel.split('/').next().unwrap_or(rel);
    sweep::not_the_checkouts(first)
}

/// One unit's journal as sorted columns, relative to it.
struct Index<'a> {
    /// The unit's rows are all looked at.
    whole: bool,
    /// The names told, sorted.
    files: Vec<&'a str, &'a Frame>,
    /// Each told name as (its folder, its last part), sorted.
    children: Vec<(&'a str, &'a str), &'a Frame>,
    lost: Vec<&'a str, &'a Frame>,
}

impl<'a> Index<'a> {
    fn of(unit: &'a Unit, name: &str, frame: &'a Frame) -> Index<'a> {
        let mut index = Index { whole: false, files: Vec::new_in(frame), children: Vec::new_in(frame), lost: Vec::new_in(frame) };
        let Some(changes) = &unit.heard else {
            index.whole = true;
            return index;
        };
        // In the checkout's own folder, blaze's, git's and cargo's are read by no row.
        let kept = |path: &str| unit.at == Where::Home || !name.is_empty() || !own(path);
        for file in changes.files().filter(|it| kept(it)) {
            index.files.push(file);
            let (folder, last) = file.rsplit_once('/').unwrap_or(("", file));
            index.children.push((folder, last));
        }
        for (under, _) in changes.lost().filter(|(it, _)| kept(it)) {
            if under.is_empty() {
                index.whole = true;
            }
            index.lost.push(under);
            // A folder lost: the listing it is in is looked at too.
            let (folder, last) = under.rsplit_once('/').unwrap_or(("", under));
            index.children.push((folder, last));
        }
        // `files` is sorted already (bstd `Changes::files`); the children by folder.
        index.children.sort_unstable();
        index
    }

    /// Whether a row of `kind` at `rel` (relative to the unit) is reached by what changed.
    fn reaches(&self, kind: LeafKind, flags: u8, rel: &str) -> bool {
        if self.whole {
            return true;
        }
        match kind {
            LeafKind::Slot if flags & MODULE != 0 => {
                // `x.rs` and `x/mod.rs` are both probed: either named moves the slot.
                let stem = rel.strip_suffix(".rs").unwrap_or(rel);
                self.touched(rel) || self.touched(stem) || self.files.binary_search_by(|it| joined_cmp(it, stem, "/mod.rs")).is_ok()
            }
            LeafKind::Slot | LeafKind::Manifest | LeafKind::MacroFile => self.touched(rel),
            LeafKind::Listing => {
                let last = rel.rsplit('/').next().unwrap_or(rel);
                // Only a name the listing hands the layout moves it (sweep.rs `wanted`); a
                // publish's temporary name beside a source file does not.
                self.touched(rel) || self.children_of(rel).any(|name| sweep::wanted(last, name, true) || sweep::wanted(last, name, false))
            }
            LeafKind::MacroFolder | LeafKind::HomeListing => self.touched(rel) || self.children_of(rel).next().is_some(),
            // Decided by its files' and folders' rows (macro_files.rs `proven`).
            LeafKind::MacroFiles => false,
        }
    }

    /// Whether `path`, or a folder above it, is named or lost.
    fn touched(&self, path: &str) -> bool {
        if self.files.binary_search(&path).is_ok() {
            return true;
        }
        let above = path.char_indices().filter(|(_, c)| *c == '/').any(|(at, _)| self.files.binary_search(&&path[..at]).is_ok());
        above || self.lost.iter().any(|under| path == *under || path.strip_prefix(under).is_some_and(|rest| rest.starts_with('/')))
    }

    /// The last parts of the names told directly in `folder`.
    fn children_of<'s>(&'s self, folder: &'s str) -> impl Iterator<Item = &'a str> + 's {
        let start = self.children.partition_point(|(it, _)| *it < folder);
        self.children[start..].iter().take_while(move |(it, _)| *it == folder).map(|(_, name)| *name)
    }
}

/// `a` against `x` followed by `y`, by bytes, with nothing joined.
fn joined_cmp(a: &str, x: &str, y: &str) -> Ordering {
    a.bytes().cmp(x.bytes().chain(y.bytes()))
}
