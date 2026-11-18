//! The `CrateScope` task: one crate's names resolved from its dependencies' records
//! (docs/v2/plan/design/drive.md, the NAMES step, "HANDED ON 07:xx").
//!
//! The world (blaze-world: every crate's `CrateInput`, its key and where its files are) is
//! made once a command from the `UnitGraph` record, the lock, the pinned toolchain on the
//! shelf under the home and the fetched sources' records. The `UnitGraph` job's record names
//! one `CrateScope` part per crate of the world ([`parts`]), its subject the crate's key: a
//! dependent names a dependency's record from the key alone.
//!
//! A task asks for its direct dependencies' and the sysroot crates' records, then runs
//! blaze-scope's `resolve_crate`; a `Need` from it asks for those records too and enters again.
//! The record is a head, then the exports record, the inner one, the names record and the
//! items record ([`sections`]); its out is the digest of the EXPORTS only, so a private change does not move a dependent. The task's key is the crate's key (an
//! identity, no text): every checkout file the resolution read is listed in its reads as that
//! file's `Surface`, so a body edit (every `Surface` out unchanged) runs no `CrateScope` task,
//! and a signature edit runs that crate's. A fetched or toolchain file is under the key.
//!
//! A crate whose unit has a build script reads that script's run, its unit's `Script` record
//! (scripts.rs, decision D115 (B)): the task Needs it, joins its `rustc-cfg` atoms and record
//! into the crate's input and its `OUT_DIR` and `rustc-env` pairs into the crate's
//! environment, and lists it in its reads; never in its key.
//!
//! Interim, said in the closing note: proc macros are not run (their calls are counted as
//! unresolved by a known gap); every record lives in the project's store, the toolchain's and
//! fetched crates' too (they belong in the shared store); a path package must be a workspace
//! member (each is read under its own directory; one outside the workspace root is refused);
//! the world and the records are held in the global allocator for the command.

use core::cell::RefCell;
use std::alloc::Global;
use std::string::String;

use blaze_drive::{Input, Kind, Name, Task, TaskFailure, Want, Why};
use blaze_fetch::SourceId;
use blaze_fetch::pack::PackView;
use blaze_id::CrateItemKey;
use blaze_ids::ContentId;
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::Alloc;
use blaze_scope::crate_scope::{CrateScopeError, resolve_crate};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::{CrateInput, SourceTexts};
use blaze_scope::prepares::Prepares;
use blaze_scope::task::Serial;
use blaze_unit_graph::{Records, StoreSources};
use blaze_world::{Fetched, Listed, Tree, World, WorldError};
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadOnly, ReadWrite};
use bstd::path::Path;
use bstd::mem::{Bytes, Frame};

use crate::CliError;
use crate::fetch::FetchEnv;
use crate::store::Store;
use crate::work::{Did, Made, held_str};

pub(crate) mod expansion_texts;
pub(crate) mod failed_call;
pub(crate) mod include_outside;
pub(crate) mod test_marks;
pub(crate) mod metered;
mod scope;
pub(crate) mod sections;

// The subject and source domains name the payload's form ([`sections::FORM`] 6) and its producer:
// an old home's records (form 1: exports then inner; form 2: no tests section; "3": form 3 by
// the old name fill, before blaze-scope's cell core became the writer; "4": form 3, without the
// expansion texts; "5": an out of the names record alone, see [`OUT_DOMAIN`]; "6": a names
// section of names record version 1, without the failed calls; "7": form 4, without the link
// table; "8": form 5, without the cell table; "9": a cell table without the names section's
// whole row; "10": a cell table without the `MemberOf`, `VariantOf` and `Beginning` rows, whose
// readers would read those cells absent; "11": an items record of version 8, whose item rows
// hold no `extern` block ABI, and `Item` cells that do not cover it; "12": the expansion texts
// section (form 3) in place of the expansions directory, the texts and columns in the record
// itself) are under other names, never found, made again.
// The subject's domain is blaze-drive key.rs `CRATE_SCOPE_DOMAIN` (the name's one home,
// `crate_scope_name`): it names the crate's task, not its record's form, so a form change
// moves this source domain alone (every key moves; the name a reader reads by stays). Bump it
// there only when what a name stands for changes.
const SOURCE_DOMAIN: &str = "blaze-cli crate scope source 13";
/// The out: the names record AND the checkout files the crate holds ([`crate_out`]). "1" was
/// the names record alone, but a body's check reads its crate's items record to find which
/// crate owns its file (body_types.rs `prepare`): a module file created gave a new record at
/// an EQUAL out, so a reader handed the old record (settled standing, then made again: the
/// drive does not re-enter readers of an equal out) kept "no crate's names record lists this
/// file" while a fresh home did not (rule 21; main.rs `edits_of_module_files_answer_as_a_fresh_home`).
const OUT_DOMAIN: &str = "blaze-cli crate scope out 2";
/// The rule a refusal of this task names.
const RULE: &str = "names.rs";


/// Why the names world of a command could not be made.
#[derive(Debug)]
pub enum NamesError {
    /// The unit graph record does not decode.
    Graph(blaze_unit_graph::record::RecordRefused),
    /// No lock was read this command.
    NoLock,
    /// `HOME` or `PWD` is not set: the toolchain's shelf is under the home, reached from the start.
    NoHome,
    /// The frame that holds the names step's files could not be reserved.
    Frame,
    /// The toolchain's shelf could not be reached.
    Shelf(crate::toolchain::ShelfError),
    /// The pinned toolchain could not be opened (its words).
    Toolchain(String),
    /// The fetched sources could not be opened (their words).
    Sources(String),
    /// The world could not be laid out (its words).
    World(String),
    /// The workspace's members were not matched to the lock this command (its finding is on
    /// the root's units): no member's directory is known.
    NoMembers,
    /// Two members whose directory is one: neither's files can be told apart.
    SharedDirectory { first: String, second: String, dir: String },
    /// A path package the graph reaches that is no member of the workspace (a path dependency
    /// outside the workspace root): the names step reads members' directories only. `dependent`
    /// is the first unit that depends on it, `None` when none does.
    PathNotMember { package: String, dependent: Option<String> },
}

impl core::fmt::Display for NamesError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NamesError::Graph(refused) => write!(f, "the unit graph record at byte {}: {}", refused.at, refused.why),
            NamesError::NoLock => f.write_str("no lock was read this command"),
            NamesError::NoHome => f.write_str("HOME or PWD is not set: the toolchain's shelf is under the home, reached from the start folder"),
            NamesError::Frame => write!(f, "the frame for the names step's files ({NAMES_FRAME} bytes reserved) could not be reserved"),
            NamesError::Shelf(error) => write!(f, "{error}"),
            NamesError::Toolchain(words) => write!(f, "the pinned toolchain: {words}"),
            NamesError::Sources(words) => write!(f, "the fetched sources: {words}"),
            NamesError::World(words) => write!(f, "the crates' inputs: {words}"),
            NamesError::NoMembers => f.write_str("the workspace's members were not matched to the lock this command: no member's directory is known"),
            NamesError::SharedDirectory { first, second, dir } => {
                write!(f, "the members `{first}` and `{second}` are both in the directory `{dir}`: one directory holds one package")
            }
            NamesError::PathNotMember { package, dependent: Some(dependent) } => write!(
                f,
                "the path package `{package}`, a dependency of `{dependent}`, is no member of the workspace (a path outside the workspace root): its directory is not known to the names step"
            ),
            NamesError::PathNotMember { package, dependent: None } => {
                write!(f, "the path package `{package}` is no member of the workspace (a path outside the workspace root): its directory is not known to the names step")
            }
        }
    }
}

/// The reserve of the frame that holds what the names step read for the command: the
/// toolchain's records, the fetched sources' records and the checkout's files.
const NAMES_FRAME: usize = 4 << 30;

/// What the names step reads, through the command's own files (rule 75: the IO is the one
/// `main` was given, so a simulated command reads its simulated files): each file read whole
/// into the names frame and held to the end of the command.
struct Reader {
    cx: &'static Cx,
    /// The checkout, read-only.
    root: Dir<ReadOnly>,
    frame: &'static Frame,
}

impl Reader {
    /// The file at `path` under the checkout, held to the end of the command; `None` when
    /// there is none (or it could not be read: the reader that asked says what it lacked).
    fn read(&self, path: &str) -> Option<&'static [u8]> {
        if path.split('/').any(|part| part == ".." || part == ".") {
            return None;
        }
        let at = Path::new(path).ok()?;
        let mut bytes = Bytes::new_in(self.frame, "the names step's files");
        self.root.read_all(self.cx, &at, &mut bytes).ok()?;
        // Held to the end of the command, in the names frame.
        let held: &'static Bytes<'static> = std::boxed::Box::leak(std::boxed::Box::new(bytes));
        Some(held.as_slice())
    }
}

/// The checkout's files, as the names library asks for them.
struct ReadTree(&'static Reader);

impl Tree for ReadTree {
    fn file(&self, path: &str) -> Option<&[u8]> {
        self.0.read(path)
    }
}

/// The fetched sources' records: `<hex of the key>` in the machine's sources store
/// (`<home>/.blaze/v2/sources`, fetch.rs `SOURCES`), never the checkout, for any key the graph
/// pins: a reader over that store, with the names step's context and frame.
struct Folder(Reader);

impl Folder {
    /// The sources store opened read only beside `checkout`'s reader (its context and frame).
    fn of(checkout: &'static Reader, env: &FetchEnv<'_>) -> Result<Folder, NamesError> {
        let sources = crate::fetch::sources_dir(checkout.cx, env).map_err(|error| NamesError::Sources(error.to_string()))?;
        Ok(Folder(Reader { cx: checkout.cx, root: sources.read_only(), frame: checkout.frame }))
    }
}

impl<'b> Records<'b> for Folder {
    fn record(&mut self, source: &SourceId<'_>) -> Option<&'b [u8]> {
        let hex = crate::fetch::key_hex(&source.key());
        self.0.read(core::str::from_utf8(&hex).ok()?)
    }
}

/// One crate made this command: its payload, out, and the checkout files its resolution read.
pub(crate) struct Resolved {
    /// The record's payload ([`sections::write`]).
    payload: blaze_own::Vec<u8, Global>,
    out: ContentId,
    /// Each checkout file read, checkout-relative, sorted and without repeats.
    files: std::vec::Vec<String, Global>,
    /// Each checkout file looked for and not found (a `mod x;` whose file is missing, a
    /// module's other place): read as its `Surface`, absent, so creating it reruns the crate.
    absent: std::vec::Vec<String, Global>,
    /// Each checkout file read as DATA (`include_str!`, `include_bytes!`), with the slot of the
    /// file whose call read it (the home whose outline lists it as a data part), sorted and
    /// without repeats: read as the file's own `FileItems` record, whose out is its bytes' id
    /// (data_include.rs), never as a `Surface`.
    data: std::vec::Vec<(String, String), Global>,
    /// Each unresolved path of the crate's own code, as its record's finding ([`unresolved`]).
    findings: blaze_own::Vec<Unresolved, Global>,
    /// Each include of the crate's own code read outside its package, inside the workspace
    /// (D111): its record's warning finding ([`include_outside::findings`]).
    left_package: blaze_own::Vec<Unresolved, Global>,
    /// The crate's failure, filed as the record's [`crate::work::TASK_FAILED`] error: its
    /// names could not be resolved or a section written (the out is then
    /// [`crate::work::failed_out`]), or its tests section is empty though it is a test unit.
    failed: Option<std::string::String>,
    /// Each failed macro call of the crate's own code, as its record's error finding
    /// ([`failed_call::findings`]).
    failed_calls: blaze_own::Vec<Unresolved, Global>,
    /// What the tests record says about single functions of the crate's own code, each with
    /// its rule ([`test_marks::findings`]).
    test_marks: blaze_own::Vec<(u16, Unresolved), Global>,
    /// The crate's expansion records, which its payload's directory names: put after its
    /// record ([`crate::work::Record::extras`]).
    expansions: crate::work::Extras,
}

/// A resolved crate's payload ([`sections::write`]) with its unresolved paths as findings.
struct ScopeRecord {
    payload: blaze_own::Vec<u8, Global>,
    findings: blaze_own::Vec<Unresolved, Global>,
    unresolved: u32,
    /// Why the tests section is empty though the unit is a test unit.
    tests_refused: Option<std::string::String>,
    /// What the tests record says about single functions ([`test_marks::findings`]).
    test_marks: blaze_own::Vec<(u16, Unresolved), Global>,
    /// Each distinct expansion record the payload's directory names, copied out of the task's
    /// frame into `Global` with the payload.
    expansions: crate::work::Extras,
}

/// A section of a resolved crate's record that could not be written: the crate's failure.
#[derive(Debug)]
struct Unwritten {
    crate_name: std::string::String,
    section: &'static str,
    why: std::string::String,
}

impl core::fmt::Display for Unwritten {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "crate `{}`: its {} record could not be written: {}", self.crate_name, self.section, self.why)
    }
}

/// The record of the crate `crate_name` resolved as `done`; `alloc` is the allocator `done` was
/// made in (its task's frame), which the link table is written into before its copy into the
/// payload.
/// `quiet`: the names the crate calls its failed dependencies by (names/scope.rs
/// `resolve_metered`), whose paths are counted, not found (plan row A8).
fn scope_record<'a>(done: &blaze_scope::crate_scope::CrateScope<'a>, crate_name: &str, quiet: &[&str], alloc: Alloc<'a>) -> Result<ScopeRecord, Unwritten> {
    let unwritten = |section: &'static str, why: std::string::String| Unwritten { crate_name: crate_name.into(), section, why };
    let mut findings = blaze_own::Vec::new_in(Global);
    let unresolved = done.names.notes.len() as u32 + unresolved(done.names, quiet, &mut findings);
    // The names and items records, filed in the same record (drive.md, "End state", piece
    // 3): a body is checked by reading them, never by solving the crate graph again.
    // Scratch copied into the payload below: in the task's frame, as the link table is.
    let names_record = blaze_scope::names_record::encode(done.names, alloc).map_err(|error| unwritten("names", std::format!("{error:?}")))?;
    let items_record = blaze_scope::items_record::encode(done.names, alloc).map_err(|error| unwritten("items", std::format!("{error:?}")))?;
    // A test unit's tests, from attributes alone (test.md, "The tests record", piece 6): the
    // fifth section. A unit without `test` in its cfg has none, and its section is empty.
    // A refused tests record is the crate's finding, not its failure: the section stays
    // empty and the rest of the record is kept, standing until a file it read moves. What it
    // says of one function (refused, or marked twice) is that function's finding: the other
    // tests keep their rows.
    let mut tests_refused = None;
    let tests_record = match blaze_scope::tests_record::encode(done.names, alloc) {
        Ok(section) => Some(section),
        Err(blaze_scope::tests_record::TestsRecordError::NotATestUnit { .. }) => None,
        Err(error) => {
            tests_refused = Some(std::format!("{}", unwritten("tests", std::format!("{error}"))));
            None
        }
    };
    let test_marks = match &tests_record {
        Some(section) => test_marks::findings(done.names, section),
        None => blaze_own::Vec::new_in(Global),
    };
    let tests: &[u8] = match &tests_record {
        Some(section) => &section.bytes[..],
        None => &[],
    };
    // The crate's expansions (runtime.md plan row B3): its directory is the record's section, a
    // row a call that made a text, beside the items rows that name them; each distinct text
    // with its origin column is its own record, keyed by its bytes, put after this one
    // ([`expansion_texts`]): a text many calls made is held once, however many crates made it.
    let split = blaze_scope::expansion_records::write(done.names, alloc).map_err(|error| unwritten("expansions", std::format!("{error}")))?;
    let expansions = &split.directory[..];
    let mut records = blaze_own::Vec::with_capacity_in(split.records.len(), Global);
    for record in split.records.iter() {
        let mut bytes = blaze_own::Vec::with_capacity_in(record.bytes.len(), Global);
        bytes.extend_from_slice(&record.bytes);
        records.push((record.key, bytes));
    }
    let calls = u32::try_from(blaze_scope::expansion_records::Directory::open(expansions).map_err(|error| unwritten("expansions", std::format!("{error}")))?.len())
        .map_err(|_| unwritten("expansions", "more than a u32 of macro calls".into()))?;
    let extras = crate::work::Extras { records, calls, released: None };
    // The crate's link table, from the names resolved here (run.md, "the native image as a
    // task"): a refused table is written as its refusal (state 1), so only a program that links
    // the crate fails; an error here is a table that could not be written at all.
    let mut links = blaze_own::AVec::new_in(alloc);
    blaze_ladder::native_links::scope_table(done.names, &mut links, alloc).map_err(|error| unwritten("link table", std::format!("{error:?}")))?;
    // The cell table (drive.md, "Names at module grain" point 3): what a reader by cell is
    // checked against, over the two records above and, whole, the names, the items and the
    // expansions. Each section a reader can read whole (`NameCell::Whole`) is a row: a body's
    // read of a cell the table does not hold is read ABSENT (engine_bodies.rs `widen`), so a
    // whole section missing here would be a read no edit moves (rule 21).
    let wholes = [
        (blaze_scope::names_cells::Section::Names, &names_record[..]),
        (blaze_scope::names_cells::Section::Items, &items_record[..]),
        (blaze_scope::names_cells::Section::Expansions, expansions),
    ];
    let cells = blaze_scope::names_cells::crate_table(done.names, &names_record, &items_record, &wholes, alloc).map_err(|error| unwritten("cell table", std::format!("{error:?}")))?;
    let payload = sections::write(unresolved, [&done.record[..], &done.inner[..], &names_record[..], &items_record[..], tests, expansions, &links[..], &cells[..]]);
    #[cfg(test)]
    sections::watch_tests(crate_name, &payload, tests);
    Ok(ScopeRecord { payload, findings, unresolved, tests_refused, test_marks, expansions: extras })
}

/// The rule of a `use` or macro path in a project's own crate that names nothing.
pub(crate) const UNRESOLVED: u16 = 14;

/// Whether a finding of `rule` (blaze-cli's area) is placed by [`place_pieces`]: its message
/// starts with the file, the item chain and the edition. The one list of them.
pub(crate) fn placed(rule: u16) -> bool {
    matches!(rule, UNRESOLVED | include_outside::INCLUDE_OUTSIDE_PACKAGE | failed_call::FAILED_CALL | test_marks::TEST_REFUSED | test_marks::TEST_DUPLICATED)
}

/// Whether a names finding of `rule` (blaze-cli's area) is a warning: drawn after the answer,
/// never an error (main.rs `every_rule`, print.rs). The one list of them.
pub(crate) fn warning(rule: u16) -> bool {
    matches!(rule, include_outside::INCLUDE_OUTSIDE_PACKAGE | test_marks::TEST_DUPLICATED)
}

/// One unresolved path of a project crate, placed item-relative (rule 71): the range of the
/// failing segment from its item's start, and the message: the file, the item's chain digest
/// ([`crate::work::item_chain`]; 16 hex digits, or empty for a path with no place), the
/// file's edition byte, then the words' two pieces, all split by 0x1F. A body edit moves no
/// byte of it; the printer finds the item's start in the file as it is ([`placed`]).
struct Unresolved {
    at: (u32, u32),
    message: blaze_own::Vec<u8, Global>,
}

/// The pieces before the words in an [`UNRESOLVED`] finding's message.
const PLACE_PIECES: usize = 3;

/// The byte offset of `word` in `text` at or after `from`, as a whole identifier.
fn find_word(text: &str, word: &str, from: usize) -> Option<usize> {
    let ident = |b: u8| b == b'_' || b.is_ascii_alphanumeric();
    let bytes = text.as_bytes();
    let mut at = from;
    while let Some(found) = text.get(at..).and_then(|rest| rest.find(word)) {
        let start = at + found;
        let end = start + word.len();
        let before = start.checked_sub(1).is_some_and(|b| ident(bytes[b]));
        let after = bytes.get(end).is_some_and(|b| ident(*b));
        if !before && !after && !word.is_empty() {
            return Some(start);
        }
        at = start + word.len().max(1);
    }
    None
}

/// Every `use` and macro path of crate `c` that resolved to nothing. The project's own (a
/// checkout file, `/0/`) are findings in `own`, sorted; the number of the others (a fetched or
/// toolchain crate's: not the project's to fix) is returned, counted in one note. A path into a
/// failed dependency (its first segment one of `quiet`, the names the crate calls such crates
/// by, failing past it) is counted too, never a finding: that crate's failure is reported once,
/// at it (plan row A8, rule 19).
fn unresolved(c: &blaze_scope::resolve::CrateNames<'_>, quiet: &[&str], own: &mut blaze_own::Vec<Unresolved, Global>) -> u32 {
    use blaze_surface::{PathKind, ScopeError};
    let project = c.files.iter().any(|file| file.path.starts_with("/0/"));
    let mut elsewhere = 0u32;
    for r in c.done.values() {
        for error in r.scope.errors.iter() {
            let (loc, path, segment) = match error {
                ScopeError::Import { site, path, segment, .. } => {
                    let fragment = site.expansion.unwrap_or(r.scope.module);
                    (c.tables.use_locs.get(&fragment).and_then(|locs| locs.get(site.index as usize).copied()), path, *segment as usize)
                }
                ScopeError::Macro { call, path, segment, .. } => (c.tables.calls.get(call).map(|src| src.loc), path, *segment as usize),
                _ => continue,
            };
            let into_failed = segment > 0 && matches!(path.kind, PathKind::Plain | PathKind::Abs) && path.segments.first().is_some_and(|first| quiet.contains(first));
            if !project || into_failed {
                elsewhere += 1;
                continue;
            }
            let mut message = blaze_own::Vec::new_in(Global);
            let mut at = (0u32, 0u32);
            let file = loc.and_then(|loc| c.files.get(loc.file as usize).map(|file| (loc, *file)));
            match file.and_then(|(loc, file)| Some((loc, file, crate::work::item_holding(&file.outline, loc.offset)?))) {
                Some((_, file, item)) => {
                    let text = file.text;
                    let entry = item.range.of(text);
                    // The failing segment: the path's segments found in order in the item's text.
                    let mut from = 0usize;
                    let mut found = None;
                    for seg in path.segments.iter().take(segment + 1) {
                        found = find_word(entry, seg, from).map(|start| (start, start + seg.len()));
                        from = found.map_or(from, |(_, end)| end);
                    }
                    at = found.map_or((0, 0), |(s, e)| (s as u32, e as u32));
                    message.extend_from_slice(file.path.trim_start_matches("/0/").as_bytes());
                    message.push(0x1F);
                    let chain = crate::work::item_chain(&file.outline, item, text);
                    for shift in (0..16).rev() {
                        message.push(b"0123456789abcdef"[((chain >> (shift * 4)) & 0xF) as usize]);
                    }
                }
                None => message.push(0x1F),
            }
            message.push(0x1F);
            message.push(b'0' + c.edition.byte());
            message.push(0x1F);
            message.push(b'`');
            message.extend_from_slice(path.segments.get(segment).copied().unwrap_or("").as_bytes());
            message.extend_from_slice(b"`\x1F");
            let before = &path.segments[..segment.min(path.segments.len())];
            let head: &[u8] = match path.kind {
                PathKind::Plain | PathKind::DollarCrate(_) => b"",
                PathKind::Crate => b"crate",
                PathKind::SelfMod => b"self",
                PathKind::Super(_) => b"super",
                PathKind::Abs => b"::",
            };
            match (head.is_empty(), before.is_empty()) {
                (true, true) => message.extend_from_slice(b"this scope"),
                _ => {
                    message.push(b'`');
                    message.extend_from_slice(head);
                    for (i, seg) in before.iter().enumerate() {
                        if i > 0 || (!head.is_empty() && head != b"::") {
                            message.extend_from_slice(b"::");
                        }
                        message.extend_from_slice(seg.as_bytes());
                    }
                    message.push(b'`');
                }
            }
            own.push(Unresolved { at, message });
        }
    }
    // The scopes are a map: its order reaches the record only sorted (rule 25).
    own.sort_unstable_by(|a, b| (&a.message[..], a.at).cmp(&(&b.message[..], b.at)));
    elsewhere
}

/// Where an [`UNRESOLVED`] finding is to be drawn, read from its message alone (no file is
/// read here): its file, its item's chain and the edition the chain was made in, and the
/// words. `None` for one with no file: drawn at its crate's name. The item's start in the file
/// as it is today is found where the file is read, once for all its findings (print.rs
/// `list`: one whose item is no longer there is drawn at its crate's name too).
pub(crate) fn place_pieces(message: &[u8]) -> Result<(Option<(&str, u64, blaze_surface::Edition)>, &[u8]), CliError> {
    let mut pieces = message.splitn(PLACE_PIECES + 1, |b| *b == 0x1F);
    let (Some(path), Some(chain), Some(edition), Some(words)) = (pieces.next(), pieces.next(), pieces.next(), pieces.next()) else {
        return Err(CliError::Text { what: "an unresolved path's finding holds no place pieces" });
    };
    if path.is_empty() || chain.len() != 16 {
        return Ok((None, words));
    }
    let path = core::str::from_utf8(path).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
    let chain = core::str::from_utf8(chain).ok().and_then(|hex| u64::from_str_radix(hex, 16).ok()).ok_or(CliError::Text { what: "an unresolved path's item digest is not hex" })?;
    let edition = edition.first().and_then(|b| b.checked_sub(b'0')).and_then(blaze_surface::Edition::from_byte).ok_or(CliError::Text { what: "an unresolved path's finding holds no edition" })?;
    Ok((Some((path, chain, edition)), words))
}

/// The names step's state for a command: the world, each crate by subject, and what the
/// tasks made (a task that asks for its files' `Surface`s answers from here when entered again).
pub struct Names<'f> {
    world: World<'f>,
    /// (subject, crate index), sorted.
    by_subject: std::vec::Vec<(ContentId, usize), Global>,
    /// (key, crate index), sorted.
    by_key: std::vec::Vec<(CrateItemKey, usize), Global>,
    made: std::vec::Vec<(ContentId, Resolved), Global>,
    /// The subjects of the crates whose records may hold nothing of a project (target.md,
    /// "Namespaces"): the toolchain's, and a registry archive's known by its lock checksum.
    /// Whether a record is public also needs every name it reads public: the store's check.
    public: std::vec::Vec<ContentId, Global>,
    /// (crate index, the `Script` name of its side's run), sorted, for each crate that reads
    /// one ([`blaze_world::Script::Read`]): its task Needs it (decision D115 (B), ruling (2)).
    scripts: std::vec::Vec<(usize, Name), Global>,
    /// How each run of `scripts` is placed, in its order (D112's choice point): in the run's key.
    script_placed: std::vec::Vec<crate::scripts::RanAs, Global>,
    /// (crate index, its build script's root file, package-relative), sorted: the file a
    /// failed script is named by (not_checked.rs).
    script_roots: std::vec::Vec<(usize, &'static str), Global>,
    /// Each crate's build-script module lookup (script_modules.rs), sorted by subject, then
    /// crate: one entry a crate reading a script, so two crates of one module are two entries.
    script_modules: std::vec::Vec<crate::script_modules::ScriptModule, Global>,
    /// The command's one set of prepared macro definitions, shared by every crate it resolves.
    prepares: &'f Prepares<'f>,
    /// The unit graph the world was made from (leaked with it): a proc-macro crate's unit, whose
    /// module identity the runner keys it by (proc_runner.rs).
    graph: &'static blaze_unit_graph::UnitGraph<'static>,
    /// The command's proc-macro runner, made by the first `CrateScope` task after the world
    /// (names/scope.rs `prepare`) and lent to every names and body job.
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    /// Where the world placed each locked package (proc_callers.rs): what a proc macro called
    /// in a crate of that package is shown.
    placed: crate::proc_callers::Placed<'f>,
    /// Crates whose names were resolved this command, and the unresolved paths among them.
    pub resolved: u32,
    pub unresolved: u64,
}

/// A checkout file a package's names read that no units record and no file's outline lists
/// (the machine's `DriveError::NoPart`, which names the file by its digest alone).
#[derive(Debug)]
pub struct Unlisted {
    pub package: String,
    /// The file's path (its slot's: `x.rs` for `x/mod.rs` too), checkout-relative.
    pub path: String,
}

impl core::fmt::Display for Unlisted {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "the names of package `{}` read `{}`, which no manifest's targets and no file's `mod` lines list (a module under a macro, an attribute or a `#[path]` the outline does not follow): its items cannot be checked",
            self.package, self.path
        )
    }
}

impl<'f> Names<'f> {
    /// The world the names are of: its crates in index order, their texts (body_types.rs).
    pub(crate) fn world(&self) -> &World<'f> {
        &self.world
    }

    /// The unit graph the world was made from.
    pub(crate) fn graph(&self) -> &'static blaze_unit_graph::UnitGraph<'static> {
        self.graph
    }

    /// The command's proc-macro runner, once made.
    pub(crate) fn procs(&self) -> Option<&'static crate::proc_runner::ProcRunner<'static>> {
        self.procs
    }

    /// Sets the runner, once a command.
    pub(crate) fn set_procs(&mut self, procs: &'static crate::proc_runner::ProcRunner<'static>) {
        self.procs = Some(procs);
    }

    /// Where the world placed each locked package (proc_callers.rs).
    pub(crate) fn placed(&self) -> &crate::proc_callers::Placed<'f> {
        &self.placed
    }
}

impl Names<'_> {
    /// The package whose names read the checkout file `file` names, and the file's path: the
    /// first in subject order. For an error that would name the file by its digest.
    pub(crate) fn reader_of(&self, file: Name) -> Option<Unlisted> {
        for (subject, resolved) in self.made.iter() {
            let Some(path) = resolved.files.iter().find(|path| Name::file(path.as_bytes()) == file) else { continue };
            let at = self.by_subject.binary_search_by_key(subject, |(subject, _)| *subject).ok()?;
            return Some(Unlisted { package: String::from(self.world.crates[self.by_subject[at].1].package), path: path.clone() });
        }
        None
    }

    /// The checkout files the crate `subject`'s resolution read this command (checkout-relative),
    /// and each file it read as data with its home's slot: what its answer asks for as its
    /// surfaces and data records (scope.rs `answer`). `None` before it was resolved.
    pub(crate) fn read_files(&self, subject: ContentId) -> Option<(&[String], &[(String, String)])> {
        let at = self.made.binary_search_by_key(&subject, |(subject, _)| *subject).ok()?;
        Some((&self.made[at].1.files, &self.made[at].1.data))
    }

    /// The `Script` name the crate at world index `at` reads, if its package has a build script.
    pub(crate) fn script_of(&self, at: usize) -> Option<Name> {
        self.scripts.binary_search_by_key(&at, |(krate, _)| *krate).ok().map(|i| self.scripts[i].1)
    }

    /// The world index of the crate whose `CrateScope` subject is `subject`.
    pub(crate) fn crate_at(&self, subject: ContentId) -> Option<usize> {
        self.by_subject.binary_search_by_key(&subject, |(subject, _)| *subject).ok().map(|i| self.by_subject[i].1)
    }

    /// The crate at world index `at` as it is printed: its crate name, and whether it is the
    /// macro side's copy.
    pub(crate) fn crate_label(&self, at: usize) -> (&str, bool) {
        let krate = &self.world.crates[at];
        (krate.input.name, krate.side == blaze_world::Side::Macro)
    }

    /// The world indices of the crates that read the `Script` run `script`, in index order.
    pub(crate) fn script_readers(&self, script: Name) -> impl Iterator<Item = usize> + '_ {
        self.scripts.iter().filter(move |(_, name)| *name == script).map(|(at, _)| *at)
    }

    /// The build-script file (package-relative) of the crate at world index `at`.
    pub(crate) fn script_root(&self, at: usize) -> Option<&str> {
        self.script_roots.binary_search_by_key(&at, |(krate, _)| *krate).ok().map(|i| self.script_roots[i].1)
    }

    /// Each `Script` run's (subject, the reading crate's world key, its placement): its part
    /// source's inputs (scripts.rs `parts`).
    pub(crate) fn script_keys(&self) -> impl Iterator<Item = (ContentId, ContentId, crate::scripts::RanAs)> + '_ {
        self.scripts.iter().zip(self.script_placed.iter()).map(|((at, name), ran_as)| (name.subject, self.world.crates[*at].input.key.0, *ran_as))
    }

    /// The `Script` part's name (kind byte, subject) of the crate at world index `at`, when its
    /// build script's run is a task: the crate list's script column (graph_payload.rs).
    pub(crate) fn script_name_of(&self, at: usize) -> Option<[u8; 33]> {
        let (_, name) = self.scripts.iter().find(|(of, _)| *of == at)?;
        let mut bytes = [0; 33];
        bytes[0] = name.kind as u8;
        bytes[1..].copy_from_slice(&name.subject.0);
        Some(bytes)
    }

    /// Every build-script module lookup of the world, sorted by subject (repeats adjacent).
    pub(crate) fn script_modules(&self) -> &[crate::script_modules::ScriptModule] {
        &self.script_modules
    }

    /// The module lookup the script read by the crate at world index `at` Needs.
    pub(crate) fn script_module_of(&self, at: usize) -> Option<&crate::script_modules::ScriptModule> {
        self.script_modules.iter().find(|module| module.crate_at == at)
    }

    /// The build-script module lookup of `subject`, if it is one (else a proc-macro crate's).
    pub(crate) fn script_module(&self, subject: ContentId) -> Option<&crate::script_modules::ScriptModule> {
        let at = self.script_modules.partition_point(|module| module.subject < subject);
        self.script_modules.get(at).filter(|module| module.subject == subject)
    }
}

/// Each workspace member's lock entry and directory, anchor-relative, from the layout the
/// command read: the graph job's when it was made, else the members matched to the lock;
/// `None` when the members were not matched to a lock this command.
pub(crate) fn member_dirs<'l>(laid: Option<&'l crate::units::Laid<'_>>) -> Option<&'l [(LockId, blaze_unit_graph::Text<'static>)]> {
    let laid = laid?;
    match (&laid.graph, &laid.parts) {
        (Some(graph), _) => Some(graph.member_dirs()),
        (None, Some(parts)) => Some(parts.member_dirs()),
        (None, None) => None,
    }
}

/// The names world of a command, made from the graph record `graph_record`; `members` is each
/// workspace member's lock entry and directory, anchor-relative ([`member_dirs`]).
pub(crate) fn world<'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    graph_record: &[u8],
    lock: Option<&'f Lock<'static>>,
    members: Option<&[(LockId, blaze_unit_graph::Text<'_>)]>,
    env: &FetchEnv<'_>,
) -> Result<Names<'f>, NamesError> {
    // The lock as given, `'static` inside: the fetch path below is typed at `'static` so the
    // places it answers can be shown to the proc-macro sandbox (proc_callers.rs).
    let lock_whole: &'f Lock<'static> = lock.ok_or(NamesError::NoLock)?;
    let lock: &'f Lock<'f> = lock_whole;
    let members = members.ok_or(NamesError::NoMembers)?;
    // The command's allocator for the world and the records: the process's heap, handed from
    // `main` in `env`, held to the end.
    let heap: Alloc<'static> = env.heap.alloc();
    let alloc: Alloc<'f> = heap;
    let Some(home) = env.home else { return Err(NamesError::NoHome) };
    let decoded = blaze_unit_graph::record::decode(graph_record, heap).map_err(NamesError::Graph)?;
    let decoded: &'static blaze_unit_graph::record::Decoded<'static> = std::boxed::Box::leak(std::boxed::Box::new(decoded));
    let graph = &decoded.graph;
    // The files read for the names step live to the end of the command: a context of the
    // command's (its deadline) and a frame of their own, both held to the end.
    let cx: &'static Cx = std::boxed::Box::leak(std::boxed::Box::new(cx.until(cx.deadline())));
    let frame: &'static Frame = std::boxed::Box::leak(std::boxed::Box::new(Frame::with_reserve(NAMES_FRAME, bstd::mem::Tag::Other).map_err(|_| NamesError::Frame)?));
    let mut shelf = crate::toolchain::HomeShelf::open(cx, &env.fs, home, frame).map_err(NamesError::Shelf)?;
    let reader: &'static Reader = std::boxed::Box::leak(std::boxed::Box::new(Reader { cx, root: root.read_only(), frame }));
    let sources = StoreSources::new(lock_whole, Folder::of(reader, env)?, PackView::open_source_put, heap)
        .map_err(|e| NamesError::Sources(std::format!("{e}")))?;
    let sources = std::boxed::Box::leak(std::boxed::Box::new(sources));
    // Every path package the graph reaches is a member, placed at its own directory (the
    // layout's, anchor-relative: `""` for the root's own package), so its root file and its
    // `mod x;` files are read under it. A lock may name path packages no unit reaches: they are
    // never placed. Two members in one directory, or a reached path package that is no member
    // (a path dependency outside the workspace root), is refused by name: nothing is guessed.
    let mut by_dir: blaze_own::Vec<(&str, &str), &Frame> = blaze_own::Vec::with_capacity_in(members.len(), frame);
    for (id, dir) in members.iter() {
        by_dir.push((dir.as_str(), lock.package(*id).name.as_str()));
    }
    by_dir.sort_unstable();
    if let Some(pair) = by_dir.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(NamesError::SharedDirectory { first: String::from(pair[0].1), second: String::from(pair[1].1), dir: String::from(pair[0].0) });
    }
    let reached = |name: &str| graph.units.iter().any(|unit| unit.name.as_str().bytes().map(|b| if b == b'-' { b'_' } else { b }).eq(name.bytes().map(|b| if b == b'-' { b'_' } else { b })));
    let mut paths: std::vec::Vec<(LockId, &'static str), Global> = std::vec::Vec::new();
    for at in 0..lock.packages.len() {
        let id = LockId(at as u32);
        let package = lock.package(id);
        if !matches!(package.source, LockSource::Path) || !reached(package.name.as_str()) {
            continue;
        }
        let Some((_, dir)) = members.iter().find(|(member, _)| *member == id) else {
            // The first unit that depends on a unit of this package, by the graph's order.
            let dependent = graph
                .units
                .iter()
                .find(|unit| unit.deps.iter().any(|dep| graph.units[dep.unit.0 as usize].package == id))
                .map(|unit| String::from(unit.name.as_str()));
            return Err(NamesError::PathNotMember { package: String::from(package.name.as_str()), dependent });
        };
        // Held to the end of the command, in the names frame, as the files read under it are.
        let held: &'static blaze_own::String<&'static Frame> = blaze_own::Box::leak(blaze_own::Box::new_in(blaze_own::String::from_str_in(dir.as_str(), frame), frame));
        paths.push((id, held.as_str()));
    }
    let paths: &'f [(LockId, &'static str)] = std::vec::Vec::leak(paths);
    let checkout: &'f ReadTree = std::boxed::Box::leak(std::boxed::Box::new(ReadTree(reader)));
    let mut fetched = Fetched { store: sources, paths, lock: lock_whole, alloc: heap };
    // Every place the world is told, kept for the proc-macro runner (proc_callers.rs).
    let mut packages = crate::proc_callers::Placing::new(&mut fetched, alloc);
    // The crate list (blaze-scope's units list, the one home) and the toolchain crates it needs.
    let listed = blaze_world::open_list(&mut shelf, env.toolchain, crate::graph::TRIPLE, graph, lock, &mut packages, heap).map_err(|e| match e {
        WorldError::Toolchain(_) => NamesError::Toolchain(std::format!("{e}")),
        other => NamesError::World(std::format!("{other}")),
    })?;
    let listed: &'f Listed<'f, crate::toolchain::Held<'static>> = blaze_own::Box::leak(blaze_own::Box::new_in(listed, Global));
    let world = blaze_world::world(graph, lock, listed, &mut packages, checkout, heap)
        .map_err(|e| NamesError::World(std::format!("{e}")))?;
    let placed = packages.finish();
    let mut by_subject = std::vec::Vec::new();
    let mut by_key = std::vec::Vec::new();
    for (at, krate) in world.crates.iter().enumerate() {
        by_subject.push((blaze_drive::key::crate_scope_name(&krate.input.key).subject, at));
        by_key.push((krate.input.key, at));
    }
    by_subject.sort_unstable();
    by_key.sort_unstable();
    // A registry package with a checksum in the lock: its archive is known by content, so its
    // crate's records are keyed by nothing of this project. A path or git package, a member,
    // or a registry package with no checksum stays the project's.
    let from_archive = |unit: blaze_unit_graph::UnitId| {
        let package = lock.package(graph.units[unit.0 as usize].package);
        matches!(package.source, LockSource::Registry { .. }) && matches!(package.checksum, blaze_manifest::lock::Checksum::Sha256(_))
    };
    let mut public = std::vec::Vec::new_in(Global);
    public.extend(
        world
            .crates
            .iter()
            .filter(|krate| match krate.origin {
                blaze_world::Origin::Toolchain(_) => true,
                blaze_world::Origin::Unit(unit) => from_archive(unit),
            })
            .map(|krate| blaze_drive::key::crate_scope_name(&krate.input.key).subject),
    );
    // Each crate that reads a build script's run, of ITS side (ruling D115 (2)): the subject
    // is the world's (`blaze_world::script_subject`). Crate order, so sorted by index.
    let mut scripts = std::vec::Vec::new_in(Global);
    for (at, krate) in world.crates.iter().enumerate() {
        if let blaze_world::Script::Read { subject } = krate.script {
            scripts.push((at, Name::new(Kind::Script, subject)));
        }
    }
    // Each crate's build-script file, from the unit it is made from ([`script_units`]): the
    // script a crate reads is one (ruling D115 (3); `blaze_world::list::one_script` refuses two
    // units under one key with different runs), so its origin unit's script names it.
    // And its module's lookup (script_modules.rs, ruling D115 (c)): the script UNIT's package,
    // version and features, what the bootstrap builder's identity names.
    // Crate order, so `script_roots` and `placements` are sorted by index, each crate once.
    let mut script_roots = std::vec::Vec::new_in(Global);
    let mut script_modules = std::vec::Vec::new_in(Global);
    let mut placements = std::vec::Vec::new_in(Global);
    for (at, script) in script_units(world.crates.iter().map(|krate| krate.origin), |unit| graph.units[unit.index()].build_script) {
        // The module is looked up for the run's placement (D112): a native run's record is a
        // host binary, a wasm run's a module.
        let ran_as = crate::scripts::placement(graph, script, heap);
        placements.push((at, ran_as));
        let script = &graph.units[script.index()];
        script_roots.push((at, script.root.as_str()));
        let mut features = std::vec::Vec::new_in(Global);
        features.extend(script.features.iter().map(|feature| feature.as_str()));
        let subject = crate::script_modules::subject(ran_as, script.name.as_str(), script.version.as_str(), &features);
        script_modules.push(crate::script_modules::ScriptModule { subject, crate_at: at, ran_as, package: script.name.as_str(), version: script.version.as_str(), features });
    }
    script_modules.sort_unstable_by_key(|module| (module.subject, module.crate_at));
    // Each run's placement, and the public list's script subjects (ruling D115 sharing (i)):
    // a public crate's run is added only when placed on the wasm path; one not added stays
    // this machine's, and so does every record reading it (the safe direction).
    let crates_public = public.len();
    let mut script_placed = std::vec::Vec::with_capacity_in(scripts.len(), Global);
    for (at, name) in scripts.iter() {
        let Ok(i) = placements.binary_search_by_key(at, |(krate, _)| *krate) else {
            return Err(NamesError::World(std::format!(
                "the crate `{}` reads a build script's run, and no unit of it has a build script (ruling D115 (3))",
                world.crates[*at].input.name
            )));
        };
        let ran_as = placements[i].1;
        script_placed.push(ran_as);
        let crate_public = public[..crates_public].contains(&blaze_drive::key::crate_scope_name(&world.crates[*at].input.key).subject);
        if crate::scripts::run_is_public(crate_public, ran_as) {
            public.push(name.subject);
        }
    }
    Ok(Names { world, by_subject, by_key, made: std::vec::Vec::new(), public, scripts, script_placed, script_roots, script_modules, prepares: Prepares::new_in(alloc), graph, procs: None, placed, resolved: 0, unresolved: 0 })
}

/// Each crate (by world index, in order) made from a unit with a build script, with that
/// build-script unit: `origins` is each crate's [`blaze_world::Origin`] in world order and
/// `build_script` a unit's build-script unit.
///
/// Taken from the crates, never from `World::crate_of_unit`: that map holds only the target
/// side, so a macro-side copy (a member proc macro's dependency, ruling D115 (2)) that reads
/// its side's run had no placement and the world was refused as "no unit of it has a build
/// script" (tempo, 2026-11-16: `icu_properties_data`, reached from a member proc macro).
pub(crate) fn script_units<'a>(
    origins: impl Iterator<Item = blaze_world::Origin> + 'a,
    build_script: impl Fn(blaze_unit_graph::UnitId) -> Option<blaze_unit_graph::UnitId> + 'a,
) -> impl Iterator<Item = (usize, blaze_unit_graph::UnitId)> + 'a {
    origins.enumerate().filter_map(move |(at, origin)| match origin {
        blaze_world::Origin::Unit(unit) => build_script(unit).map(|script| (at, script)),
        blaze_world::Origin::Toolchain(_) => None,
    })
}

/// Tells `store` the world's public crates (the toolchain's and checksummed registry
/// archives'), whose records hold nothing of a project when every name they read is public
/// too: they go to the machine store (remote-read.md, "One chain of stores"). Called wherever
/// a world is made.
pub(crate) fn tell_public(store: &mut dyn Store<'_>, names: &Names<'_>) {
    store.public_crates(&names.public);
}

/// The `CrateScope` parts the graph's record names: one per crate of the world, by subject,
/// its source the same key's digest. Owed (drive.md, RUNNER 16): every part runs, so a
/// `#![no_core]` workspace still resolves all 15 toolchain crates cold; a `Need` locates its
/// name among its home's parts, so a toolchain crate cannot simply be left out here: the
/// drive needs a part that is located but run only when wanted.
pub(crate) fn parts(names: &Names<'_>) -> std::vec::Vec<(Name, ContentId), Global> {
    let mut parts = std::vec::Vec::with_capacity(names.world.crates.len());
    for krate in names.world.crates.iter() {
        parts.push((blaze_drive::key::crate_scope_name(&krate.input.key), ContentId::of(SOURCE_DOMAIN, &krate.input.key.0.0)));
    }
    parts.sort_unstable_by_key(|(name, _)| name.subject);
    parts.dedup_by_key(|(name, _)| name.subject);
    parts
}

/// The surface name of a checkout file.
/// The name a file the resolver read is listed by: the slot `a/x.rs` for `a/x/mod.rs` found
/// through `mod x;`, else the path itself. A `mod.rs` file is a slot's exactly when the
/// resolve also looked at that slot (`probed`): `mod x;` looks at `a/x.rs` before
/// `a/x/mod.rs`, while a crate root (`[lib] path = "mod.rs"`) and a `#[path]` naming a `mod.rs`
/// file are opened by their own path and listed by it (units.rs, work.rs `module_file`).
/// (Before 2026-11-16 every `mod.rs` was mapped: `hir-ty-codegen`'s root, `crates/
/// hir-ty-codegen/mod.rs`, was asked for as `crates/hir-ty-codegen.rs`, which nothing lists.)
/// Owed: the texts interface saying how it reached a file, so this is told, not inferred; a
/// resolver that looked at `x/mod.rs` first would end in `Unlisted`, by name, never quietly.
fn slot_of(path: String, probed: impl Fn(&str) -> bool) -> String {
    match path.strip_suffix("/mod.rs") {
        Some(stem) => {
            let mut slot = String::with_capacity(stem.len() + 3);
            slot.push_str(stem);
            slot.push_str(".rs");
            if probed(&slot) { slot } else { path }
        }
        None => path,
    }
}

fn surface(path: &str) -> Name {
    Name::new(Kind::Surface, Name::file(path.as_bytes()).subject)
}

#[cfg(test)]
mod script_units_tests;

#[cfg(test)]
pub(crate) use scope::scope_made;
pub(crate) use scope::{Prepared, Resolution, SCOPE_FRAME, SCOPE_JOBS, ScopeJob, ScopeRound, answer, prepare, resolve, run, settle};
