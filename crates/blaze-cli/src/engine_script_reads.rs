//! A member build script's reads of the checkout as engine cells (runtime.md point 2: a fact is a
//! cell). The sandbox records each read a run makes of the served tree with the answer it gave
//! (blaze-proc-macro script_sandbox.rs `record`); for a member that tree is the checkout, and a
//! member producer's mount is the checkout too, recorded under the same checkout-relative
//! subjects (dep_mount.rs `Served::ReaderTree`). Each read becomes one `Kind::Given` cell whose
//! digest is exactly what the script could observe, and nothing more:
//!
//! | subject | the cell's digest |
//! |---|---|
//! | `file:<path>` | the file's bytes ([`script_sandbox::file_answer`]); [`NONE`] when absent |
//! | `stat:<path>` | its kind, and a file's size ([`script_sandbox::stat_answer`]): a folder's does not depend on what is in it |
//! | `dir:<path>` | its entries' names and kinds, one level ([`script_sandbox::listing_answer`]) |
//!
//! A path that is absent has its own digest ([`NONE`]); a folder where a file was read, or a file
//! where a folder was listed, has another ([`NOT_A_FILE`], [`NOT_A_FOLDER`]): the run could only
//! have seen something else there. Reads under `OUT_DIR` and of store-served trees are never
//! recorded by the sandbox (script_sandbox.rs `Place::Out`, `Served::Keyed`), so none is a
//! checkout read. A `git:` fact and any subject of another kind has no cell: a typed refusal
//! ([`NoCell`]), the run a hole as before.
//!
//! The cells live three places, one concept each:
//! - the leaf table (sweep.rs `LeafKind::ScriptFile`, `ScriptListing`, `ScriptStat`): each path
//!   a script read, followed by the sweep ([`now`]) and seen by the journal sweep, its id the
//!   digest above;
//! - the plan: one `Kind::Given` task a leaf row, in the scripts stage's wave 0 ([`plan`]), its
//!   key moved exactly when the answer did;
//! - the script's record: its reads hold `(cell, digest seen)`, so the engine enters the script
//!   again exactly when a read moved, and keeps its outputs otherwise.
//!
//! A read new this request (no leaf row yet, so no planned task) is made by the script that read
//! it, as a [`Made`] record of wave 0 (engine.rs invariant 12: its maker reads it).

use blaze_drive::key::Kind;
use blaze_drive::{Key, Name};
use blaze_engine::{Done, Made, Phase, Planned, SVec, Stage, Task, Tools};
use blaze_ids::ContentId;
use blaze_own::SharedAlloc;
use blaze_proc_macro::script_sandbox;
use bstd::cx::Cx;
use bstd::fs::{Dir, EntryKind, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use crate::CliError;
use crate::engine_tasks::WHOLE;
use crate::sweep::{LeafKind, Plant, Rows, Stamp, Stamps};

const NAME_DOMAIN: &str = "blaze-cli script read cell 1";
const SOURCE_DOMAIN: &str = "blaze-cli script read source 1";
#[cfg(test)]
const ANSWER_DOMAIN: &str = "blaze-cli script read answer 1";

/// The answer of a read of a path where nothing is: what the sandbox records as no digest.
pub(crate) const NONE: ContentId = ContentId([0xA0; 32]);
/// A `file:` read's path is a folder today: the run would have opened a folder, not this file.
pub(crate) const NOT_A_FILE: ContentId = ContentId([0xA1; 32]);
/// A `dir:` read's path is a file today: the run would have read a file, not this listing.
pub(crate) const NOT_A_FOLDER: ContentId = ContentId([0xA2; 32]);

/// The most checkout reads one run's record keeps: past it the run is refused by name (rule 18).
pub(crate) const READS_CAP: usize = 1 << 16;

/// What a read observed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum ReadKind {
    File,
    Listing,
    Stat,
}

impl ReadKind {
    pub(crate) fn leaf(self) -> LeafKind {
        match self {
            ReadKind::File => LeafKind::ScriptFile,
            ReadKind::Listing => LeafKind::ScriptListing,
            ReadKind::Stat => LeafKind::ScriptStat,
        }
    }

    /// The read kind of a leaf row, `None` for a row no script made.
    pub(crate) fn of_leaf(kind: LeafKind) -> Option<ReadKind> {
        match kind {
            LeafKind::ScriptFile => Some(ReadKind::File),
            LeafKind::ScriptListing => Some(ReadKind::Listing),
            LeafKind::ScriptStat => Some(ReadKind::Stat),
            LeafKind::Slot
            | LeafKind::Manifest
            | LeafKind::Listing
            | LeafKind::HomeListing
            | LeafKind::MacroFiles
            | LeafKind::MacroFile
            | LeafKind::MacroFolder => None,
        }
    }

    fn byte(self) -> u8 {
        self as u8
    }
}

/// One read of the checkout, as the worker hands it back (in `Global`: its frame ends with it).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct CheckoutRead {
    pub(crate) kind: ReadKind,
    /// Checkout-relative, `/`-separated, no `.` or `..` part; "" is the checkout's root.
    pub(crate) path: std::string::String,
    /// What the run saw ([`NONE`] for nothing there).
    pub(crate) answer: ContentId,
}

impl CheckoutRead {
    pub(crate) fn name(&self) -> Name {
        cell_name(self.kind, &self.path)
    }

    pub(crate) fn source(&self) -> ContentId {
        source(self.kind, &self.path, self.answer)
    }
}

/// Why a recorded read has no cell: the run stays a hole, named (rule 19).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum NoCell {
    /// A `git:` fact: answered from `.git` files that are not in the read list (D54).
    Git { subject: std::string::String },
    /// A subject of a kind no cell is made for.
    Kind { subject: std::string::String },
    /// A path that is not checkout-relative (absolute, or with a `.` or `..` part).
    Path { subject: std::string::String },
    /// More reads than [`READS_CAP`].
    TooMany { reads: usize },
}

impl core::fmt::Display for NoCell {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NoCell::Git { subject } => write!(f, "it asked git `{}`: a git fact is answered from `.git` files no cell names yet (D54), so its outputs are not kept (engine_script_reads.rs)", subject.replace('\n', " ")),
            NoCell::Kind { subject } => write!(f, "it read `{subject}`, a kind of read no cell is made for (engine_script_reads.rs `parse`)"),
            NoCell::Path { subject } => write!(f, "it read `{subject}`, a path that is not relative to the checkout (engine_script_reads.rs `parse`)"),
            NoCell::TooMany { reads } => write!(f, "it read {reads} paths of the checkout, past the {READS_CAP} one run's record keeps (engine_script_reads.rs `READS_CAP`)"),
        }
    }
}

/// The bug a test plants in [`cells_of`], to show its test catches it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ReadsPlant {
    None,
    /// Every `file:` read is dropped: an edit of a file the script read is missed.
    FileDropped,
}

/// One recorded read as its cell: `digest` is the sandbox's answer, `None` for nothing there.
pub(crate) fn parse(subject: &str, digest: Option<ContentId>) -> Result<CheckoutRead, NoCell> {
    let owned = || std::string::String::from(subject);
    let Some((kind, path)) = subject.split_once(':') else { return Err(NoCell::Kind { subject: owned() }) };
    let kind = match kind {
        "file" => ReadKind::File,
        "dir" => ReadKind::Listing,
        "stat" => ReadKind::Stat,
        "git" => return Err(NoCell::Git { subject: owned() }),
        _ => return Err(NoCell::Kind { subject: owned() }),
    };
    let relative = !path.starts_with('/') && (path.is_empty() || path.split('/').all(|part| !part.is_empty() && part != "." && part != ".."));
    if !relative {
        return Err(NoCell::Path { subject: owned() });
    }
    Ok(CheckoutRead { kind, path: std::string::String::from(path), answer: digest.unwrap_or(NONE) })
}

/// A run's recorded reads as cells, each once at its first read (the sandbox records each
/// subject once already); the first that has none refuses the whole list.
pub(crate) fn cells_of<'s>(reads: impl Iterator<Item = (&'s str, Option<ContentId>)>, plant: ReadsPlant) -> Result<std::vec::Vec<CheckoutRead>, NoCell> {
    let mut cells = std::vec::Vec::new();
    for (subject, digest) in reads {
        let read = parse(subject, digest)?;
        if plant == ReadsPlant::FileDropped && read.kind == ReadKind::File {
            continue;
        }
        if cells.len() == READS_CAP {
            return Err(NoCell::TooMany { reads: READS_CAP + 1 });
        }
        cells.push(read);
    }
    Ok(cells)
}

/// The cell of a read of `kind` at `path`: one name whoever reads it.
pub(crate) fn cell_name(kind: ReadKind, path: &str) -> Name {
    let mut h = blaze_ids::Hasher::new(NAME_DOMAIN);
    h.update(&[kind.byte()]).update(path.as_bytes());
    Name::new(Kind::Given, h.finish())
}

/// The source of the `Given` task of a read: its kind, its path and today's answer, so its key
/// moves exactly when the answer does (rule 21).
pub(crate) fn source(kind: ReadKind, path: &str, answer: ContentId) -> ContentId {
    let mut h = blaze_ids::Hasher::new(SOURCE_DOMAIN);
    h.update(&[kind.byte()]).update(&(path.len() as u64).to_le_bytes()).update(path.as_bytes()).update(&answer.0);
    h.finish()
}

/// The scripts stage's wave 0, where every read's cell is defined.
pub(crate) const PHASE: Phase = Phase::new(Stage::Scripts, 0);

/// One `Given` task a script-read row of today's leaf rows, each its own file (a row that goes
/// retires its task, and the cell moves to absent for its readers), with today's answer.
pub(crate) fn plan<'h>(rows: &Rows<'_>, alloc: SharedAlloc<'h>) -> SVec<'h, (Planned, ContentId, ReadKind)> {
    let mut planned = SVec::new_in(alloc);
    for leaf in rows.rows.iter() {
        let Some(kind) = ReadKind::of_leaf(leaf.kind) else { continue };
        let path = rows.path(leaf);
        let name = cell_name(kind, path);
        planned.push((Planned { name, phase: PHASE, source: source(kind, path, leaf.id), file: name }, leaf.id, kind));
    }
    planned.sort_unstable_by_key(|(one, _, _)| one.name);
    planned.dedup_by_key(|(one, _, _)| one.name);
    planned
}

/// A planned `Given` task's result: it defines its cell whole as today's answer, reading nothing.
pub(crate) fn given_done<'a>(task: &Task, answer: ContentId, alloc: SharedAlloc<'a>) -> Done<'a> {
    let mut defines = SVec::new_in(alloc);
    defines.push((WHOLE, answer));
    Done { name: task.planned.name, key: task.key, out: answer, defines, reads: SVec::new_in(alloc), findings: 0, failed: false }
}

/// The record a script makes of a read no task of today's plan defines (new this request): wave
/// 0, keyed as the planned task of the next request will be, so that task stands by it.
pub(crate) fn made<'a>(read: &CheckoutRead, by: Name, tools: &Tools, alloc: SharedAlloc<'a>) -> Made<'a> {
    let name = read.name();
    let source = read.source();
    let key: Key = tools.key(name, &source);
    let mut defines = SVec::new_in(alloc);
    defines.push((WHOLE, read.answer));
    let done = Done { name, key, out: read.answer, defines, reads: SVec::new_in(alloc), findings: 0, failed: false };
    Made { planned: Planned { name, phase: PHASE, source, file: name }, done, by }
}

/// Pushes onto `out` the leaf rows of the script reads the answer still reads: each followed row
/// whose cell has a row in `kept_cells`, then each read of this request's runs (`fresh`) not
/// among them. A read no row reads any more leaves the table, so its task is retired.
pub(crate) fn keep_rows<'r>(followed: Option<&Rows<'_>>, fresh: impl Iterator<Item = &'r CheckoutRead>, kept_cells: impl Fn(Name) -> bool, out: &mut Rows<'_>) {
    let mut seen: std::collections::BTreeSet<Name> = std::collections::BTreeSet::new();
    for followed in followed.into_iter() {
        for leaf in followed.rows.iter() {
            let Some(kind) = ReadKind::of_leaf(leaf.kind) else { continue };
            let path = followed.path(leaf);
            let name = cell_name(kind, path);
            if kept_cells(name) && seen.insert(name) {
                out.push(leaf.kind, 0, path, leaf.stamp, leaf.id);
            }
        }
    }
    for read in fresh {
        let name = read.name();
        if kept_cells(name) && seen.insert(name) {
            out.push(read.kind.leaf(), 0, &read.path, None, read.answer);
        }
    }
}

/// A row of a script read today: its flags (always 0: [`NONE`] is an answer, not an absent
/// row), its stamp when one proves it, and its answer, computed as the sandbox computes it. A
/// file's or a folder's settled stamp equal to `old`'s keeps `old`'s answer unread; a stat needs
/// the stat alone.
pub(crate) fn now(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    stamps: &impl Stamps,
    plant: Plant,
    kind: ReadKind,
    path: &str,
    old: Option<(Option<Stamp>, ContentId)>,
    frame: &Frame,
) -> Result<(u8, Option<Stamp>, ContentId), CliError> {
    let meta = match path.is_empty() {
        true => None,
        false => root.stat(cx, &Path::new(path)?)?,
    };
    let is_dir = path.is_empty() || meta.as_ref().is_some_and(|meta| meta.dir);
    if !path.is_empty() && meta.is_none() {
        return Ok((0, None, NONE));
    }
    let stamp = meta.as_ref().and_then(|meta| match plant {
        Plant::RacyTrusted => stamps.stamp(path, meta),
        Plant::None | Plant::AbsentForgotten => stamps.stamp(path, meta).filter(|it| it.settled(stamps.swept_at_ns(), stamps.granularity_ns())),
    });
    let proven = match (stamp, old) {
        (Some(now), Some((Some(was), id))) if now == was && id != NONE && id != NOT_A_FILE && id != NOT_A_FOLDER => Some(id),
        _ => None,
    };
    match (kind, proven) {
        (ReadKind::Stat, _) => Ok((0, None, script_sandbox::stat_answer(is_dir, meta.as_ref().map_or(0, |meta| meta.len)))),
        (ReadKind::File, _) if is_dir => Ok((0, None, NOT_A_FILE)),
        (ReadKind::Listing, _) if !is_dir => Ok((0, None, NOT_A_FOLDER)),
        (_, Some(id)) => Ok((0, stamp, id)),
        (ReadKind::File, None) => {
            let mut bytes = Bytes::new_in(frame, "a file a build script read");
            match root.read_all(cx, &Path::new(path)?, &mut bytes) {
                Ok(_) => Ok((0, stamp, script_sandbox::file_answer(bytes.as_slice()))),
                Err(error) if error.kind == bstd::fs::FsKind::NotFound => Ok((0, None, NONE)),
                Err(error) => Err(error.into()),
            }
        }
        (ReadKind::Listing, None) => Ok((0, stamp, listing(cx, root, path, frame)?)),
    }
}

/// The `dir:` answer of the folder at `path` ("" the checkout's root): every entry, in name order
/// (bstd sorts them), a folder through any link counted a folder and anything else a file, as
/// the sandbox lists it (script_sandbox.rs `Tree::node`); an entry gone since the listing is left
/// out, as the sandbox's `read_dir` leaves it out.
fn listing(cx: &Cx, root: &Dir<ReadWrite>, path: &str, frame: &Frame) -> Result<ContentId, CliError> {
    let listed = match path.is_empty() {
        true => root.own_entries(cx)?,
        false => match root.entries(cx, &Path::new(path)?)? {
            Some(listed) => listed,
            None => return Ok(NONE),
        },
    };
    let mut entries: blaze_own::Vec<(&str, bool), &Frame> = blaze_own::Vec::with_capacity_in(listed.len(), frame);
    let mut joined = blaze_own::String::new_in(frame);
    for i in 0..listed.len() {
        let name = listed.name(i);
        let is_dir = match listed.kind(i) {
            EntryKind::Dir => true,
            EntryKind::File | EntryKind::Other => false,
            EntryKind::Gone => continue,
            // A link, or a kind the listing cannot tell: what it leads to, as `is_dir` follows it.
            EntryKind::Link | EntryKind::Unknown => {
                joined.clear();
                if !path.is_empty() {
                    joined.push_str(path);
                    joined.push('/');
                }
                joined.push_str(name);
                root.stat(cx, &Path::new(joined.as_str())?)?.is_some_and(|meta| meta.dir)
            }
        };
        entries.push((name, is_dir));
    }
    Ok(script_sandbox::listing_answer(entries.iter().map(|(name, is_dir)| (name.as_bytes(), *is_dir))))
}

/// The answer a test's fake script computes from what it read: one digest of every read's answer
/// in order, so two runs that saw the same answers make the same outputs.
#[cfg(test)]
pub(crate) fn outputs_of(reads: &[CheckoutRead]) -> ContentId {
    let mut h = blaze_ids::Hasher::new(ANSWER_DOMAIN);
    for read in reads.iter() {
        h.update(&[read.kind.byte()]).update(read.path.as_bytes()).update(&[0]).update(&read.answer.0);
    }
    h.finish()
}
