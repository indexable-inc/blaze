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
//! | `git:<folder>\n<words>` | the answer git gives (exit code, stdout, stderr), recomputed from the checkout's `.git` ([`git_now`]); [`NONE`] when refused |
//! | `git-scope:<folder>` | the checkout-relative worktree where this folder's status question binds |
//!
//! A path that is absent has its own digest ([`NONE`]); a folder where a file was read, or a file
//! where a folder was listed, has another ([`NOT_A_FILE`], [`NOT_A_FOLDER`]): the run could only
//! have seen something else there. Reads under `OUT_DIR` and of store-served trees are never
//! recorded by the sandbox (script_sandbox.rs `Place::Out`, `Served::Keyed`), so none is a
//! checkout read.
//!
//! A git fact is a derived cell (D54): its digest is the answer, not the `.git` files behind it,
//! so a commit that leaves the answer alone (a new commit on another branch, a `gc` that packs
//! the refs) moves nothing, and one that changes it moves exactly the scripts that asked. What
//! each question reads to be answered (blaze-build-script git.rs, git_objects.rs):
//!
//! | question | reads |
//! |---|---|
//! | `--version` | nothing: blaze's fixed [`git::VERSION`], a fact of blaze's build like a compiler's, never of the machine's git |
//! | `rev-parse --is-inside-work-tree` | the `.git` (or `.git` file) found from the folder up |
//! | `rev-parse HEAD`, `--abbrev-ref HEAD`, `branch --show-current`, `symbolic-ref HEAD` | that, `HEAD`, `commondir`, the ref's loose file, `packed-refs` |
//! | `log -1 --pretty=format:..` | that, and the commit object (a loose object, or a pack's `.idx` and its entry's range) |
//! | `describe [--always] [--tags] [--match ..]` | that, `refs/tags` (a tag refuses: not built), and for `--always` the objects to size the abbreviation |
//! | `status --porcelain` | repository discovery from the script's folder, then the status of that repository, including the selected untracked mode |
//!
//! Any other subject has no cell: a typed refusal ([`NoCell`]), the run's outputs used by this
//! command and not kept (scripts.rs `run`, engine_scripts.rs `run_phase`).
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
//!
//! A BODY's read of a checkout file as data that no `FileItems` task defines (a computed
//! include, [`file_read`]) is the same `file:` read: the same cell, leaf kind and `Given` task,
//! one a path whoever read it. It makes no record: its row keeps the answer it saw and its leaf
//! row is kept by the request (engine_bodies.rs `BodiesStage::data_files`), so the next
//! request's `Given` task defines the cell, equal to what the body saw unless the file moved
//! (then the body is entered). A leaf stays while a row reads its cell ([`keep_rows`]).

use blaze_build_script::git::{self, GitFiles};
use blaze_drive::key::Kind;
use blaze_drive::{Key, Name};
use blaze_engine::{Done, Made, Phase, Planned, SVec, Stage, Task, Tools};
use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, SharedAlloc};
use blaze_proc_macro::script_sandbox;
use bstd::cx::Cx;
use bstd::fs::{Dir, EntryKind, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use blaze_core::Driver;

use crate::CliError;
use crate::engine_tasks::WHOLE;
use crate::journal_sweep::Taken;
use crate::store::{CommandStore, Sharing, Store as _};
use crate::sweep::{LeafKind, Plant, Rows, Stamp, Stamps};

const NAME_DOMAIN: &str = "blaze-cli script read cell 1";
const SOURCE_DOMAIN: &str = "blaze-cli script read source 1";
#[cfg(test)]
const ANSWER_DOMAIN: &str = "blaze-cli script read answer 1";

/// The answer of a read of a path where nothing is: what the sandbox records as no digest.
pub(crate) const NONE: ContentId = ContentId([0xA0; 32]);
pub(crate) use script_sandbox::{NOT_A_FILE, NOT_A_FOLDER};
/// A read now stops at the sandbox boundary. The script must enter and see
/// that refusal itself, even when its old observation was absence.
pub(crate) const DENIED: ContentId = ContentId([0xA3; 32]);

/// The most checkout reads one run's record keeps: past it the run is refused by name (rule 18).
pub(crate) const READS_CAP: usize = script_sandbox::READS_CAP;

/// What a read observed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum ReadKind {
    File,
    Listing,
    Stat,
    /// A git question (`<folder>\n<words>`) or its discovery binding (a folder with no newline).
    Git,
}

impl ReadKind {
    pub(crate) fn leaf(self) -> LeafKind {
        match self {
            ReadKind::File => LeafKind::ScriptFile,
            ReadKind::Listing => LeafKind::ScriptListing,
            ReadKind::Stat => LeafKind::ScriptStat,
            ReadKind::Git => LeafKind::ScriptGit,
        }
    }

    /// The read kind of a leaf row, `None` for a row no script made.
    pub(crate) fn of_leaf(kind: LeafKind) -> Option<ReadKind> {
        match kind {
            LeafKind::ScriptFile => Some(ReadKind::File),
            LeafKind::ScriptListing => Some(ReadKind::Listing),
            LeafKind::ScriptStat => Some(ReadKind::Stat),
            LeafKind::ScriptGit => Some(ReadKind::Git),
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

/// The worker's result outlives its frame and is consumed on the coordinator.
pub(crate) type ReadList = SVec<'static, CheckoutRead>;
pub(crate) type Subject = blaze_own::String<SharedAlloc<'static>>;

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct CheckoutRead {
    pub(crate) kind: ReadKind,
    /// Checkout-relative, `/`-separated, no `.` or `..` part; "" is the checkout's root.
    pub(crate) path: Subject,
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

/// Why a recorded read has no cell, named (rule 19): the run's outputs are used by this command
/// and not kept, and it runs again next command (scripts.rs `run`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum NoCell {
    /// Malformed question or a command the sandbox cannot answer.
    Git { subject: Subject },
    /// No answer was observed for a derived fact. Unlike a missing file, a failed
    /// probe cannot certify a successful git invocation.
    Unobserved { subject: Subject },
    NoStatusView { subject: Subject },
    Conflicting { subject: Subject },
    /// A subject of a kind no cell is made for.
    Kind { subject: Subject },
    /// A path that is not checkout-relative (absolute, or with a `.` or `..` part).
    Path { subject: Subject },
    /// More reads than [`READS_CAP`].
    TooMany { reads: usize, subject: Subject },
}

impl NoCell {
    pub(crate) fn first_rejected(&self) -> (&'static str, &str) {
        match self {
            Self::Git { subject } => ("Git", subject),
            Self::Unobserved { subject } => ("Unobserved", subject),
            Self::NoStatusView { subject } => ("NoStatusView", subject),
            Self::Conflicting { subject } => ("Conflicting", subject),
            Self::Kind { subject } => ("Kind", subject),
            Self::Path { subject } => ("Path", subject),
            Self::TooMany { subject, .. } => ("TooMany", subject),
        }
    }
}

impl core::fmt::Display for NoCell {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NoCell::Git { subject } => {
                f.write_str("it asked git `")?;
                for (at, part) in subject.split('\n').enumerate() {
                    if at > 0 {
                        f.write_str(" ")?;
                    }
                    f.write_str(part)?;
                }
                f.write_str("`: malformed or unsupported git observation (engine_script_reads.rs `observed`)")
            }
            NoCell::Unobserved { subject } => write!(f, "the sandbox supplied no answer for {subject:?}"),
            NoCell::NoStatusView { subject } => write!(f, "{subject:?} requires the granted checkout's git status reader"),
            NoCell::Conflicting { subject } => write!(f, "the sandbox observed two answers for {subject:?} during this run"),
            NoCell::Kind { subject } => write!(f, "it read `{subject}`, a kind of read no cell is made for (engine_script_reads.rs `observed`)"),
            NoCell::Path { subject } => write!(f, "it read `{subject}`, a path that is not relative to the checkout (engine_script_reads.rs `observed`)"),
            NoCell::TooMany { reads, subject } => write!(f, "observation {reads}, {subject:?}, exceeds the {READS_CAP} one run's record keeps"),
        }
    }
}

/// Only a command with the exact replay reader can keep a status result.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct StatusView(());

/// The bug a test plants in [`collect_reads`], to show its test catches it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ReadsPlant {
    None,
    /// Every `file:` read is dropped: an edit of a file the script read is missed.
    FileDropped,
    #[cfg(test)]
    StatusAlwaysOwed,
}

/// One recorded read as its cell: `digest` is the sandbox's answer, `None` for nothing there.
fn observed<'s>(subject: &'s str, digest: Option<ContentId>, alloc: SharedAlloc<'static>) -> Result<(ReadKind, &'s str), NoCell> {
    if let Some(changed) = subject.strip_prefix("changed-during-run:") {
        return Err(NoCell::Conflicting { subject: Subject::from_str_in(changed, alloc) });
    }
    let owned = || Subject::from_str_in(subject, alloc);
    let Some((kind, path)) = subject.split_once(':') else { return Err(NoCell::Kind { subject: owned() }) };
    let kind = match kind {
        "file" => ReadKind::File,
        "dir" => ReadKind::Listing,
        "stat" => ReadKind::Stat,
        "git-scope" => {
            if !relative(path) || path.contains('\n') {
                return Err(NoCell::Path { subject: owned() });
            }
            if digest.is_none() {
                return Err(NoCell::Unobserved { subject: owned() });
            }
            ReadKind::Git
        }
        "git" => {
            // `<folder>\n<words>` (script_sandbox.rs `git_subject`), the folder checkout-relative.
            let Some((folder, command)) = path.split_once('\n') else { return Err(NoCell::Git { subject: owned() }) };
            if !relative(folder) {
                return Err(NoCell::Path { subject: owned() });
            }
            let mut words = [""; MOST_GIT_WORDS];
            let Some(count) = git_words(command, &mut words) else { return Err(NoCell::Git { subject: owned() }) };
            if matches!(git::question(&words[..count]), git::Question::Other | git::Question::Dirty | git::Question::Short | git::Question::Log(Err(_))) {
                return Err(NoCell::Git { subject: owned() });
            }
            if digest.is_none() {
                return Err(NoCell::Unobserved { subject: owned() });
            }
            ReadKind::Git
        }
        _ => return Err(NoCell::Kind { subject: owned() }),
    };
    // A git question's rest is no path: its folder was checked above.
    if kind != ReadKind::Git && !relative(path) {
        return Err(NoCell::Path { subject: owned() });
    }
    Ok((kind, path))
}

#[cfg(test)]
pub(crate) fn parse(subject: &str, digest: Option<ContentId>) -> Result<CheckoutRead, NoCell> {
    let alloc = blaze_own::testing::heap().shared();
    let (kind, path) = observed(subject, digest, alloc)?;
    Ok(CheckoutRead { kind, path: Subject::from_str_in(path, alloc), answer: digest.unwrap_or(NONE) })
}

/// Whether `path` is checkout-relative: `/`-separated, no empty, `.` or `..` part ("" the root).
fn relative(path: &str) -> bool {
    !path.contains('\0') && !path.starts_with('/') && (path.is_empty() || path.split('/').all(|part| !part.is_empty() && part != "." && part != ".."))
}

/// The most words of a git question a cell holds (`log -1 --pretty=format:..` is three); past it
/// the read has no cell (rule 18).
const MOST_GIT_WORDS: usize = 16;

/// `command`'s words, split at each space as the sandbox joined them (script_sandbox.rs
/// `git_subject`, `answer_now`), into `into`; their count, `None` past [`MOST_GIT_WORDS`].
fn git_words<'s>(command: &'s str, into: &mut [&'s str; MOST_GIT_WORDS]) -> Option<usize> {
    let mut count = 0;
    for word in command.split(' ') {
        if word.is_empty() || word.contains(char::is_whitespace) || word.contains('\0') {
            return None;
        }
        *into.get_mut(count)? = word;
        count += 1;
    }
    Some(count)
}

/// A run's recorded reads as cells, each once at its first read (the sandbox records each
/// subject once already); the first that has none refuses the whole list.
#[cfg(test)]
pub(crate) fn cells_of<'s>(reads: impl Iterator<Item = (&'s str, Option<ContentId>)>, plant: ReadsPlant) -> Result<ReadList, NoCell> {
    let mut cells = ReadList::new_in(blaze_own::testing::heap().shared());
    collect_reads(reads, plant, None, &mut cells)?;
    Ok(cells)
}

/// Append to the worker's result buffer. The caller discards the whole list on
/// a refusal, including any prefix collected before the rejected observation.
pub(crate) fn collect_reads<'s>(reads: impl Iterator<Item = (&'s str, Option<ContentId>)>, plant: ReadsPlant, view: Option<StatusView>, cells: &mut ReadList) -> Result<(), NoCell> {
    let alloc = *cells.allocator();
    for (at, (subject, digest)) in reads.enumerate() {
        check_count(at, subject, alloc)?;
        let (kind, path) = observed(subject, digest, alloc)?;
        if kind == ReadKind::Git && path.split_once('\n').is_none_or(|(_, command)| reads_status(command)) {
            #[cfg(test)]
            if plant == ReadsPlant::StatusAlwaysOwed && path.contains('\n') {
                return Err(NoCell::NoStatusView { subject: Subject::from_str_in(subject, alloc) });
            }
            if view.is_none() {
                return Err(NoCell::NoStatusView { subject: Subject::from_str_in(subject, alloc) });
            }
        }
        if plant == ReadsPlant::FileDropped && kind == ReadKind::File {
            continue;
        }
        cells.push(CheckoutRead { kind, path: Subject::from_str_in(path, alloc), answer: digest.unwrap_or(NONE) });
    }
    Ok(())
}

/// Whether the git question `command` is answered from the worktree's status (`status`, and
/// `describe --dirty`, whose mark is the tracked status's): only the served checkout's sweep
/// replays it ([`StatusView`]).
pub(crate) fn reads_status(command: &str) -> bool {
    command.starts_with("status ") || (command.starts_with("describe ") && command.split(' ').any(|word| word == "--dirty" || word == "--dirty=-dirty"))
}

fn check_count(at: usize, subject: &str, alloc: SharedAlloc<'static>) -> Result<(), NoCell> {
    if at == READS_CAP {
        return Err(NoCell::TooMany { reads: at + 1, subject: Subject::from_str_in(subject.strip_prefix("read-limit:").unwrap_or(subject), alloc) });
    }
    Ok(())
}

/// A fetched reader's own tree is pinned. Its dependency mounts either stand
/// on their producer records or are refused at grant time. An unknown subject
/// still refuses the run: only validated tree reads are covered by the pin.
pub(crate) fn pinned_reads<'s>(reads: impl Iterator<Item = (&'s str, Option<ContentId>)>, alloc: SharedAlloc<'static>) -> Result<(), NoCell> {
    for (at, (subject, digest)) in reads.enumerate() {
        check_count(at, subject, alloc)?;
        observed(subject, digest, alloc)?;
    }
    Ok(())
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

/// The `Given` cell and answer of a checkout file a BODY read as data with no `FileItems` record
/// of today's plan to define it (a computed `include_str!(concat!(..))`, which data_include.rs
/// cannot list from the text): the SAME read as a build script's `file:` read of that path, one
/// cell and one leaf row whoever read it (rule 30), followed by the sweep ([`now`]) and planned
/// as its `Given` task ([`plan`]) from the next request on. Its answer is what the body was
/// handed (`bytes`), as the sandbox digests a file ([`script_sandbox::file_answer`]), [`NONE`]
/// when it had nothing. `None`: the path is no checkout-relative one, so no cell names it.
pub(crate) fn file_read(path: &str, bytes: Option<&[u8]>) -> Option<(Name, ContentId)> {
    (relative(path) && !path.is_empty()).then(|| (cell_name(ReadKind::File, path), bytes.map_or(NONE, script_sandbox::file_answer)))
}

/// Pushes onto `out` the leaf rows of the checkout reads the answer still reads: each followed
/// row whose cell `kept_cells` says a row still reads, then each read of this request (`fresh`:
/// its scripts' runs and its bodies' computed data includes, [`file_read`]) not among them. A
/// read no row reads any more leaves the table, so its task is no longer planned and is retired
/// (rule 18: the followed set is bounded by the reads the answer holds).
pub(crate) fn keep_rows<'r>(followed: Option<&Rows<'_>>, fresh: impl Iterator<Item = (ReadKind, &'r str, ContentId)>, kept_cells: impl Fn(Name) -> bool, out: &mut Rows<'_>) {
    keep_rows_with(followed, core::iter::empty(), fresh, false, |name, _| kept_cells(name), |_, _| false, |_, _, _| None, out);
}

/// Faster [`keep_rows`] for the command tail: `fresh_scripts` are reads recorded by kept,
/// succeeded build scripts that ran in this request (so each is guaranteed to have a reader in
/// `finished.answer` without querying `LastAnswer::readers`), deduplicated by `(kind, path)`
/// before hashing. Preserves [`crate::sweep::DIRECT_PATH`] on followed rows and marks newly
/// observed direct non-symlink paths via `is_direct`; stamps freshly recorded meta-only rows
/// via `stamp_of`.
#[expect(clippy::too_many_arguments, reason = "the tail's script-row merger takes its inputs by name")]
pub(crate) fn keep_rows_with<'r>(
    followed: Option<&Rows<'_>>,
    fresh_scripts: impl Iterator<Item = (ReadKind, &'r str, ContentId)>,
    more: impl Iterator<Item = (ReadKind, &'r str, ContentId)>,
    followed_all_kept: bool,
    kept_cells: impl Fn(Name, bool) -> bool,
    is_direct: impl Fn(ReadKind, &str) -> bool,
    stamp_of: impl Fn(ReadKind, &str, ContentId) -> Option<Stamp>,
    out: &mut Rows<'_>,
) {
    let alloc = *out.rows.allocator();
    let mut fresh_unique: blaze_own::BTreeMap<(ReadKind, &'r str), ContentId, _> = blaze_own::BTreeMap::new_in(alloc);
    for (kind, path, answer) in fresh_scripts {
        fresh_unique.entry((kind, path)).or_insert(answer);
    }
    let mut seen: blaze_own::BTreeSet<(ReadKind, &str), _> = blaze_own::BTreeSet::new_in(alloc);
    let mut seen_more: blaze_own::BTreeSet<Name, _> = blaze_own::BTreeSet::new_in(alloc);
    for followed in followed.into_iter() {
        for leaf in followed.rows.iter() {
            let Some(kind) = ReadKind::of_leaf(leaf.kind) else { continue };
            let path = followed.path(leaf);
            if seen.contains(&(kind, path)) {
                continue;
            }
            let known = followed_all_kept || fresh_unique.contains_key(&(kind, path));
            if !known {
                let name = cell_name(kind, path);
                if !kept_cells(name, true) {
                    continue;
                }
                seen_more.insert(name);
            }
            seen.insert((kind, path));
            let flags = if leaf.flags & crate::sweep::DIRECT_PATH == 0 && is_direct(kind, path) {
                leaf.flags | crate::sweep::DIRECT_PATH
            } else {
                leaf.flags
            };
            let stamp = leaf.stamp.or_else(|| stamp_of(kind, path, leaf.id));
            out.push(leaf.kind, flags, path, stamp, leaf.id);
        }
    }
    for ((kind, path), answer) in fresh_unique.iter() {
        if seen.contains(&(*kind, *path)) {
            continue;
        }
        seen.insert((*kind, *path));
        let flags = if is_direct(*kind, *path) { crate::sweep::DIRECT_PATH } else { 0 };
        let stamp = stamp_of(*kind, *path, *answer);
        out.push(kind.leaf(), flags, path, stamp, *answer);
    }
    for (kind, path, answer) in more {
        if seen.contains(&(kind, path)) {
            continue;
        }
        let name = cell_name(kind, path);
        if seen_more.contains(&name) || !kept_cells(name, false) {
            continue;
        }
        seen.insert((kind, path));
        seen_more.insert(name);
        let flags = if is_direct(kind, path) { crate::sweep::DIRECT_PATH } else { 0 };
        let stamp = stamp_of(kind, path, answer);
        out.push(kind.leaf(), flags, path, stamp, answer);
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
    // No stamp proves a git answer: a commit moves `.git` files no row of it follows.
    if kind == ReadKind::Git {
        return Ok((0, None, git_now(cx, root, path, frame)?));
    }
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
        // Answered above, before any stat of its rest (which is no path).
        (ReadKind::Git, _) => Ok((0, None, git_now(cx, root, path, frame)?)),
        (ReadKind::Stat, _) => Ok((0, stamp, script_sandbox::stat_answer(is_dir, meta.as_ref().map_or(0, |meta| meta.len)))),
        (ReadKind::File, _) if is_dir => Ok((0, stamp, NOT_A_FILE)),
        (ReadKind::Listing, _) if !is_dir => Ok((0, stamp, NOT_A_FOLDER)),
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

/// The settled stamp of `path` under `root`, when `path` is a non-empty relative path that
/// currently exists and its stat is settled under `stamps` and `plant`.
pub(crate) fn settled_path_stamp(cx: &Cx, root: &Dir<ReadWrite>, stamps: &impl Stamps, plant: Plant, path: &str) -> Option<Stamp> {
    if path.is_empty() || !relative(path) {
        return None;
    }
    let path_obj = Path::new(path).ok()?;
    let meta = root.stat(cx, &path_obj).ok()??;
    match plant {
        Plant::RacyTrusted => stamps.stamp(path, &meta),
        Plant::None | Plant::AbsentForgotten => stamps.stamp(path, &meta).filter(|it| it.settled(stamps.swept_at_ns(), stamps.granularity_ns())),
    }
}

/// A settled stamp proving `answer` from a single `stat(path)` alone (zero TOCTOU):
/// - `ReadKind::Stat`: `stat_answer(meta.dir, meta.len) == answer`
/// - `ReadKind::File`: `meta.dir && answer == NOT_A_FILE`
/// - `ReadKind::Listing`: `!meta.dir && answer == NOT_A_FOLDER`
/// Returns `None` if `path` is empty, non-relative, missing, unsettled, or disagrees with `answer`.
pub(crate) fn settled_meta_stamp(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    stamps: &impl Stamps,
    plant: Plant,
    kind: ReadKind,
    path: &str,
    answer: ContentId,
) -> Option<Stamp> {
    if path.is_empty() || !relative(path) || answer == NONE || answer == DENIED {
        return None;
    }
    let is_meta_only = match kind {
        ReadKind::Stat => true,
        ReadKind::File => answer == NOT_A_FILE,
        ReadKind::Listing => answer == NOT_A_FOLDER,
        ReadKind::Git => return None,
    };
    if !is_meta_only {
        return None;
    }
    let path_obj = Path::new(path).ok()?;
    let meta = root.stat(cx, &path_obj).ok()??;
    let matches = match kind {
        ReadKind::Stat => script_sandbox::stat_answer(meta.dir, meta.len) == answer,
        ReadKind::File => meta.dir,
        ReadKind::Listing => !meta.dir,
        ReadKind::Git => false,
    };
    if !matches {
        return None;
    }
    match plant {
        Plant::RacyTrusted => stamps.stamp(path, &meta),
        Plant::None | Plant::AbsentForgotten => stamps.stamp(path, &meta).filter(|it| it.settled(stamps.swept_at_ns(), stamps.granularity_ns())),
    }
}

/// The `dir:` listing digest of the checkout root `""`, for post-`EV_ADD` verification of root
/// `ScriptListing` leaves in `daemon_watch`.
#[cfg(target_os = "macos")]
pub(crate) fn root_listing_id(cx: &Cx, root: &Dir<ReadWrite>, frame: &Frame) -> Result<ContentId, CliError> {
    listing(cx, root, "", frame)
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

/// The checkout the command's build scripts are served from, as its edge made it: a value
/// handed down (engine_check.rs `Place`, fetch.rs `FetchEnv`), never asked again.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Served<'f> {
    /// The checkout's root, absolute, every link resolved.
    Root(&'f script_sandbox::RealRoot),
    /// A simulated world: no checkout on disk to serve.
    Simulated,
}

impl<'f> Served<'f> {
    /// The ONE resolution of the checkout's root a command makes, at its edge: the checkout
    /// relative to the folder the process was started in, resolved by one `realpath`
    /// ([`script_sandbox::RealRoot::of_start`]). No environment variable is read (`PWD` is
    /// ambient and may be stale or unset, rule 75). `Err` names the path and the OS's error.
    pub(crate) fn at_edge(simulated: bool, checkout: &str, frame: &'f Frame) -> Result<Served<'f>, script_sandbox::RootUnresolved> {
        if simulated {
            return Ok(Served::Simulated);
        }
        let root = script_sandbox::RealRoot::of_start(crate::scripts::checkout_path(checkout), blaze_own::planted::NoPlant)?;
        Ok(Served::Root(blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(root, frame))))
    }

    /// The root as text, for what prints it or names a folder from it (engine_check.rs's shown
    /// root): `None` in a simulated world, or where the path is not UTF-8.
    pub(crate) fn root_text(self) -> Option<&'f str> {
        match self {
            Served::Root(root) => root.path().to_str(),
            Served::Simulated => None,
        }
    }

    /// The same root, held in `frame` (a state that outlives the edge's borrow): a copy of the
    /// resolved path, never a second resolution.
    pub(crate) fn within<'g>(self, frame: &'g Frame) -> Served<'g> {
        match self {
            Served::Root(root) => Served::Root(blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(root.clone(), frame))),
            Served::Simulated => Served::Simulated,
        }
    }
}

/// The served checkout for a sweep. Discovery binds the asking folder to a
/// worktree. All scripts bound there share a row per exact status question.
///
/// **A status after a change (E3).** A `status --porcelain` question (and the status under a
/// `describe --dirty`) is answered from the answer the last command kept and the paths the
/// checkout's journal named since its place (blaze-build-script git.rs `status_after`), not by a
/// walk of the worktree; where that is not covered the status is computed whole, and the
/// profile says why ([`StatusCounts`]). What is kept is two facts, stored apart:
///
/// - the ANSWER record: the bytes a script observes (the git answer's digest preimage,
///   blaze-build-script git.rs `Answer::digest_bytes`: exit code 0, the porcelain text, no
///   stderr) under `Key(ContentId::of(STATUS_ANSWER_DOMAIN, cell digest))`. The cell digest is
///   the digest of those very bytes, so one key names one value (rule 21): two different texts
///   have two digests, two keys. Its bytes are checked against the digest when read
///   ([`status_text`]).
/// - the COVERAGE record ([`COVERAGE_MAGIC`], versioned, rule 21): the journal's place this
///   command took before reading any file, and per status question (its key: worktree and
///   untracked mode, [`status_key`]) the answer's digest, HEAD's tree and the `text=auto` it
///   was computed under. It is hashed into no cell and no task's key, so it can never make a
///   cell or record hold two values: it only decides whether the next command may start from
///   the answer record, and a wrong decision there is refused by git.rs `status_after` (the
///   place must equal the replay's start, `since`). Its key is the digest of its bytes.
///
/// Two roots name them, replaced by each command that answered a status question at a journal
/// place ([`CheckoutSweep::keep`]): `<answer root>.status` ([`status_root`]) names the coverage
/// record ALONE, and `<answer root>.status.answers` ([`answers_root`]) names one answer record
/// per status question the answer reads (at most [`MOST_KEPT`]), so neither root's reach grows.
/// The coverage is found by its root holding one key, as every single-record root is read
/// (engine_answer.rs `load_pointer`, journal_checkpoint.rs `read`), never by its place among
/// several: a root's keys are stored SORTED (blaze-cas plain/core.rs `sorted_keys`), so until
/// 2026-11-21, when one root named the coverage and its answers and the coverage was read as
/// "the first key", the coverage was first only when its key happened to sort below every
/// answer's. The coverage's key moves with the journal's place, so whether the next command
/// found it was a coin per command: tempo's profiles showed "previous answers none: a record of
/// another form" (an answer record, read as the coverage) and 383 folders listed on about half
/// of the commands (score-c0220wave84 to c0255wave87). A lapsed root, a missing record, or a
/// record of another form is exactly no previous answer ([`git::WholeWhy::NoPrevious`], the
/// reason printed: [`NoCoverage`]), never an error.
pub(crate) struct CheckoutSweep<'t, 'f> {
    tree: script_sandbox::Tree,
    /// Canonical checkout root path when backed by a real disk tree (`Served::Root`), used to
    /// verify that nonexistent script paths traverse no symlink (`absent_direct`).
    root_abs: Option<&'f str>,
    /// Each status question asked this command, by its key ([`status_key`]): the answer's
    /// digest (the cell's), and the answer.
    status: blaze_own::BTreeMap<ContentId, (ContentId, StatusCell<'t>), &'f Frame>,
    /// The answers the last command kept, by question key (sorted), or why there are none.
    previous: Result<AVec<'t, (ContentId, git::StatusAnswer<'t>)>, NoCoverage>,
    /// The checkout's journal this command ([`CheckoutSweep::prepare`]); `None`: no place.
    journal: Option<JournalNow<'t>>,
    counts: StatusCounts,
    /// The bug a control plants in [`CheckoutSweep::keep`].
    plant: CoveragePlant,
    scratch: Alloc<'t>,
}

/// The checkout's journal as a status after a change takes it: this command's place, and the
/// paths it named since the stored answer's place, from that place (journal_sweep.rs
/// `checkout_changes`), when its replay was judged a continuation.
struct JournalNow<'t> {
    now: git::Gen,
    covered: Option<(git::Gen, AVec<'t, AString<'t>>)>,
    /// A folder that ignored every untracked name all through the window (journal_sweep.rs
    /// `CheckoutChanges::ignored_whole`: blaze's own), handed to the proof
    /// (`git::CoveredSince::ignoring`).
    ignored: Option<&'static str>,
}

/// One status question's answer this command.
enum StatusCell<'t> {
    /// Computed at the journal's place this command: kept for the next.
    Placed(git::StatusAnswer<'t>),
    /// Computed with no place of the checkout's journal (none on this OS, the checkout no one
    /// unit, its replay failed): right for this command, never kept.
    Unplaced(AString<'t>),
    /// Refused by git.rs: the cell is [`NONE`], as the sandbox's run records it.
    Refused,
}

impl StatusCell<'_> {
    fn text(&self) -> Option<&str> {
        match self {
            StatusCell::Placed(answer) => Some(answer.text.as_str()),
            StatusCell::Unplaced(text) => Some(text.as_str()),
            StatusCell::Refused => None,
        }
    }
}

/// The coverage record's form (rule 21): another form is no coverage, said so
/// ([`NoCoverage::OtherForm`]), and the status is computed whole.
pub(crate) const COVERAGE_MAGIC: &[u8; 4] = b"BSC1";
const COVERAGE_DOMAIN: &str = "blaze-cli status coverage 1";
/// An answer record's key, from the status cell's digest (the digest of its bytes).
const STATUS_ANSWER_DOMAIN: &str = "blaze-cli status answer record 1";
/// The magic, the place (the journal's identity, the offset), the count of entries.
const COVERAGE_HEADER: usize = 4 + 24 + 8 + 4;
/// One entry: the question's key, the answer's digest, HEAD's tree, the flags.
const COVERAGE_ENTRY: usize = 32 + 32 + 20 + 1;
const UNTRACKED: u8 = 1;
const ALL: u8 = 2;
const TEXT_AUTO: u8 = 4;
/// The most status questions one checkout's coverage keeps (rule 18): one a repository and
/// untracked mode the answer's scripts ask; past it none is kept, said so ([`NotKept`]).
pub(crate) const MOST_KEPT: usize = 64;

/// The root naming a checkout's status coverage record, alone, beside its answer's root.
pub(crate) fn status_root<'f>(answer_root: &str, frame: &'f Frame) -> blaze_own::String<&'f Frame> {
    let mut name = blaze_own::String::new_in(frame);
    name.push_str(answer_root);
    name.push_str(".status");
    name
}

/// The root naming the answer records the coverage under [`status_root`] lists: it keeps them
/// in the store; it is never read for which record is which (the coverage names each by its
/// digest).
pub(crate) fn answers_root<'f>(answer_root: &str, frame: &'f Frame) -> blaze_own::String<&'f Frame> {
    let mut name = status_root(answer_root, frame);
    name.push_str(".answers");
    name
}

/// A status question's key: its worktree and both switches (the collapsed-directory form and
/// the all-files form are different answers even when both include untracked paths).
fn status_key(work: &str, untracked: bool, all: bool) -> ContentId {
    let mut key = blaze_ids::Hasher::new("blaze script status question 1");
    key.part(work.as_bytes()).update(&[u8::from(untracked), u8::from(all)]);
    key.finish()
}

fn status_answer_key(digest: ContentId) -> Key {
    Key(ContentId::of(STATUS_ANSWER_DOMAIN, &digest.0))
}

/// A status `text` as the git answer a script observes: exit code 0, the text, no stderr.
fn status_answer<'a>(text: AString<'a>) -> git::Answer<'a> {
    git::Answer { code: 0, stdout: text, stderr: "" }
}

/// The text of a status answer record, `None` unless its bytes are the preimage of `digest` and
/// the form a status answer has (code 0, UTF-8 text, no stderr).
fn status_text(bytes: &[u8], digest: ContentId) -> Option<&str> {
    if script_sandbox::file_answer(bytes) != digest {
        return None;
    }
    let code = i32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
    let len = usize::try_from(u64::from_le_bytes(bytes.get(4..12)?.try_into().ok()?)).ok()?;
    let text = bytes.get(12..12usize.checked_add(len)?)?;
    let rest = bytes.get(12 + len..)?;
    if code != 0 || rest != 0u64.to_le_bytes() {
        return None;
    }
    core::str::from_utf8(text).ok()
}

/// Why no status answer of the last command is started from: each is exactly no previous
/// answer ([`git::WholeWhy::NoPrevious`]), never an error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NoCoverage {
    /// The sweep was made without its command's store (a test of the sweep alone).
    NotRead,
    /// No root `<answer root>.status`: the first command, a lapsed root, or no command before
    /// answered a status question at a journal place.
    NoRoot,
    /// A record the root names is not in the store.
    Missing { key: Key },
    /// The coverage record is of another form (an older blaze).
    OtherForm,
    /// The root or the coverage record is not of the length its form says.
    Malformed { bytes: usize },
    /// An answer record whose bytes are not its digest's, or not a status answer's form.
    AnswerDiffers { key: Key },
}

impl core::fmt::Display for NoCoverage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NoCoverage::NotRead => f.write_str("not read (no store)"),
            NoCoverage::NoRoot => f.write_str("no `.status` root (a first command, or it lapsed)"),
            NoCoverage::Missing { key } => write!(f, "the record {key:?} it names is gone"),
            NoCoverage::OtherForm => f.write_str("a record of another form"),
            NoCoverage::Malformed { bytes } => write!(f, "{bytes} bytes, not its form's length"),
            NoCoverage::AnswerDiffers { key } => write!(f, "the answer record {key:?} is not its digest's bytes"),
        }
    }
}

/// Why this command's status answers were not kept for the next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NotKept {
    /// The answer was not committed (yet): no keep was asked.
    NotAsked,
    /// The checkout's journal gave no place this command: an answer bound to none could not be
    /// started from.
    NoPlace,
    /// More status questions than [`MOST_KEPT`].
    TooMany { questions: u32 },
}

impl core::fmt::Display for NotKept {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NotKept::NotAsked => f.write_str("no commit asked"),
            NotKept::NoPlace => f.write_str("the checkout's journal gave no place"),
            NotKept::TooMany { questions } => write!(f, "{questions} status questions, over {MOST_KEPT}"),
        }
    }
}

/// How a command's status answers were made: the profile's status line, and the tests'.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct StatusCounts {
    /// The last command's answers read: how many, or why none.
    pub(crate) coverage: Result<u32, NoCoverage>,
    /// Status questions answered this command.
    pub(crate) questions: u32,
    /// From the previous answer and the journal's names (git `Made::Incremental`), and the
    /// regions they recomputed in all.
    pub(crate) incremental: u32,
    pub(crate) regions: u32,
    /// Computed whole, and the first one's reason.
    pub(crate) whole: u32,
    pub(crate) first_whole: Option<git::WholeWhy>,
    /// Computed whole with no journal place: right for this command, never kept.
    pub(crate) unplaced: u32,
    /// Refused by git.rs (the cell is [`NONE`]).
    pub(crate) refused: u32,
    /// Folders listed and ignore files read by the git answers this command (the status's, and a
    /// `describe`'s), through the served tree's one reader (script_sandbox.rs `GitView`).
    pub(crate) lists: u32,
    pub(crate) ignore_reads: u32,
    /// Kept for the next command: how many answers, or why none.
    pub(crate) kept: Result<u32, NotKept>,
}

impl StatusCounts {
    const NONE: StatusCounts = StatusCounts {
        coverage: Err(NoCoverage::NotRead),
        questions: 0,
        incremental: 0,
        regions: 0,
        whole: 0,
        first_whole: None,
        unplaced: 0,
        refused: 0,
        lists: 0,
        ignore_reads: 0,
        kept: Err(NotKept::NotAsked),
    };

    fn asked(&mut self, counts: script_sandbox::GitCounts) {
        self.lists = self.lists.saturating_add(counts.lists);
        self.ignore_reads = self.ignore_reads.saturating_add(counts.ignore_reads);
    }
}

impl core::fmt::Display for StatusCounts {
    /// `status: Q question(s): I from the previous answer (R region(s)), W whole (first: why), ...`
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "status: {} question(s): {} from the previous answer and the journal's names ({} region(s) recomputed), {} whole", self.questions, self.incremental, self.regions, self.whole)?;
        if let Some(why) = self.first_whole {
            write!(f, " (first: {why})")?;
        }
        write!(f, ", {} with no journal place, {} refused; {} folder(s) listed, {} ignore file(s) read (floor after an edit of tracked files: 0, 0); previous answers ", self.unplaced, self.refused, self.lists, self.ignore_reads)?;
        match self.coverage {
            Ok(read) => write!(f, "read: {read}")?,
            Err(why) => write!(f, "none: {why}")?,
        }
        match self.kept {
            Ok(kept) => write!(f, "; kept {kept}"),
            Err(why) => write!(f, "; none kept: {why}"),
        }
    }
}

/// The bug a control plants in [`CheckoutSweep::keep`] (blaze_own::planted, D165).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CoveragePlant {
    None,
    /// The coverage is written and never named by the root: the next command reads none, and
    /// every status is computed whole.
    Dropped,
}

impl blaze_own::planted::Plants for CoveragePlant {}

/// The answers the last command kept ([`CheckoutSweep::keep`]), by question key, copied into
/// `scratch`; or why none is started from (never an error: a store failure alone is one).
fn read_coverage<'t, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'_, '_, D, S>, answer_root: &str, scratch: Alloc<'t>, frame: &Frame) -> Result<Result<AVec<'t, (ContentId, git::StatusAnswer<'t>)>, NoCoverage>, CliError> {
    let root = status_root(answer_root, frame);
    let Some(keys) = store.named(root.as_str())? else { return Ok(Err(NoCoverage::NoRoot)) };
    // One key: the coverage alone (the struct's doc). A root of several keys is the form before
    // 2026-11-21 (the coverage beside its answers, sorted): another form, computed whole once.
    let Ok(one) = <[u8; 32]>::try_from(keys) else { return Ok(Err(NoCoverage::Malformed { bytes: keys.len() })) };
    let key = Key(ContentId(one));
    let Some(held) = store.get(cx, key)? else { return Ok(Err(NoCoverage::Missing { key })) };
    let bytes = held.bytes;
    if bytes.get(..4) != Some(COVERAGE_MAGIC.as_slice()) {
        return Ok(Err(NoCoverage::OtherForm));
    }
    let malformed = NoCoverage::Malformed { bytes: bytes.len() };
    let Some(header) = bytes.get(..COVERAGE_HEADER) else { return Ok(Err(malformed)) };
    let journal: [u8; 24] = header[4..28].try_into().expect("24 bytes");
    let at = u64::from_le_bytes(header[28..36].try_into().expect("8 bytes"));
    let count = u32::from_le_bytes(header[36..40].try_into().expect("4 bytes")) as usize;
    if count > MOST_KEPT || bytes.len() != COVERAGE_HEADER + count * COVERAGE_ENTRY {
        return Ok(Err(malformed));
    }
    let place = git::Gen { journal, at };
    let mut kept: AVec<'t, (ContentId, git::StatusAnswer<'t>)> = blaze_own::Vec::with_capacity_in(count, scratch);
    for entry in bytes[COVERAGE_HEADER..].chunks_exact(COVERAGE_ENTRY) {
        let question = ContentId(entry[..32].try_into().expect("32 bytes"));
        let digest = ContentId(entry[32..64].try_into().expect("32 bytes"));
        let tree = git::Oid(entry[64..84].try_into().expect("20 bytes"));
        let flags = entry[84];
        // Sorted, each once (as `keep` writes them): read by binary search.
        if flags & !(UNTRACKED | ALL | TEXT_AUTO) != 0 || kept.last().is_some_and(|(last, _)| *last >= question) {
            return Ok(Err(malformed));
        }
        let record = status_answer_key(digest);
        let Some(held) = store.get(cx, record)? else { return Ok(Err(NoCoverage::Missing { key: record })) };
        let Some(text) = status_text(held.bytes, digest) else { return Ok(Err(NoCoverage::AnswerDiffers { key: record })) };
        let answer = git::StatusAnswer {
            text: AString::from_str_in(text, scratch),
            untracked: flags & UNTRACKED != 0,
            all: flags & ALL != 0,
            text_auto: flags & TEXT_AUTO != 0,
            tree,
            place,
        };
        kept.push((question, answer));
    }
    Ok(Ok(kept))
}

/// Names the coverage record alone under [`status_root`] and its answer records under
/// [`answers_root`], unless the plant drops them.
fn pin_coverage<D: Driver, S: Sharing>(store: &mut CommandStore<'_, '_, D, S>, answer_root: &str, coverage: Key, answers: &[Key], plant: impl blaze_own::planted::Planted<CoveragePlant>, frame: &Frame) -> Result<(), CliError> {
    if plant.has(CoveragePlant::Dropped) {
        return Ok(());
    }
    store.pin(answers_root(answer_root, frame).as_str(), answers)?;
    store.pin(status_root(answer_root, frame).as_str(), &[coverage])
}

impl<'t, 'f> CheckoutSweep<'t, 'f> {
    pub(crate) fn new(tree: script_sandbox::Tree, scratch: Alloc<'t>, frame: &'f Frame) -> Self {
        Self {
            tree,
            root_abs: None,
            status: blaze_own::BTreeMap::new_in(frame),
            previous: Err(NoCoverage::NotRead),
            journal: None,
            counts: StatusCounts::NONE,
            plant: CoveragePlant::None,
            scratch,
        }
    }

    /// The command's served checkout, at the root its edge resolved once ([`Served::at_edge`]);
    /// a simulated world has none.
    pub(crate) fn for_command(served: Served<'_>, scratch: Alloc<'t>, frame: &'f Frame) -> Option<Self> {
        #[cfg(test)]
        if let Some(tree) = crate::engine_scripts::runner_tree() {
            return Some(Self { plant: crate::engine_scripts::runner_coverage_plant(), ..Self::new(tree, scratch, frame) });
        }
        match served {
            Served::Root(root) => {
                let root_abs = root.path().to_str().map(|s| &*frame.alloc_str(s));
                Some(Self { root_abs, ..Self::new(script_sandbox::dir_tree(root), scratch, frame) })
            }
            Served::Simulated => None,
        }
    }

    /// Before any status is asked: the checkout journal's place this command and what it named
    /// since the stored answer's (journal_sweep.rs `checkout_place`, `checkout_changes`), and
    /// the answers the last command kept (`<answer root>.status`). Every command calls it, the
    /// first too: its answers are kept at its place, and the next starts from them.
    pub(crate) fn prepare<D: Driver, S: Sharing>(&mut self, cx: &Cx, store: &mut CommandStore<'_, '_, D, S>, answer_root: &str, journal: &Taken<'_>, frame: &Frame) -> Result<(), CliError> {
        let scratch = self.scratch;
        self.journal = journal.checkout_place().map(|now| {
            let mut ignored = None;
            let covered = journal.checkout_changes(frame).filter(|changes| changes.now == now).map(|changes| {
                let mut changed: AVec<'t, AString<'t>> = blaze_own::Vec::with_capacity_in(changes.changed.len(), scratch);
                changed.extend(changes.changed.iter().map(|path| AString::from_str_in(path, scratch)));
                ignored = changes.ignored_whole;
                (changes.since.gen_of(), changed)
            });
            JournalNow { now: now.gen_of(), covered, ignored }
        });
        self.previous = read_coverage(cx, store, answer_root, scratch, frame)?;
        self.counts.coverage = match &self.previous {
            Ok(kept) => Ok(u32::try_from(kept.len()).unwrap_or(u32::MAX)),
            Err(why) => Err(*why),
        };
        Ok(())
    }

    /// How this command's status answers were made and kept (the profile's line).
    pub(crate) fn counts(&self) -> StatusCounts {
        StatusCounts { questions: u32::try_from(self.status.len()).unwrap_or(u32::MAX), ..self.counts }
    }

    /// After the answer: each status question the answer's rows read (`rows`, its leaf table),
    /// computed now where the sweep did not ask it (a first command, a script asking first),
    /// kept for the next command under `<answer_root>.status`, replacing the last command's.
    /// Called after the one commit call both drivers share (engine_check.rs: `save_edit` or
    /// `save_changes`, then this), and where a certificate stood, before it is renewed: the
    /// place moves with every command that answers, so a no-op between two edits does not leave
    /// the next one uncovered.
    pub(crate) fn keep<D: Driver, S: Sharing>(&mut self, cx: &Cx, store: &mut CommandStore<'_, '_, D, S>, answer_root: &str, rows: &Rows<'_>, frame: &Frame) -> Result<(), CliError> {
        let mut asked: blaze_own::Vec<ContentId, &Frame> = blaze_own::Vec::new_in(frame);
        for leaf in rows.of(LeafKind::ScriptGit) {
            let path = rows.path(leaf);
            // A discovery row (no newline) asks no status.
            let Some((folder, command)) = path.split_once('\n') else { continue };
            let mut words = [""; MOST_GIT_WORDS];
            let count = git_words(command, &mut words).ok_or(CliError::Text { what: "a kept git row's command is malformed or exceeds MOST_GIT_WORDS" })?;
            let (untracked, all) = match git::question(&words[..count]) {
                git::Question::Status { untracked, all } => (untracked, all),
                git::Question::Describe { dirty: true, .. } => (false, false),
                _ => continue,
            };
            if !relative(folder) {
                return Err(CliError::Text { what: "a kept git row's folder is not relative to the granted tree" });
            }
            let work = script_sandbox::status_scope(&self.tree, folder, self.scratch);
            asked.push(self.status(work, untracked, all)?);
        }
        asked.sort_unstable();
        asked.dedup();
        let Some(journal) = &self.journal else {
            self.counts.kept = Err(NotKept::NoPlace);
            return Ok(());
        };
        let now = journal.now;
        let mut placed: blaze_own::Vec<(ContentId, ContentId, &git::StatusAnswer<'_>), &Frame> = blaze_own::Vec::new_in(frame);
        for key in asked.iter() {
            if let Some((digest, StatusCell::Placed(answer))) = self.status.get(key)
                && answer.place == now
            {
                placed.push((*key, *digest, answer));
            }
        }
        if placed.is_empty() {
            // Nothing to start from next time, and nothing written.
            self.counts.kept = Ok(0);
            return Ok(());
        }
        if placed.len() > MOST_KEPT {
            self.counts.kept = Err(NotKept::TooMany { questions: u32::try_from(placed.len()).unwrap_or(u32::MAX) });
            return Ok(());
        }
        let mut coverage: blaze_own::Vec<u8, &Frame> = blaze_own::Vec::with_capacity_in(COVERAGE_HEADER + placed.len() * COVERAGE_ENTRY, frame);
        coverage.extend_from_slice(COVERAGE_MAGIC);
        coverage.extend_from_slice(&now.journal);
        coverage.extend_from_slice(&now.at.to_le_bytes());
        coverage.extend_from_slice(&u32::try_from(placed.len()).unwrap_or(u32::MAX).to_le_bytes());
        let mut keys: blaze_own::Vec<Key, &Frame> = blaze_own::Vec::with_capacity_in(placed.len(), frame);
        let mut bytes: AVec<'_, u8> = blaze_own::Vec::new_in(self.scratch);
        for (key, digest, answer) in placed.iter() {
            coverage.extend_from_slice(&key.0);
            coverage.extend_from_slice(&digest.0);
            coverage.extend_from_slice(&answer.tree.0);
            let flags = (if answer.untracked { UNTRACKED } else { 0 }) | (if answer.all { ALL } else { 0 }) | (if answer.text_auto { TEXT_AUTO } else { 0 });
            coverage.push(flags);
            bytes.clear();
            status_answer(AString::from_str_in(answer.text.as_str(), self.scratch)).digest_bytes(&mut bytes);
            let record = status_answer_key(*digest);
            store.put(cx, record, &bytes)?;
            keys.push(record);
        }
        let key = Key(ContentId::of(COVERAGE_DOMAIN, &coverage));
        store.put(cx, key, &coverage)?;
        pin_coverage(store, answer_root, key, &keys, self.plant, frame)?;
        self.counts.kept = Ok(u32::try_from(placed.len()).unwrap_or(u32::MAX));
        Ok(())
    }

    /// The status question `untracked`/`all` of the worktree `work`, answered once this
    /// command: its key in [`CheckoutSweep::status`].
    fn status(&mut self, work: &str, untracked: bool, all: bool) -> Result<ContentId, CliError> {
        let key = status_key(work, untracked, all);
        if self.status.contains_key(&key) {
            return Ok(key);
        }
        if self.status.len() == READS_CAP {
            return Err(CliError::Text { what: "more than 65536 distinct repository status questions in a script-read sweep" });
        }
        let alloc = self.scratch;
        let previous = match &self.previous {
            Ok(kept) => kept.binary_search_by_key(&key, |(question, _)| *question).ok().map(|at| &kept[at].1),
            Err(_) => None,
        };
        let answered = match &self.journal {
            Some(journal) => {
                let mut paths: AVec<'_, &str> = blaze_own::Vec::new_in(alloc);
                let covered = journal.covered.as_ref().map(|(since, changed)| {
                    paths.extend(changed.iter().map(|path| path.as_str()));
                    *since
                });
                let proof = covered.map(|since| {
                    let proof = git::CoveredSince::replayed(since, &paths);
                    match journal.ignored {
                        Some(folder) => proof.ignoring(folder),
                        None => proof,
                    }
                });
                match script_sandbox::status_after(&self.tree, work, untracked, all, previous, proof.as_ref(), journal.now, alloc, blaze_own::planted::NoPlant) {
                    Ok((after, counts)) => {
                        self.counts.asked(counts);
                        match after.made {
                            git::Made::Incremental { regions } => {
                                self.counts.incremental = self.counts.incremental.saturating_add(1);
                                self.counts.regions = self.counts.regions.saturating_add(regions);
                            }
                            git::Made::Whole(why) => {
                                self.counts.whole = self.counts.whole.saturating_add(1);
                                if self.counts.first_whole.is_none() {
                                    self.counts.first_whole = Some(why);
                                }
                            }
                        }
                        let mut answer = after.answer;
                        let observed = status_answer(core::mem::replace(&mut answer.text, AString::new_in(alloc)));
                        let digest = script_sandbox::answer_digest(&observed);
                        answer.text = observed.stdout;
                        (digest, StatusCell::Placed(answer))
                    }
                    Err(_) => {
                        self.counts.refused = self.counts.refused.saturating_add(1);
                        (NONE, StatusCell::Refused)
                    }
                }
            }
            None => match script_sandbox::git_question(&self.tree, work, git::Question::Status { untracked, all }, alloc) {
                Ok((answer, counts)) => {
                    self.counts.asked(counts);
                    self.counts.unplaced = self.counts.unplaced.saturating_add(1);
                    (script_sandbox::answer_digest(&answer), StatusCell::Unplaced(answer.stdout))
                }
                Err(_) => {
                    self.counts.refused = self.counts.refused.saturating_add(1);
                    (NONE, StatusCell::Refused)
                }
            },
        };
        self.status.insert(key, answered);
        Ok(key)
    }

    /// `describe [--always] --dirty` in `folder`: the answer without the mark, marked from the
    /// tracked status of its worktree (git.rs `dirty_marked`, the mark's one home), which is
    /// answered as any status question is ([`CheckoutSweep::status`]). Byte-equal to git.rs
    /// `answer`'s, which composes the same two.
    fn describe_dirty(&mut self, folder: &str, always: bool) -> Result<ContentId, CliError> {
        let plain = script_sandbox::git_question(&self.tree, folder, git::Question::Describe { always, dirty: false }, self.scratch);
        let Ok((answer, counts)) = plain else { return Ok(NONE) };
        self.counts.asked(counts);
        // A failed describe (no names) is never marked, and git.rs asks no status for it.
        if answer.code != 0 {
            return Ok(script_sandbox::answer_digest(&answer));
        }
        let work = script_sandbox::status_scope(&self.tree, folder, self.scratch);
        let key = self.status(work, false, false)?;
        match self.status.get(&key).and_then(|(_, cell)| cell.text()) {
            Some(tracked) => Ok(script_sandbox::answer_digest(&git::dirty_marked(answer, tracked))),
            None => Ok(NONE),
        }
    }

    pub(crate) fn view(&self) -> StatusView {
        StatusView(())
    }

    pub(crate) fn is_direct(&self, kind: ReadKind, path: &str) -> bool {
        if !matches!(kind, ReadKind::File | ReadKind::Stat | ReadKind::Listing) || !relative(path) {
            return false;
        }
        self.tree.is_direct(path) || self.root_abs.is_some_and(|root| absent_direct(root, path))
    }

    pub(crate) fn row(&mut self, kind: ReadKind, path: &str) -> Result<(u8, Option<Stamp>, ContentId), CliError> {
        use script_sandbox::TreeRead;
        let op = match kind {
            ReadKind::File => TreeRead::File,
            ReadKind::Listing => TreeRead::Listing,
            ReadKind::Stat => TreeRead::Stat,
            ReadKind::Git => return Ok((0, None, self.answer(path)?)),
        };
        if !relative(path) {
            return Err(CliError::Text { what: "a saved script read is outside the granted checkout" });
        }
        let answer = match op.answer(&self.tree, path) {
            Ok(answer) => answer.unwrap_or(NONE),
            Err(_) => DENIED,
        };
        let flags = if self.is_direct(kind, path) { crate::sweep::DIRECT_PATH } else { 0 };
        Ok((flags, None, answer))
    }

    #[cfg(test)]
    pub(crate) fn status_questions(&self) -> usize {
        self.status.len()
    }

    pub(crate) fn answer(&mut self, asked: &str) -> Result<ContentId, CliError> {
        if !asked.contains('\n') {
            if !relative(asked) {
                return Err(CliError::Text { what: "a git discovery row is outside the checkout" });
            }
            return Ok(script_sandbox::scope_answer(script_sandbox::status_scope(&self.tree, asked, self.scratch)));
        }
        let mut words = [""; MOST_GIT_WORDS];
        let (folder, count) = git_parts(asked, &mut words)?;
        let words = &words[..count];
        let alloc = self.scratch;
        match git::question(words) {
            git::Question::Status { untracked, all } => {
                let work = script_sandbox::status_scope(&self.tree, folder, alloc);
                let key = self.status(work, untracked, all)?;
                Ok(self.status.get(&key).map_or(NONE, |(digest, _)| *digest))
            }
            git::Question::Describe { always, dirty: true } => self.describe_dirty(folder, always),
            _ => Ok(script_sandbox::git_answer(&self.tree, folder, words, alloc).unwrap_or(NONE)),
        }
    }
}

/// Returns `true` when `path` is a clean relative checkout path that does not exist on disk and
/// whose existing ancestor components under `root_abs` are all non-symlinks (and `path` itself
/// is not a broken symlink). Such absent paths can only appear via a write in one of their
/// watched ancestor directories.
fn absent_direct(root_abs: &str, path: &str) -> bool {
    use blaze_sys::Errno;
    use blaze_sys::posix::io::CWD;
    use blaze_sys::posix::proc::{AT_SYMLINK_NOFOLLOW, S_IFDIR, S_IFLNK, S_IFMT, stat_at};

    if path.is_empty() || path.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
        return false;
    }
    let root_b = root_abs.trim_end_matches('/').as_bytes();
    let mut buf = [0u8; 4096];
    let mut end = 0usize;
    while end < path.len() {
        let Some(rest) = path.get(end..) else {
            return false;
        };
        let next = rest.find('/').map_or(path.len(), |i| end + i);
        let Some(prefix) = path.get(..next) else {
            return false;
        };
        let Some(sep_at) = root_b.len().checked_add(1) else {
            return false;
        };
        let Some(nul_at) = sep_at.checked_add(prefix.len()) else {
            return false;
        };
        let Some(needed) = nul_at.checked_add(1) else {
            return false;
        };
        let Some(dst_root) = buf.get_mut(..root_b.len()) else {
            return false;
        };
        dst_root.copy_from_slice(root_b);
        let Some(sep) = buf.get_mut(root_b.len()) else {
            return false;
        };
        *sep = b'/';
        let Some(dst_prefix) = buf.get_mut(sep_at..nul_at) else {
            return false;
        };
        dst_prefix.copy_from_slice(prefix.as_bytes());
        let Some(nul) = buf.get_mut(nul_at) else {
            return false;
        };
        *nul = 0;
        let Some(slice) = buf.get(..needed) else {
            return false;
        };
        let Ok(cstr) = core::ffi::CStr::from_bytes_with_nul(slice) else {
            return false;
        };
        match stat_at(CWD, blaze_sys::Path::new(cstr), AT_SYMLINK_NOFOLLOW) {
            Ok(st) => {
                let kind = st.st_mode & S_IFMT;
                if kind == S_IFLNK {
                    return false;
                }
                if kind != S_IFDIR {
                    return next < path.len();
                }
            }
            Err(Errno::NoEnt | Errno::NotDir) => return true,
            Err(_) => return false,
        }
        end = next + 1;
    }
    false
}

fn git_parts<'s>(asked: &'s str, words: &mut [&'s str; MOST_GIT_WORDS]) -> Result<(&'s str, usize), CliError> {
    let Some((folder, command)) = asked.split_once('\n') else {
        return Err(CliError::Text { what: "a git read's leaf row is not `<folder>\\n<words>` (engine_script_reads.rs `git_now`)" });
    };
    if !relative(folder) {
        return Err(CliError::Text { what: "a git read's folder is not relative to the granted tree" });
    }
    let Some(count) = git_words(command, words) else {
        return Err(CliError::Text { what: "a git read's command is malformed or exceeds MOST_GIT_WORDS" });
    };
    Ok((folder, count))
}

/// The simulated filesystem's reader for ref/object questions. Status requires
/// the actual served tree: this adapter cannot observe link text or executable mode.
fn git_now(cx: &Cx, root: &Dir<ReadWrite>, asked: &str, frame: &Frame) -> Result<ContentId, CliError> {
    let mut words = [""; MOST_GIT_WORDS];
    let (folder, count) = git_parts(asked, &mut words)?;
    let question = git::question(&words[..count]);
    if matches!(question, git::Question::Status { .. } | git::Question::Describe { dirty: true, .. }) {
        return Err(CliError::Text { what: "git status replay requires the build script's granted tree (engine_script_reads.rs CheckoutSweep)" });
    }
    let erased = blaze_own::Erased::new(frame);
    let alloc = erased.handle();
    let mut files = DirGit { cx, root, frame, fault: None };
    let answered = git::answer(question, folder, git::Vcs::Served, &mut files, alloc);
    // A fault of the file system is the command's error, never an absent file (rule 3).
    if let Some(fault) = files.fault {
        return Err(fault);
    }
    let Ok(answer) = answered else { return Ok(NONE) };
    let mut bytes: AVec<'_, u8> = blaze_own::Vec::new_in(alloc);
    answer.digest_bytes(&mut bytes);
    // The sandbox's `answer_digest` is its read digest of these bytes, which `file_answer` is.
    Ok(script_sandbox::file_answer(&bytes))
}

/// The checkout's `.git` as `git::answer` reads it, through bstd: what the sandbox's `GitView`
/// serves a member's run over the same files. A fault of the file system is kept and returned by
/// [`git_now`] (rule 3), never read as an absent file.
///
/// Where it serves otherwise than `GitView` (each only makes the answer differ, so the script
/// runs again): a stat holds the kind and size alone (no times, inode or mode: `status`, which
/// compares them with the index, requires [`CheckoutSweep`]); a link is whatever bstd's stat says it
/// leads to, where `GitView` refuses to pass through one; and an absolute path a worktree's
/// `.git` file names is outside ([`GitFiles::within`] is `None`), as this reader is not told
/// where the checkout is.
struct DirGit<'c> {
    cx: &'c Cx,
    root: &'c Dir<ReadWrite>,
    frame: &'c Frame,
    fault: Option<CliError>,
}

impl DirGit<'_> {
    /// `rel` as a path under the checkout; `None` (absent) when it is not plain, as `GitView`
    /// reads such a path.
    fn plain(rel: &str) -> Option<Path> {
        if rel.is_empty() || rel.split('/').any(|part| matches!(part, "" | "." | "..")) {
            return None;
        }
        Path::new(rel).ok()
    }

    /// `rel` and what is there; `None` when absent, or on a fault, which is kept.
    fn meta(&mut self, rel: &str) -> Option<(Path, bstd::fs::Meta)> {
        let path = Self::plain(rel)?;
        match self.root.stat(self.cx, &path) {
            Ok(Some(meta)) => Some((path, meta)),
            Ok(None) => None,
            Err(error) => {
                self.fault.get_or_insert(CliError::from(error));
                None
            }
        }
    }

    fn keep(&mut self, error: bstd::fs::FsError) {
        self.fault.get_or_insert(CliError::from(error));
    }
}

impl GitFiles for DirGit<'_> {
    fn read(&mut self, rel: &str, into: &mut AVec<'_, u8>) -> bool {
        let Some((path, meta)) = self.meta(rel) else { return false };
        if meta.dir {
            return false;
        }
        let mut bytes = Bytes::new_in(self.frame, "a `.git` file a build script's git fact reads");
        match self.root.read_all(self.cx, &path, &mut bytes) {
            Ok(_) => {
                into.extend_from_slice(bytes.as_slice());
                true
            }
            Err(error) if error.kind == bstd::fs::FsKind::NotFound => false,
            Err(error) => {
                self.keep(error);
                false
            }
        }
    }

    fn read_range(&mut self, rel: &str, at: u64, len: u64, into: &mut AVec<'_, u8>) -> bool {
        let Ok(count) = usize::try_from(len) else { return false };
        let Some((path, meta)) = self.meta(rel) else { return false };
        if meta.dir || at.checked_add(len).is_none_or(|end| end > meta.len) {
            return false;
        }
        let file = match self.root.open_read(self.cx, &path) {
            Ok(file) => file,
            Err(error) if error.kind == bstd::fs::FsKind::NotFound => return false,
            Err(error) => {
                self.keep(error);
                return false;
            }
        };
        let mut bytes = Bytes::new_in(self.frame, "a range of a `.git` pack a build script's git fact reads");
        match file.read_at(self.cx, at, count, &mut bytes) {
            Ok(read) if read == count => {
                into.extend_from_slice(bytes.as_slice());
                true
            }
            // Shorter than its stat said: it changed while read, which this answer cannot use.
            Ok(_) => false,
            Err(error) => {
                self.keep(error);
                false
            }
        }
    }

    fn stat(&mut self, rel: &str) -> Option<git::FileStat> {
        let (_, meta) = self.meta(rel)?;
        let kind = if meta.dir { git::FileKind::Dir } else { git::FileKind::File };
        Some(git::FileStat { kind, size: meta.len, mtime: (0, 0), ctime: (0, 0), ino: 0, uid: 0, gid: 0, executable: false })
    }

    fn list<'a>(&mut self, rel: &str, alloc: Alloc<'a>, into: &mut AVec<'a, AString<'a>>) {
        let Some(path) = Self::plain(rel) else { return };
        match self.root.entries(self.cx, &path) {
            // Sorted by name (bstd), as `GitView` sorts its names by bytes.
            Ok(Some(listed)) => {
                for i in 0..listed.len() {
                    if !matches!(listed.kind(i), EntryKind::Gone) {
                        into.push(blaze_own::String::from_str_in(listed.name(i), alloc));
                    }
                }
            }
            Ok(None) => {}
            Err(error) => self.keep(error),
        }
    }

    fn within<'p>(&self, _path: &'p str) -> Option<&'p str> {
        None
    }
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
