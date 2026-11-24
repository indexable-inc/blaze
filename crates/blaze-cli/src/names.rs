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

use std::alloc::Global;
use std::string::String;

use blaze_drive::{Kind, Name, TaskFailure, Why};
use blaze_fetch::SourceId;
use blaze_fetch::pack::PackView;
use blaze_ids::ContentId;
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::Alloc;
use blaze_unit_graph::{Records, StoreSources};
use blaze_world::crate_list::{CrateGraph, CrateList};
use blaze_world::{Fetched, Listed, Tree, World, WorldError};
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadOnly, ReadWrite};
use bstd::path::Path;
use bstd::mem::{Bytes, Frame};

use crate::CliError;
use crate::fetch::FetchEnv;
use crate::store::Store;

pub(crate) mod expansion_texts;
pub(crate) mod failed_call;
pub(crate) mod module_missing;
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
// itself); "13": a cell table whose `Module` cell still held the module's textual macro list and
// marks, with no `Textual` or `Incoherent` cell (lane BH1: blaze-scope names_cells.rs, cell
// digest domain 2), whose readers would read those cells absent) are under other names, never
// found, made again. Source 14 reported every unresolved path as a missing name.
// The subject's domain is blaze-drive key.rs `CRATE_SCOPE_DOMAIN` (the name's one home,
// `crate_scope_name`): it names the crate's task, not its record's form, so a form change
// moves this source domain alone (every key moves; the name a reader reads by stays). Bump it
// there only when what a name stands for changes. Source 18: D104 implicit `fn main` reads on
// blaze-edition binary roots (engine_names.rs `finish`, rule 21) and expansion record keys in
// `OUT_DOMAIN` 3.
const SOURCE_DOMAIN: &str = "blaze-cli crate scope source 18";
/// The out: the names record, the crate's expansion record keys (`EXPANSIONS_OUT_DOMAIN`), AND
/// the checkout files the crate holds ([`crate_out`]). "1" was the names record alone, but a
/// body's check reads its crate's items record to find which crate owns its file (body_types.rs
/// `prepare`): a module file created gave a new record at an EQUAL out, so a reader handed the
/// old record (settled standing, then made again: the drive does not re-enter readers of an
/// equal out) kept "no crate's names record lists this file" while a fresh home did not (rule 21;
/// main.rs `edits_of_module_files_answer_as_a_fresh_home`). "3" adds each macro expansion call's
/// content record key so editing inside a macro-expanded item (such as D104's implicit `fn main`)
/// moves `CrateScope`'s `WHOLE` out too.
const OUT_DOMAIN: &str = "blaze-cli crate scope out 3";
const EXPANSIONS_OUT_DOMAIN: &str = "blaze-cli crate scope expansions out 1";
/// The rule named by resolver refusals from both check drivers.
pub(crate) const RULE: &str = "names.rs";


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
    /// The crate list (blaze-world crate_list.rs) was not written from the world, or does not
    /// open: a version 5 list of an old home is refused here, and the graph is made again (D160).
    List(blaze_world::crate_list::ListError),
    /// A row of the crate list whose planners' fact this crate cannot read (`what`): a script
    /// part of another kind than `Script`, a placement byte no `RanAs` has.
    ListFact { row: usize, what: &'static str },
    /// Under test only: the list just written answers otherwise than the world it was written
    /// from ([`Names::list_differences`], the oracle every test world's discovery runs).
    ListDiffers { differences: usize },
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
            NamesError::List(error) => write!(f, "the crate list: {error}"),
            NamesError::ListFact { row, what } => write!(f, "the crate list's row {row}: {what} (names.rs `of_list`)"),
            NamesError::ListDiffers { differences } => {
                write!(f, "the crate list written answers {differences} fact(s) otherwise than its own world (names.rs `differences_from`, the tests' oracle)")
            }
        }
    }
}

/// What a request's names world was made by need for ([`Names::made_or_make`]): each its own
/// span, so the profile says when the world was made and why (rule 15; engine.md item 2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NeededBy {
    /// A `CrateScope` task entered (engine_names.rs).
    NamesTask,
    /// A `Script` job made (scripts.rs `job`).
    ScriptJob,
    /// A build script's module lookup made (engine_scripts.rs `prepare_lookup`).
    ModuleLookup,
}

impl NeededBy {
    fn span(self) -> &'static str {
        match self {
            NeededBy::NamesTask => "names: make the world (needed by a names task)",
            NeededBy::ScriptJob => "names: make the world (needed by a script job)",
            NeededBy::ModuleLookup => "names: make the world (needed by a build script's module lookup)",
        }
    }
}

/// Why the world a request's crate list stands for was not made by need (rule 19): what needed
/// it, and why not.
#[derive(Debug)]
pub(crate) struct WorldRefused {
    pub(crate) needed_by: NeededBy,
    pub(crate) why: WorldWhy,
}

#[derive(Debug)]
pub(crate) enum WorldWhy {
    /// These names were made from a world, not read from a list: nothing to make from.
    NoMaker,
    /// The graph's record named by the list's maker was not read back again.
    Back(crate::engine_units::BackRefused),
    /// The world was not made from it.
    Make(NamesError),
    /// The world made holds another crate at `row` than the list (or another count): the list
    /// and the world under one graph key disagree (rule 21), refused, never mixed.
    ListMoved { row: usize, crates: usize, rows: usize },
    /// The world made answers `differences` facts otherwise than the list the request planned
    /// from ([`Names::list_differences`]): under one graph key (rule 21), refused, never mixed.
    ListDiffers { differences: usize },
}

impl core::fmt::Display for WorldRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the names world, needed by {:?}, was not made (names.rs `made_or_make`): ", self.needed_by)?;
        match &self.why {
            WorldWhy::NoMaker => f.write_str("these names hold no graph record to make it from"),
            WorldWhy::Back(why) => write!(f, "{why}"),
            WorldWhy::Make(why) => write!(f, "{why}"),
            WorldWhy::ListMoved { row, crates, rows } => {
                write!(f, "the world made holds another crate at row {row} than the crate list ({crates} crates, {rows} rows): the list and the world under one graph key disagree (rule 21)")
            }
            WorldWhy::ListDiffers { differences } => {
                write!(f, "the world made answers {differences} fact(s) otherwise than the crate list the request planned from: the list and the world under one graph key disagree (rule 21)")
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
        // Held to the end of the command, in the names frame: the bytes and their header both
        // (the header was a box of the global heap until 2026-11-21).
        let held: &'static Bytes<'static> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(bytes, self.frame));
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
/// pins: each MAPPED read-only in place, never read whole into a frame (it was until
/// 2026-11-20: every command copied every fetched package's record, about 920 on tempo, into
/// the names frame). A record there is written once and only ever renamed into place
/// (fetch.rs `write_record`), so `Dir::map_read`'s contract holds, as for the unit graph's
/// mapping of the same records (fetch.rs `Lockfile::map_source`).
///
/// THE ONE PROOF A FETCHED SOURCE IS STILL THERE (rule 74): this map. Every package the world
/// places is mapped here once a command; one that is gone (removed by hand, or by a cleanup
/// that ignored the answer's roots) is no mapping, so its package is not placed, the world is
/// not made, and the units stage discovers in full (engine_units.rs `UnitsCause::World`), where
/// the fetch's own stat (`fetch_stands`) finds it gone and fetches it again. No other stat of
/// the sources store is made on a read back.
struct Folder {
    cx: &'static Cx,
    store: Dir<ReadOnly>,
    /// Where each mapping is held to the end of the command.
    heap: Alloc<'static>,
    opened: &'static Opened,
}

/// What making this world did, for the profile's `plan:` line (engine_units.rs `PlanRows`): each
/// counted where it is done ([`world`]). One `Opened` a world; a world is made at most once a
/// command on the units path, so these are the command's counts there.
#[derive(Default)]
pub(crate) struct Opened {
    /// The graph record decoded ([`world`]: one; floor 0 once the world is read in place).
    pub(crate) graph_decodes: core::cell::Cell<u32>,
    /// The fetched records mapped in place ([`Folder`]), and their bytes.
    pub(crate) records_mapped: core::cell::Cell<u32>,
    pub(crate) mapped_bytes: core::cell::Cell<u64>,
}

impl Folder {
    /// The sources store opened read only, with `checkout`'s context.
    fn of(checkout: &'static Reader, env: &FetchEnv<'_>, opened: &'static Opened) -> Result<Folder, NamesError> {
        let sources = crate::fetch::sources_dir(checkout.cx, env).map_err(|error| NamesError::Sources(error.to_string()))?;
        Ok(Folder { cx: checkout.cx, store: sources.read_only(), heap: env.heap.alloc(), opened })
    }
}

impl<'b> Records<'b> for Folder {
    fn record(&mut self, source: &SourceId<'_>) -> Option<&'b [u8]> {
        let hex = crate::fetch::key_hex(&source.key());
        let path = Path::new(core::str::from_utf8(&hex).ok()?).ok()?;
        // None is the package's `NotFetched` (blaze-unit-graph `SourceError`), named by its
        // package and key: no world is made, and the units stage says why (engine_units.rs).
        let mapping = self.store.map_read(self.cx, &path).ok()?;
        // Held to the end of the command: the world and every reader of its packs borrow it.
        let mapping: &'static blaze_core::Mapping = blaze_own::Box::leak_static(blaze_own::Box::new_in(mapping, self.heap));
        let bytes: &'static [u8] = mapping.bytes();
        self.opened.records_mapped.set(self.opened.records_mapped.get().saturating_add(1));
        self.opened.mapped_bytes.set(self.opened.mapped_bytes.get().saturating_add(bytes.len() as u64));
        Some(bytes)
    }
}

/// One crate made this command: its payload, out, and the checkout files its resolution read.
pub(crate) struct Resolved {
    /// The record's payload ([`sections::write`]).
    payload: blaze_own::Vec<u8, Global>,
    /// The digest both check drivers publish for this resolution.
    pub(crate) out: ContentId,
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
    /// Each unresolved path of the crate's own code with its rule ([`rule_of`]).
    findings: PathFindings,
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
    /// Each `mod` of the crate's own code whose file the walk did not find, as its record's
    /// error finding at the `mod` item ([`module_missing::findings`], E0583).
    module_missing: blaze_own::Vec<module_missing::ModuleMissing, blaze_own::SharedAlloc<'static>>,
    /// What the tests record says about single functions of the crate's own code, each with
    /// its rule ([`test_marks::findings`]).
    test_marks: blaze_own::Vec<(u16, Unresolved), Global>,
    /// The crate's expansion records, which its payload's directory names: put after its
    /// record ([`crate::work::Record::extras`]).
    expansions: crate::work::Extras,
}

impl Resolved {
    /// The engine worker takes its store key and wire length from these exact bytes.
    pub(crate) fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Checkout inputs for the engine worker's surface, absent-surface and data reads.
    pub(crate) fn checkout_reads(&self) -> (&[String], &[String], &[(String, String)]) {
        (&self.files, &self.absent, &self.data)
    }
}

/// A resolved crate's payload ([`sections::write`]) with its unresolved paths as findings.
struct ScopeRecord {
    payload: blaze_own::Vec<u8, Global>,
    findings: PathFindings,
    unresolved: u32,
    /// Why the tests section is empty though the unit is a test unit.
    tests_refused: Option<std::string::String>,
    /// What the tests record says about single functions ([`test_marks::findings`]).
    test_marks: blaze_own::Vec<(u16, Unresolved), Global>,
    /// Each distinct expansion record the payload's directory names, copied out of the task's
    /// frame into `Global` with the payload.
    expansions: crate::work::Extras,
    /// Position-independent digest of `(call, record_key)` over the expansions directory, folded
    /// into `CrateScope`'s `out` (`OUT_DOMAIN` 3) so expansion body edits move `WHOLE` (rule 21).
    expansions_out: ContentId,
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
/// payload. `kept` holds findings that leave the worker with the crate's record.
/// `waits`: what in the crate waits on a failed dependency (names/scope.rs `resolve_metered`,
/// blaze-scope waits_on.rs); such a path is counted, not found (plan row A8).
fn scope_record<'a>(done: &blaze_scope::crate_scope::CrateScope<'a>, crate_name: &str, member: bool, waits: &blaze_scope::waits_on::WaitsOn<'_, 'a>, bump: blaze_own::Bump<'a>, kept: blaze_own::SharedAlloc<'static>) -> Result<ScopeRecord, Unwritten> {
    let alloc = bump.alloc();
    let unwritten = |section: &'static str, why: std::string::String| Unwritten { crate_name: crate_name.into(), section, why };
    let mut findings = PathFindings::new_in(kept);
    let unresolved = done.names.notes.len() as u32 + unresolved(done.names, member, waits, &mut findings);
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
    let test_marks = match (member, &tests_record) {
        (true, Some(section)) => test_marks::findings(done.names, section),
        _ => blaze_own::Vec::new_in(Global),
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
    let dir = blaze_scope::expansion_records::Directory::open(expansions)
        .map_err(|error| unwritten("expansions", blaze_own::format_in!(alloc, "{error}").as_str().into()))?;
    let calls = u32::try_from(dir.len())
        .map_err(|_| unwritten("expansions", "more than a u32 of macro calls".into()))?;
    let mut expansions_hash = blaze_ids::Hasher::new(EXPANSIONS_OUT_DOMAIN);
    expansions_hash.update(&u64::from(calls).to_le_bytes());
    for i in 0..dir.len() {
        let entry = dir
            .entry_at(i)
            .map_err(|error| unwritten("expansions", blaze_own::format_in!(alloc, "{error}").as_str().into()))?;
        expansions_hash.id(&entry.call.0).id(&entry.record);
    }
    let expansions_out = expansions_hash.finish();
    let extras = crate::work::Extras { records, calls, released: None };
    // The crate's link table, from the names resolved here (run.md, "the native image as a
    // task"): a refused table is written as its refusal (state 1), so only a program that links
    // the crate fails; an error here is a table that could not be written at all.
    let mut links = blaze_own::AVec::new_in(alloc);
    blaze_ladder::native_links::scope_table(done.names, &mut links, bump).map_err(|error| unwritten("link table", std::format!("{error:?}")))?;
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
    let cells = blaze_scope::names_cells::crate_table(done.names, &names_record, &items_record, &wholes, bump).map_err(|error| unwritten("cell table", std::format!("{error:?}")))?;
    // Section::Inner is omitted (&[]) on disk: no reader consumes it (only `names/scope.rs`
    // hashes `done.inner` in memory into the task's `out` digest). Keeping the 8-section wire
    // header avoids bumping the format while cutting 14-18% off every stored CrateScope record.
    let payload = sections::write(unresolved, [&done.record[..], &[], &names_record[..], &items_record[..], tests, expansions, &links[..], &cells[..]]);
    #[cfg(test)]
    sections::watch_tests(crate_name, &payload, tests);
    Ok(ScopeRecord { payload, findings, unresolved, tests_refused, test_marks, expansions: extras, expansions_out })
}

/// No binding was found for this segment of a `use` or macro path.
pub(crate) const UNRESOLVED: u16 = 14;
pub(crate) const UNRESOLVED_PRIVATE: u16 = 28;
pub(crate) const UNRESOLVED_AMBIGUOUS: u16 = 29;
pub(crate) const UNRESOLVED_NOT_A_MODULE: u16 = 30;
pub(crate) const UNRESOLVED_SUPER_ABOVE_ROOT: u16 = 31;
pub(crate) const UNRESOLVED_DOLLAR_CRATE: u16 = 32;
pub(crate) const UNRESOLVED_WRONG_MACRO_KIND: u16 = 33;
pub(crate) const UNRESOLVED_DERIVE_AFTER_ATTRIBUTE: u16 = 34;

fn rule_of(why: blaze_surface::Unresolved) -> u16 {
    use blaze_surface::Unresolved as Reason;
    match why {
        Reason::NotFound => UNRESOLVED,
        Reason::Private => UNRESOLVED_PRIVATE,
        Reason::Ambiguous(_) => UNRESOLVED_AMBIGUOUS,
        Reason::NotAModule => UNRESOLVED_NOT_A_MODULE,
        Reason::NoParent => UNRESOLVED_SUPER_ABOVE_ROOT,
        Reason::NoCrate => UNRESOLVED_DOLLAR_CRATE,
        Reason::WrongMacroKind => UNRESOLVED_WRONG_MACRO_KIND,
        Reason::DeriveAfterAttrMacro => UNRESOLVED_DERIVE_AFTER_ATTRIBUTE,
    }
}

/// Whether a finding of `rule` (blaze-cli's area) is placed by [`place_pieces`]: its message
/// starts with the file, the item chain and the edition. The one list of them.
pub(crate) fn placed(rule: u16) -> bool {
    matches!(rule,
        UNRESOLVED | UNRESOLVED_PRIVATE | UNRESOLVED_AMBIGUOUS | UNRESOLVED_NOT_A_MODULE |
        UNRESOLVED_SUPER_ABOVE_ROOT | UNRESOLVED_DOLLAR_CRATE | UNRESOLVED_WRONG_MACRO_KIND |
        UNRESOLVED_DERIVE_AFTER_ATTRIBUTE | include_outside::INCLUDE_OUTSIDE_PACKAGE |
        failed_call::FAILED_CALL | test_marks::TEST_REFUSED | test_marks::TEST_DUPLICATED |
        blaze_drive::task::MODULE_FILE_MISSING_RULE | blaze_drive::task::PATH_FILE_MISSING_RULE
    )
}

/// Whether a names finding of `rule` (blaze-cli's area) is a warning: drawn after the answer,
/// never an error (main.rs `every_rule`, print.rs). The one list of them.
pub(crate) fn warning(rule: u16) -> bool {
    matches!(rule, include_outside::INCLUDE_OUTSIDE_PACKAGE | test_marks::TEST_DUPLICATED)
}

/// A placed include, failed call or test mark. Its message starts with [`place_pieces`].
struct Unresolved {
    at: (u32, u32),
    message: blaze_own::Vec<u8, Global>,
}

/// Each path's report rule and its item-relative finding, kept until the crate's record is written.
type PathFindings = blaze_own::Vec<(u16, PathFinding), blaze_own::SharedAlloc<'static>>;

/// The failing segment's range from the item's start. The message holds the file, item chain,
/// edition and [`unresolved_args`], separated by 0x1F for [`place_pieces`].
struct PathFinding {
    at: (u32, u32),
    message: blaze_own::Vec<u8, blaze_own::SharedAlloc<'static>>,
}

/// The pieces before the arguments in a [`placed`] finding's message.
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

/// Every `use` and macro path of crate `c` that the resolver refused. The project's own (a
/// checkout file, `/0/`) are findings in `own`, sorted; the number of the others (a fetched or
/// toolchain crate's: not the project's to fix) is returned, counted in one note. A path that
/// waits on a failed dependency (`waits`: written into it, or failing in a scope a glob or
/// import into it left unknown) is counted too, never a finding: that crate's failure is
/// reported once, at it (plan row A8, rule 19).
fn unresolved<'a>(c: &blaze_scope::resolve::CrateNames<'a>, member: bool, waits: &blaze_scope::waits_on::WaitsOn<'_, 'a>, own: &mut PathFindings) -> u32 {
    use blaze_surface::ScopeError;
    let project = member && c.files.iter().any(|file| file.path.starts_with("/0/"));
    let mut elsewhere = 0u32;
    for r in c.done.values() {
        for error in r.scope.errors.iter() {
            let (loc, path, segment, why) = match error {
                ScopeError::Import { site, path, segment, why } => {
                    let fragment = site.expansion.unwrap_or(r.scope.module);
                    (c.tables.use_locs.get(&fragment).and_then(|locs| locs.get(site.index as usize).copied()), path, *segment as usize, *why)
                }
                ScopeError::Macro { call, path, segment, why } => (c.tables.calls.get(call).map(|src| src.loc), path, *segment as usize, *why),
                _ => continue,
            };
            let into_failed = waits.waits_on(r.scope.module, error).is_some();
            if !project || into_failed {
                elsewhere += 1;
                continue;
            }
            let mut message = blaze_own::Vec::new_in(*own.allocator());
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
            unresolved_args(&mut message, why, path.kind, &path.segments, segment);
            own.push((rule_of(why), PathFinding { at, message }));
        }
    }
    // The scopes are a map: its order reaches the record only sorted (rule 25).
    own.sort_unstable_by(|(ra, a), (rb, b)| (ra, &a.message[..], a.at).cmp(&(rb, &b.message[..], b.at)));
    elsewhere
}

/// Append the arguments of the reason's wording row, separated by the record's 0x1F.
fn unresolved_args(message: &mut blaze_own::Vec<u8, impl core::alloc::Allocator>, why: blaze_surface::Unresolved, kind: blaze_surface::PathKind, segments: &[&str], segment: usize) {
    use blaze_surface::{PathKind, Unresolved as Reason};
    let through = &segments[..segment.saturating_add(1).min(segments.len())];
    match why {
        Reason::NotFound => {
            message.push(b'`');
            message.extend_from_slice(segments.get(segment).copied().unwrap_or("").as_bytes());
            message.extend_from_slice(b"`\x1F");
            // The missing-name wording omits the hygiene root and spells any super prefix once.
            let scope_kind = match kind {
                PathKind::DollarCrate(_) => PathKind::Plain,
                PathKind::Super(_) => PathKind::Super(1),
                other => other,
            };
            let before = &segments[..segment.min(segments.len())];
            if scope_kind == PathKind::Plain && before.is_empty() {
                message.extend_from_slice(b"this scope");
            } else {
                message.push(b'`');
                path_arg(message, scope_kind, before);
                message.push(b'`');
            }
        }
        Reason::Private | Reason::NotAModule => path_arg(message, kind, through),
        Reason::Ambiguous(ambiguity) => {
            path_arg(message, kind, through);
            message.push(0x1F);
            message.extend_from_slice(ambiguity.name().as_bytes());
        }
        Reason::NoParent | Reason::NoCrate | Reason::WrongMacroKind | Reason::DeriveAfterAttrMacro => path_arg(message, kind, segments),
    }
}

fn path_arg(message: &mut blaze_own::Vec<u8, impl core::alloc::Allocator>, kind: blaze_surface::PathKind, segments: &[&str]) {
    use blaze_surface::PathKind;
    let (root, count) = match kind {
        PathKind::Plain => ("", 0),
        PathKind::Abs => {
            message.extend_from_slice(b"::");
            ("", 0)
        }
        PathKind::Crate => ("crate", 1),
        PathKind::SelfMod => ("self", 1),
        PathKind::Super(depth) => ("super", usize::from(depth)),
        PathKind::DollarCrate(_) => ("$crate", 1),
    };
    for (i, name) in core::iter::repeat_n(root, count).chain(segments.iter().copied()).enumerate() {
        if i > 0 {
            message.extend_from_slice(b"::");
        }
        message.extend_from_slice(name.as_bytes());
    }
}

/// Where a finding recognized by [`placed`] is to be drawn, read from its message alone (no file is
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

/// The names step's state for a command (engine.md item 2): the crate list it plans from, read
/// in place (blaze-world crate_list.rs version 6), each crate by subject, what the tasks made (a
/// task that asks for its files' `Surface`s answers from here when entered again), and the world
/// the list stands for, made only when a task needs it ([`Names::made_or_make`]). Every planner
/// answers from the list, so a request whose names, scripts and proc macros all stand makes no
/// world and decodes no graph; the list's rows are in the world's order, so a row is the made
/// world's crate index.
pub struct Names<'f> {
    /// The list, its bytes held to the command's end (the command's heap).
    list: CrateList<'static>,
    /// Its bytes: what the graph's record is sealed with (engine_units.rs `discover`).
    list_bytes: &'static [u8],
    /// (subject, crate index), sorted.
    by_subject: blaze_own::Vec<(ContentId, usize), Alloc<'static>>,
    made: std::vec::Vec<(ContentId, Resolved), Global>,
    /// The subjects of the crates whose records may hold nothing of a project (target.md,
    /// "Namespaces"): the toolchain's, and a registry archive's known by its lock checksum
    /// (the list's `PUBLIC` flag); then each run of such a crate placed on the wasm path.
    /// Whether a record is public also needs every name it reads public: the store's check.
    public: blaze_own::Vec<ContentId, Alloc<'static>>,
    /// (crate index, the `Script` name of its side's run), sorted, for each crate that reads
    /// one ([`blaze_world::Script::Read`]): its task Needs it (decision D115 (B), ruling (2)).
    scripts: blaze_own::Vec<(usize, Name), Alloc<'static>>,
    /// How each run of `scripts` is placed, in its order (D112's choice point): in the run's key.
    script_placed: blaze_own::Vec<crate::scripts::RanAs, Alloc<'static>>,
    /// Each crate's build-script module lookup, sorted by subject, then crate: one entry a crate
    /// with a build script, so two crates of one module are two entries.
    modules: blaze_own::Vec<ModuleAt, Alloc<'static>>,
    /// The world, once a task needed it ([`Names::made_or_make`]); made with the list when the
    /// graph was made this request ([`world`]).
    world: core::cell::OnceCell<NamesWorld<'f>>,
    /// What the world is made from by need: `None` for names made from a world.
    maker: Option<Maker<'f>>,
    /// The command's proc-macro runner, made by the first `CrateScope` task after the world
    /// (names/scope.rs `prepare`) and lent to every names and body job.
    procs: core::cell::Cell<Option<&'static crate::proc_runner::ProcRunner<'static>>>,
    /// Crates whose names were resolved this command, and the unresolved paths among them.
    pub resolved: u32,
    pub unresolved: u64,
}

/// A crate's build-script module lookup as the list holds it: its subject (script_modules.rs
/// `subject`), the crate whose package's script it is, and the run's placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModuleAt {
    pub(crate) subject: ContentId,
    pub(crate) crate_at: usize,
    pub(crate) ran_as: crate::scripts::RanAs,
}

/// What a list-read [`Names`] makes its world from when a task needs it: the graph's record by
/// its key and out (the one the list was read from), the lock and the checkout path packages'
/// directories.
pub(crate) struct Maker<'f> {
    record: blaze_drive::Key,
    out: ContentId,
    lock: &'f Lock<'static>,
    members: &'f [blaze_world::CheckoutPath<'f>],
}

impl<'f> Maker<'f> {
    /// The maker of the graph record at `record` (out `out`), `members` copied into `frame`.
    pub(crate) fn new(record: blaze_drive::Key, out: ContentId, lock: &'f Lock<'static>, members: &[crate::graph::CheckoutDir], frame: &'f Frame) -> Maker<'f> {
        let mut held = blaze_own::Vec::with_capacity_in(members.len(), frame);
        for entry in members.iter() {
            held.push(blaze_world::CheckoutPath { id: entry.id, dir: crate::work::held_str(entry.dir.as_str(), frame), kind: entry.kind });
        }
        Maker { record, out, lock, members: held.into_bump_slice() }
    }
}

/// The names world (blaze-world): every crate's input, made from the graph's record, the lock,
/// the toolchain's shelf and the fetched sources ([`world`], [`Names::made_or_make`]), with what
/// a task reads of it beyond the list: the decoded graph, where each package was placed, each
/// module lookup's package, version and features.
pub(crate) struct NamesWorld<'f> {
    world: World<'f>,
    /// Each crate's word for `public`, in world order (the crate list's `PUBLIC` flag).
    crate_public: blaze_own::Vec<bool, Alloc<'static>>,
    /// The command's heap (`FetchEnv::heap`): what the crate list's facts are worked out in
    /// ([`blaze_world::crate_list::ListFacts`]).
    heap: Alloc<'static>,
    /// As [`Names`]'s, from the world.
    scripts: std::vec::Vec<(usize, Name), Global>,
    /// (crate index, its build script's root file, package-relative), sorted: the file a
    /// failed script is named by (not_checked.rs).
    script_roots: std::vec::Vec<(usize, &'static str), Global>,
    /// Each crate's build-script module lookup (script_modules.rs), sorted by subject, then
    /// crate: one entry a crate reading a script, so two crates of one module are two entries.
    script_modules: std::vec::Vec<crate::script_modules::ScriptModule, Global>,
    /// The unit graph the world was made from (leaked with it): a proc-macro crate's unit, whose
    /// module identity the runner keys it by (proc_runner.rs).
    graph: &'static blaze_unit_graph::UnitGraph<'static>,
    /// Where the world placed each locked package (proc_callers.rs): populated on the eager
    /// discovery path (`make_world`), empty on the lazy standing-list path (`make_world_from_list`).
    placed: &'static [(LockId, blaze_world::Place<'static>)],
    /// On-demand pack opener when the world was made lazily from a standing `CrateList`
    /// (`make_world_from_list`); `None` when packs were opened eagerly into `world.texts.packs()`.
    lazy_trees: Option<&'static crate::body_types::SyncLazyTrees<'static>>,
    /// The names step's own frame ([`NAMES_FRAME`], held to the command's end): what the world
    /// read into it, for the profile's memory line ([`Names::held`]).
    frame: &'static Frame,
    /// What making the world did ([`Opened`]): the profile's `plan:` line.
    opened: &'static Opened,
}

impl<'f> NamesWorld<'f> {
    /// The world: its crates in index order (the list's rows), their texts (body_types.rs).
    pub(crate) fn world(&self) -> &World<'f> {
        &self.world
    }

    /// Whether `self.world.texts.packs()` holds all opened packs (true for eager discovery, false
    /// when packs are opened lazily via [`Self::lazy_trees`]).
    pub(crate) fn has_open_packs(&self) -> bool {
        self.lazy_trees.is_none()
    }

    /// On-demand pack opener when this world was built lazily from a standing `CrateList`.
    pub(crate) fn lazy_trees(&self) -> Option<&'static crate::body_types::SyncLazyTrees<'static>> {
        self.lazy_trees
    }

    /// Thread-safe packs view for names workers (`SyncLazy` when built lazily, `Open` when eager).
    pub(crate) fn packs<'a>(&'a self, fs: &'a bstd::fs::Fs) -> crate::job_texts::SyncPacks<'a> {
        match self.lazy_trees {
            Some(lazy) => crate::job_texts::SyncPacks::SyncLazy(lazy, fs),
            None => crate::job_texts::SyncPacks::Open(self.world.texts.packs()),
        }
    }

    /// The unit graph the world was made from.
    pub(crate) fn graph(&self) -> &'static blaze_unit_graph::UnitGraph<'static> {
        self.graph
    }

    /// What making the world did: its graph decodes, fetched records mapped and packages placed.
    pub(crate) fn opened(&self) -> &Opened {
        self.opened
    }

    /// Where the world placed each locked package (proc_callers.rs).
    pub(crate) fn placed(&self) -> &'static [(LockId, blaze_world::Place<'static>)] {
        self.placed
    }

    /// Each build-script run the world's crates read: (its `Script` subject, as the world named
    /// it, its build-script unit, the side of the crates that read it), sorted by subject, each
    /// once. The runs a `Script` part can name (scripts.rs `Scripts::open`), no unit of the graph
    /// walked beyond them; the list's runs table is written from it.
    pub(crate) fn script_runs<A: core::alloc::Allocator>(&self, alloc: A) -> blaze_own::Vec<(ContentId, blaze_unit_graph::UnitId, blaze_world::Side), A> {
        let mut runs = blaze_own::Vec::with_capacity_in(self.scripts.len(), alloc);
        for (at, name) in self.scripts.iter() {
            let krate = &self.world.crates[*at];
            let blaze_world::Origin::Unit(unit) = krate.origin else { continue };
            if let Some(script) = self.graph.units.get(unit.index()).and_then(|unit| unit.build_script) {
                runs.push((name.subject, script, krate.side));
            }
        }
        runs.sort_unstable_by_key(|(subject, script, _)| (*subject, *script));
        runs.dedup_by_key(|(subject, _, _)| *subject);
        runs
    }

    /// The build-script module lookup of `subject` whole (its package, version and features:
    /// what the lookup's job asks), if it is one.
    pub(crate) fn script_module(&self, subject: ContentId) -> Option<&crate::script_modules::ScriptModule> {
        let at = self.script_modules.partition_point(|module| module.subject < subject);
        self.script_modules.get(at).filter(|module| module.subject == subject)
    }

    fn script_module_of(&self, at: usize) -> Option<&crate::script_modules::ScriptModule> {
        self.script_modules.iter().find(|module| module.crate_at == at)
    }

    fn script_root(&self, at: usize) -> Option<&str> {
        self.script_roots.binary_search_by_key(&at, |(krate, _)| *krate).ok().map(|i| self.script_roots[i].1)
    }

    fn script_name_of(&self, at: usize) -> Option<[u8; 33]> {
        let (_, name) = self.scripts.iter().find(|(of, _)| *of == at)?;
        let mut bytes = [0; 33];
        bytes[0] = name.kind as u8;
        bytes[1..].copy_from_slice(&name.subject.0);
        Some(bytes)
    }
}

/// Pre-indexed read-only tables of a validated `CrateList<'static>`, cached in the resident
/// daemon's `HotGraph` so incremental edits skip `stored_graph_at`, `CrateList::open`, and
/// ~3,300 BLAKE3 digests in [`Names::of_list`].
pub(crate) struct HotList {
    pub(crate) list: CrateList<'static>,
    pub(crate) list_bytes: &'static [u8],
    pub(crate) by_subject: blaze_own::Vec<(ContentId, usize), Alloc<'static>>,
    pub(crate) public: blaze_own::Vec<ContentId, Alloc<'static>>,
    pub(crate) scripts: blaze_own::Vec<(usize, Name), Alloc<'static>>,
    pub(crate) script_placed: blaze_own::Vec<crate::scripts::RanAs, Alloc<'static>>,
    pub(crate) modules: blaze_own::Vec<ModuleAt, Alloc<'static>>,
    /// Sorted `Name::file(...)` of every member crate root `/0/<path>` listed in `list`.
    pub(crate) listed_roots: blaze_own::Vec<Name, Alloc<'static>>,
}

impl<'f> Names<'f> {
    /// The names read from the crate list `list` (its bytes held to the command's end), the
    /// world made by need from `maker`'s record ([`Names::made_or_make`]). No world is made and
    /// no graph decoded here: O(crates), the list's rows read in place (engine.md item 2).
    pub(crate) fn of_list(list_bytes: &'static [u8], maker: Option<Maker<'f>>, heap: Alloc<'static>) -> Result<Names<'f>, NamesError> {
        let list = CrateList::open(list_bytes).map_err(NamesError::List)?;
        let n = list.len();
        let mut by_subject = blaze_own::Vec::with_capacity_in(n, heap);
        by_subject.extend((0..n).map(|r| (blaze_drive::key::crate_scope_name(&list.key(r)).subject, r)));
        by_subject.sort_unstable();
        let mut scripts = blaze_own::Vec::new_in(heap);
        for r in 0..n {
            if let Some(name) = list.script(r) {
                if name[0] != Kind::Script as u8 {
                    return Err(NamesError::ListFact { row: r, what: "a script part of another kind than `Script`" });
                }
                let mut subject = [0u8; 32];
                subject.copy_from_slice(&name[1..]);
                scripts.push((r, Name::new(Kind::Script, ContentId(subject))));
            }
        }
        let mut modules = blaze_own::Vec::with_capacity_in(list.modules().count(), heap);
        for module in list.modules() {
            let ran_as = match module.ran_as {
                1 => crate::scripts::RanAs::Wasm,
                2 => crate::scripts::RanAs::Native,
                _ => return Err(NamesError::ListFact { row: module.row, what: "a module's placement byte that no `RanAs` has" }),
            };
            modules.push(ModuleAt { subject: ContentId(module.subject), crate_at: module.row, ran_as });
        }
        // The list holds them sorted by (subject, row) (crate_list.rs `open` checks it).
        let mut script_placed = blaze_own::Vec::with_capacity_in(scripts.len(), heap);
        for (at, _) in scripts.iter() {
            let module = modules.iter().find(|module| module.crate_at == *at).ok_or(NamesError::ListFact { row: *at, what: "a crate that reads a build script's run with no module lookup (ruling D115 (3))" })?;
            script_placed.push(module.ran_as);
        }
        // The public crates, in the list's order, then their runs placed on the wasm path (as
        // [`world`] tells them: ruling D115 sharing (i)). Both `CrateScope` and `ImplBucket`
        // subjects are added so closed public crates' impl tables are shareable and publishable.
        let mut public = blaze_own::Vec::new_in(heap);
        for r in (0..n).filter(|r| list.public(*r)) {
            let key = list.key(r);
            public.push(blaze_drive::key::crate_scope_name(&key).subject);
            public.push(blaze_ladder::front2::buckets::impl_tables_name(key).subject);
        }
        for ((at, name), ran_as) in scripts.iter().zip(script_placed.iter()) {
            if crate::scripts::run_is_public(list.public(*at), *ran_as) {
                public.push(name.subject);
            }
        }
        Ok(Names {
            list,
            list_bytes,
            by_subject,
            made: std::vec::Vec::new(),
            public,
            scripts,
            script_placed,
            modules,
            world: core::cell::OnceCell::new(),
            maker,
            procs: core::cell::Cell::new(None),
            resolved: 0,
            unresolved: 0,
        })
    }

    /// Captures the pre-indexed tables of `self` into a reusable [`HotList`] for the daemon.
    pub(crate) fn to_hot_list(&self, heap: Alloc<'static>) -> HotList {
        let mut listed_roots = blaze_own::Vec::new_in(heap);
        for r in 0..blaze_world::crate_list::CrateGraph::len(&self.list) {
            if let Some(file) = self.list.root(r).strip_prefix("/0/") {
                listed_roots.push(Name::file(file.as_bytes()));
            }
        }
        listed_roots.sort_unstable();
        listed_roots.dedup();
        HotList {
            list: self.list,
            list_bytes: self.list_bytes,
            by_subject: blaze_own::Vec::from_slice_in(self.by_subject.as_slice(), heap),
            public: blaze_own::Vec::from_slice_in(self.public.as_slice(), heap),
            scripts: blaze_own::Vec::from_slice_in(self.scripts.as_slice(), heap),
            script_placed: blaze_own::Vec::from_slice_in(self.script_placed.as_slice(), heap),
            modules: blaze_own::Vec::from_slice_in(self.modules.as_slice(), heap),
            listed_roots,
        }
    }

    /// Reconstructs [`Names`] from a cached [`HotList`] via flat slice copies (0 BLAKE3 digests,
    /// 0 `CrateList::open` re-validation).
    pub(crate) fn from_hot_list(hot: &HotList, heap: Alloc<'static>) -> Names<'f> {
        Names {
            list: hot.list,
            list_bytes: hot.list_bytes,
            by_subject: blaze_own::Vec::from_slice_in(hot.by_subject.as_slice(), heap),
            made: Default::default(),
            public: blaze_own::Vec::from_slice_in(hot.public.as_slice(), heap),
            scripts: blaze_own::Vec::from_slice_in(hot.scripts.as_slice(), heap),
            script_placed: blaze_own::Vec::from_slice_in(hot.script_placed.as_slice(), heap),
            modules: blaze_own::Vec::from_slice_in(hot.modules.as_slice(), heap),
            world: core::cell::OnceCell::new(),
            maker: None,
            procs: core::cell::Cell::new(None),
            resolved: 0,
            unresolved: 0,
        }
    }

    /// The crate list these names plan from.
    pub(crate) fn list(&self) -> &CrateList<'static> {
        &self.list
    }

    /// The list's bytes, as written or read back.
    pub(crate) fn list_bytes(&self) -> &'static [u8] {
        self.list_bytes
    }

    /// The world, when it was made (with the list, or by need).
    pub(crate) fn made(&self) -> Option<&NamesWorld<'f>> {
        self.world.get()
    }

    /// The world, made now if no task made it yet: the graph's record read again by the key the
    /// list was read from, the world made from it under the span naming `needed_by`, and every
    /// row of the list checked to be the made world's crate at that index (a world that differs
    /// is refused, never mixed with the list's answers). Made once a request: the cell is the
    /// request's, dropped with it; nothing is kept between requests (rule 70). `Err` for the
    /// store's own failure; `Ok(Err)` names why the world was not made.
    pub(crate) fn made_or_make(
        &self,
        cx: &Cx,
        root: &Dir<ReadWrite>,
        store: &mut dyn Store<'_>,
        env: &FetchEnv<'_>,
        needed_by: NeededBy,
    ) -> Result<Result<&NamesWorld<'f>, WorldRefused>, CliError> {
        if let Some(made) = self.world.get() {
            return Ok(Ok(made));
        }
        let refused = |why| Ok(Err(WorldRefused { needed_by, why }));
        let Some(maker) = self.maker.as_ref() else { return refused(WorldWhy::NoMaker) };
        let cx = cx.span(needed_by.span());
        let (view, sections) = match crate::engine_units::stored_graph_at(&cx, store, maker.record)? {
            Ok(found) => found,
            Err(why) => return refused(WorldWhy::Back(why)),
        };
        if view.out != maker.out {
            return refused(WorldWhy::Back(crate::engine_units::BackRefused::Out { key: maker.record }));
        }
        let made = match make_world(&cx, root, sections.graph, maker.lock, maker.members, env, Some(self.list)) {
            Ok(made) => made,
            Err(why) => return refused(WorldWhy::Make(why)),
        };
        let (crates, rows) = (made.world.crates.len(), self.list.len());
        if let Some(row) = (0..crates.max(rows)).find(|r| *r >= crates || *r >= rows || self.list.key(*r) != made.world.crates[*r].input.key) {
            return refused(WorldWhy::ListMoved { row, crates, rows });
        }
        // Every answer the planners took from the list, against the world's (rule 21: two values
        // under one key are refused, never mixed): a fact of the list whose input the graph's key
        // missed (a lock entry moved under one key, say) stops the request here, by name.
        let differences = {
            let _cx = cx.span("names: the crate list checked against the world made");
            self.differences_from(&made)
        };
        if differences != 0 {
            return refused(WorldWhy::ListDiffers { differences });
        }
        // The cell was empty above, and nothing between filled it (one request, one thread).
        Ok(Ok(self.world.get_or_init(|| made)))
    }

    /// The command's proc-macro runner, once made.
    pub(crate) fn procs(&self) -> Option<&'static crate::proc_runner::ProcRunner<'static>> {
        self.procs.get()
    }

    /// Sets the runner, once a command.
    pub(crate) fn set_procs(&self, procs: &'static crate::proc_runner::ProcRunner<'static>) {
        self.procs.set(Some(procs));
    }
}

impl Names<'_> {
    /// File one worker's result in subject order for both check drivers.
    pub(crate) fn keep_resolution(&mut self, name: Name, resolved: Resolved, ok: bool, unresolved: u32) -> Result<(), TaskFailure> {
        match self.made.binary_search_by_key(&name.subject, |row| row.0) {
            Ok(_) => Err(TaskFailure { name, why: Why::Refused { what: "a second resolution of the same crate", rule: RULE } }),
            Err(at) => {
                self.made.insert(at, (name.subject, resolved));
                self.resolved += u32::from(ok);
                self.unresolved += u64::from(unresolved);
                Ok(())
            }
        }
    }

    /// What this world keeps of the crates resolved this command, to the command's end, for the
    /// profile's memory lines (engine_memory.rs): how many crates, the bytes of their read lists
    /// and findings (a later step reads the lists), and the payload bytes
    /// not yet handed to a record (an answered crate hands its payload over, names/scope.rs
    /// `hand_payload`, so this is 0 once every crate resolved was answered). A walk made once,
    /// when the profile is written: profile-only, never in a record (rule 25).
    pub(crate) fn held(&self) -> (u64, u64, u64) {
        self.held_by_crates()
    }

    /// The names step's own frame ([`NAMES_FRAME`]): its bytes in use and committed now. It
    /// takes the world's reads (the shelf, the members' places, whatever is read through the
    /// world's checkout reader; the fetched records are mapped, [`Folder`], not read into it),
    /// held to the command's end. `(0, 0)` while no world was made (none was reserved).
    pub(crate) fn frame_held(&self) -> (u64, u64) {
        self.world.get().map_or((0, 0), |made| (made.frame.used() as u64, made.frame.committed() as u64))
    }

    fn held_by_crates(&self) -> (u64, u64, u64) {
        let mut lists = 0usize;
        let mut payloads = 0usize;
        for (_, resolved) in self.made.iter() {
            let paths: usize = resolved.files.iter().chain(resolved.absent.iter()).map(|path| path.capacity()).sum();
            let data: usize = resolved.data.iter().map(|(path, from)| path.capacity() + from.capacity()).sum();
            let slots = (resolved.files.capacity() + resolved.absent.capacity()) * core::mem::size_of::<String>() + resolved.data.capacity() * core::mem::size_of::<(String, String)>();
            let placed_bytes: usize = resolved.left_package.iter()
                .chain(resolved.failed_calls.iter())
                .map(|one| core::mem::size_of::<Unresolved>() + one.message.len())
                .sum();
            let path_bytes: usize = resolved.findings.iter().map(|entry| core::mem::size_of_val(entry) + entry.1.message.len()).sum();
            let mark_bytes: usize = resolved.test_marks.iter().map(|entry| core::mem::size_of_val(entry) + entry.1.message.len()).sum();
            let failed = resolved.failed.as_ref().map_or(0, |words| words.capacity());
            lists += paths + data + slots + placed_bytes + path_bytes + mark_bytes + failed;
            payloads += resolved.payload.len();
        }
        (self.made.len() as u64, lists as u64, payloads as u64)
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
        (CrateGraph::name(&self.list, at), CrateGraph::side(&self.list, at) == blaze_world::Side::Macro)
    }

    /// Each build-script run the world's crates read: (its `Script` subject, as the world named
    /// it, its build-script unit, the side of the crates that read it), sorted by subject, each
    /// once: the list's runs table ([`NamesWorld::script_runs`] wrote it). The runs a `Script`
    /// part can name (scripts.rs `Scripts::open`), no unit of the graph walked beyond them.
    pub(crate) fn script_runs<A: core::alloc::Allocator>(&self, alloc: A) -> blaze_own::Vec<(ContentId, blaze_unit_graph::UnitId, blaze_world::Side), A> {
        let mut runs = blaze_own::Vec::with_capacity_in(self.list.runs().count(), alloc);
        runs.extend(self.list.runs().map(|run| (ContentId(run.subject), blaze_unit_graph::UnitId(run.unit), run.side)));
        runs
    }

    /// The run of `subject` as the list holds it: its unit, side, package name and version, and
    /// the `Script` runs its `links` closure reads (engine_scripts.rs `plan`).
    pub(crate) fn script_run(&self, subject: ContentId) -> Option<blaze_world::crate_list::RunRow<'static>> {
        self.list.run(subject.0)
    }

    /// The world indices of the crates that read the `Script` run `script`, in index order.
    pub(crate) fn script_readers(&self, script: Name) -> impl Iterator<Item = usize> + '_ {
        self.scripts.iter().filter(move |(_, name)| *name == script).map(|(at, _)| *at)
    }

    /// The build-script file (package-relative) of the crate at world index `at`.
    pub(crate) fn script_root(&self, at: usize) -> Option<&str> {
        self.list.module_of(at).map(|module| module.root)
    }

    /// Each `Script` run's (subject, the reading crate's world key, its placement): its part
    /// source's inputs (scripts.rs `parts`).
    pub(crate) fn script_keys(&self) -> impl Iterator<Item = (ContentId, ContentId, crate::scripts::RanAs)> + '_ {
        self.scripts.iter().zip(self.script_placed.iter()).map(|((at, name), ran_as)| (name.subject, self.list.key(*at).0, *ran_as))
    }

    /// The `Script` part's name (kind byte, subject) of the crate at world index `at`, when its
    /// build script's run is a task: the crate list's script column (graph_payload.rs).
    pub(crate) fn script_name_of(&self, at: usize) -> Option<[u8; 33]> {
        self.list.script(at)
    }

    /// Every build-script module lookup's subject, sorted (repeats adjacent: one a crate).
    pub(crate) fn script_module_subjects(&self) -> impl Iterator<Item = ContentId> + '_ {
        self.modules.iter().map(|module| module.subject)
    }

    /// The module lookup the script read by the crate at world index `at` Needs.
    pub(crate) fn script_module_of(&self, at: usize) -> Option<ModuleAt> {
        self.modules.iter().find(|module| module.crate_at == at).copied()
    }

    /// The build-script module lookup of `subject`, if it is one (else a proc-macro crate's):
    /// its first crate. Its package, version and features are the world's
    /// ([`NamesWorld::script_module`]).
    pub(crate) fn script_module(&self, subject: ContentId) -> Option<ModuleAt> {
        let at = self.modules.partition_point(|module| module.subject < subject);
        self.modules.get(at).filter(|module| module.subject == subject).copied()
    }

    /// How many of the answers these names give from the list differ from the made world's,
    /// crate by crate (0 when they agree; `None` while no world was made): each crate's key,
    /// subject, label, script part, module (subject, placement, root) and public word; the runs
    /// (subject, unit, side, name, version and the `links` producers the graph's closure names);
    /// the proc-macro subjects. The planners' seam's oracle: run on every world made by need
    /// ([`Names::made_or_make`]) and, under test, on every world discovered ([`world`]); its
    /// controls in tests/engine_ready_edit_tests.rs.
    pub(crate) fn list_differences(&self) -> Option<usize> {
        self.world.get().map(|made| self.differences_from(made))
    }

    /// [`Names::list_differences`] against `made`, before it is kept.
    fn differences_from(&self, made: &NamesWorld<'_>) -> usize {
        let world = &made.world;
        let mut differ = if made.has_open_packs() {
            blaze_world::crate_list::differences(world, &self.list)
        } else {
            blaze_world::crate_list::differences_without_packs(world, &self.list)
        };
        for (at, krate) in world.crates.iter().enumerate() {
            if at >= self.list.len() {
                break;
            }
            let subject = blaze_drive::key::crate_scope_name(&krate.input.key).subject;
            differ += usize::from(self.crate_at(subject) != Some(at));
            differ += usize::from(self.crate_label(at) != (krate.input.name, krate.side == blaze_world::Side::Macro));
            differ += usize::from(self.script_name_of(at) != made.script_name_of(at));
            differ += usize::from(self.script_root(at) != made.script_root(at));
            let module = made.script_module_of(at).map(|module| (module.subject, module.ran_as));
            differ += usize::from(self.script_module_of(at).map(|module| (module.subject, module.ran_as)) != module);
            differ += usize::from(self.list.public(at) != made.crate_public[at]);
        }
        let runs = made.script_runs(made.heap);
        differ += usize::from(self.script_runs(made.heap).as_slice() != runs.as_slice());
        let mut each = |run: blaze_world::crate_list::Run<'_>| {
            let listed = self.script_run(ContentId(run.subject));
            let same = listed.is_some_and(|listed| {
                listed.unit == run.unit && listed.side == run.side && listed.name == run.name && listed.version == run.version && listed.producers().eq(run.producers.iter().copied())
            });
            differ += usize::from(!same);
        };
        blaze_world::crate_list::ListFacts::runs(made, &mut each);
        let mut macros: blaze_own::Vec<[u8; 32], Alloc<'static>> = blaze_own::Vec::new_in(made.heap);
        blaze_world::crate_list::ListFacts::macros(made, &mut |subject| macros.push(subject));
        differ += usize::from(!self.list.macros().eq(macros.iter().copied()));
        differ
    }
}

/// The crate list's planners' facts (blaze-world crate_list.rs version 6), answered from the
/// world and the graph it was made from, by the one maker of the list ([`world`]): what the
/// names, scripts and units stages then plan from with no world made and no graph decoded
/// (engine.md item 2). [`Names::list_differences`] holds the list's answers equal to these.
impl blaze_world::crate_list::ListFacts for NamesWorld<'_> {
    fn script(&self, at: usize) -> Option<[u8; 33]> {
        self.script_name_of(at)
    }

    fn public(&self, at: usize) -> bool {
        self.crate_public[at]
    }

    fn module(&self, at: usize) -> Option<blaze_world::crate_list::Module<'_>> {
        let module = self.script_module_of(at)?;
        let root = self.script_root(at)?;
        Some(blaze_world::crate_list::Module { ran_as: module.ran_as as u8, subject: module.subject.0, root })
    }

    fn runs(&self, each: &mut dyn FnMut(blaze_world::crate_list::Run<'_>)) {
        let runs = self.script_runs(self.heap);
        let mut producers: blaze_own::Vec<[u8; 32], Alloc<'static>> = blaze_own::Vec::new_in(self.heap);
        for (subject, script, side) in runs.iter() {
            producers.clear();
            // The plan's reads of the run (engine_scripts.rs `plan`): each `Script` run its
            // `links` closure reads, by the subject its side names.
            for (input, _, _) in blaze_proc_macro::script_unit::links_closure(self.graph, *script).iter() {
                if let Some(producer) = crate::scripts::name_of(self.graph, *input, *side, self.heap) {
                    producers.push(producer.subject.0);
                }
            }
            let unit = &self.graph.units[script.index()];
            each(blaze_world::crate_list::Run { subject: subject.0, unit: script.0, side: *side, name: unit.name.as_str(), version: unit.version.as_str(), producers: &producers });
        }
    }

    fn macros(&self, each: &mut dyn FnMut([u8; 32])) {
        let mut subjects: blaze_own::Vec<[u8; 32], Alloc<'static>> = blaze_own::Vec::new_in(self.heap);
        subjects.extend(crate::macros::macro_subjects(self.graph).map(|subject| subject.0));
        subjects.sort_unstable();
        subjects.dedup();
        for subject in subjects.iter() {
            each(*subject);
        }
    }
}

/// Each checkout path package's lock entry, directory (anchor-relative), and kind, from the
/// layout the command read: the graph job's when it was made, else the members matched to the
/// lock; `None` when the members were not matched to a lock this command.
pub(crate) fn member_dirs<'l>(laid: Option<&'l crate::units::Laid<'_>>) -> Option<&'l [crate::graph::CheckoutDir]> {
    let laid = laid?;
    match (&laid.graph, &laid.parts) {
        (Some(graph), _) => Some(graph.member_dirs()),
        (None, Some(parts)) => Some(parts.member_dirs()),
        (None, None) => None,
    }
}

/// The names world of a command, made from the graph record `graph_record`; `members` is each
/// checkout path package's lock entry, directory (anchor-relative), and kind ([`member_dirs`]).
/// The world is made with its crate list (blaze-world crate_list.rs version 8, written from it:
/// [`Names::list`]), which the graph's record is sealed with (engine_units.rs `discover`).
pub(crate) fn world<'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    graph_record: &[u8],
    lock: Option<&'f Lock<'static>>,
    members: Option<&[crate::graph::CheckoutDir]>,
    env: &FetchEnv<'_>,
) -> Result<Names<'f>, NamesError> {
    let lock = lock.ok_or(NamesError::NoLock)?;
    let members = members.ok_or(NamesError::NoMembers)?;
    let heap: Alloc<'static> = env.heap.alloc();
    let mut dirs: blaze_own::Vec<blaze_world::CheckoutPath<'_>, Alloc<'static>> = blaze_own::Vec::with_capacity_in(members.len(), heap);
    dirs.extend(members.iter().map(|entry| blaze_world::CheckoutPath { id: entry.id, dir: entry.dir.as_str(), kind: entry.kind }));
    let made = make_world(cx, root, graph_record, lock, &dirs, env, None)?;
    let list: &'static [u8] = {
        let _cx = cx.span("names: the crate list written");
        blaze_world::crate_list::encode_with(&made.world, &made, heap).map_err(NamesError::List)?.leak_static()
    };
    let names = Names::of_list(list, None, heap)?;
    // THE ORACLE, under test (names.rs `differences_from`): every answer the planners take from
    // the list equals the world's, crate by crate, on every test world discovered. In a build it
    // is the by-need make that checks it ([`Names::made_or_make`]): the list just written from
    // this world is not checked against it again (rule 77).
    #[cfg(test)]
    {
        let differences = names.differences_from(&made);
        if differences != 0 {
            return Err(NamesError::ListDiffers { differences });
        }
    }
    names.world.get_or_init(|| made);
    Ok(names)
}

/// Places only checkout path packages from `paths`, without touching any fetched source record.
struct CheckoutOnly<'l, 'w> {
    paths: &'w [blaze_world::CheckoutPath<'w>],
    lock: &'l Lock<'w>,
    alloc: blaze_own::Bump<'w>,
}

impl<'w> blaze_world::Packages<'w> for CheckoutOnly<'_, 'w> {
    fn place(&mut self, package: LockId) -> Result<blaze_world::Place<'w>, WorldError<'w>> {
        match self.paths.iter().find(|entry| entry.id == package) {
            Some(blaze_world::CheckoutPath { dir, kind: blaze_world::PathKind::Member, .. }) => Ok(blaze_world::Place::Checkout { dir }),
            Some(blaze_world::CheckoutPath { dir, kind: blaze_world::PathKind::External, .. }) => Ok(blaze_world::Place::Path { dir }),
            None => Err(WorldError::PathNotPlaced { package: self.alloc.alloc_str(&self.lock.package(package).name) }),
        }
    }
}

/// The names world made from the graph record `graph_record`, the lock and the checkout path
/// packages' directories, anchor-relative ([`world`], [`Names::made_or_make`]). When `stored_list`
/// is `Some`, builds lazily via `open_list_light` + `world_from_list` + `SyncLazyTrees` without
/// opening any toolchain or fetched package pack up front.
fn make_world<'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    graph_record: &[u8],
    lock_whole: &'f Lock<'static>,
    members: &[blaze_world::CheckoutPath<'_>],
    env: &FetchEnv<'_>,
    stored_list: Option<CrateList<'static>>,
) -> Result<NamesWorld<'f>, NamesError> {
    let lock: &'f Lock<'f> = lock_whole;
    let heap: Alloc<'static> = env.heap.alloc();
    let Some(home) = env.home else { return Err(NamesError::NoHome) };
    let opened: &'static Opened = blaze_own::Box::leak_static(blaze_own::Box::new_in(Opened::default(), heap));
    let decoded = {
        let _cx = cx.span("names: the unit graph decoded");
        opened.graph_decodes.set(opened.graph_decodes.get().saturating_add(1));
        blaze_unit_graph::record::decode(graph_record, heap).map_err(NamesError::Graph)?
    };
    let decoded: &'static blaze_unit_graph::record::Decoded<'static> = std::boxed::Box::leak(std::boxed::Box::new(decoded));
    let graph = &decoded.graph;
    let cx: &'static Cx = std::boxed::Box::leak(std::boxed::Box::new(cx.until(cx.deadline())));
    let frame: &'static Frame = std::boxed::Box::leak(std::boxed::Box::new(Frame::with_reserve(NAMES_FRAME, bstd::mem::Tag::Other).map_err(|_| NamesError::Frame)?));
    let mut shelf = {
        let _cx = cx.span("names: the toolchain's shelf opened");
        crate::toolchain::HomeShelf::open(cx, &env.fs, home, frame).map_err(NamesError::Shelf)?
    };
    let reader: &'static Reader = std::boxed::Box::leak(std::boxed::Box::new(Reader { cx, root: root.read_only(), frame }));
    let mut by_dir: blaze_own::Vec<(&str, &str), &Frame> = blaze_own::Vec::with_capacity_in(members.len(), frame);
    for entry in members.iter() {
        by_dir.push((entry.dir, lock.package(entry.id).name.as_str()));
    }
    by_dir.sort_unstable();
    if let Some(pair) = by_dir.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(NamesError::SharedDirectory { first: String::from(pair[0].1), second: String::from(pair[1].1), dir: String::from(pair[0].0) });
    }
    let mut paths: blaze_own::Vec<blaze_world::CheckoutPath<'static>, &Frame> = blaze_own::Vec::new_in(frame);
    for at in 0..lock.packages.len() {
        let id = LockId(at as u32);
        let package = lock.package(id);
        if !matches!(package.source, LockSource::Path) || !graph.units.iter().any(|unit| unit.package == id) {
            continue;
        }
        let Some(entry) = members.iter().find(|entry| entry.id == id) else {
            let dependent = graph
                .units
                .iter()
                .find(|unit| unit.deps.iter().any(|dep| graph.units[dep.unit.0 as usize].package == id))
                .map(|unit| String::from(unit.name.as_str()));
            return Err(NamesError::PathNotMember { package: String::from(package.name.as_str()), dependent });
        };
        let held: &'static str = frame.alloc_str(entry.dir);
        paths.push(blaze_world::CheckoutPath { id, dir: held, kind: entry.kind });
    }
    let paths: &'f [blaze_world::CheckoutPath<'static>] = paths.into_bump_slice();
    let checkout: &'f ReadTree = std::boxed::Box::leak(std::boxed::Box::new(ReadTree(reader)));
    let bump: blaze_own::Bump<'static> = frame.alloc(blaze_own::Erased::new(frame)).bump();
    let world_err = |e: WorldError<'_>| -> NamesError {
        let words = bstd::mem::format_in!(frame, "{e}");
        match e {
            WorldError::Toolchain(_) => NamesError::Toolchain(String::from(words.as_str())),
            _ => NamesError::World(String::from(words.as_str())),
        }
    };
    let (world, placed, lazy_trees) = if let Some(stored) = stored_list {
        let mut packages = CheckoutOnly { paths, lock: lock_whole, alloc: bump };
        let listed = {
            let _cx = cx.span("names: the crate list and toolchain manifest opened (lazy)");
            blaze_world::open_list_light(&mut shelf, env.toolchain, crate::graph::TRIPLE, graph, lock, &mut packages, bump).map_err(&world_err)?
        };
        let listed: &'f Listed<'f, crate::toolchain::Held<'static>> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(listed, frame));
        let world = {
            let _cx = cx.span("names: the world built from crate list (lazy)");
            blaze_world::world_from_list(graph, lock, listed, &stored, &mut packages, checkout, bump).map_err(&world_err)?
        };
        let lazy_trees = crate::body_types::SyncLazyTrees::new_in_heap(Some(home), env.heap, stored).map_err(|why| NamesError::World(String::from(why)))?;
        (world, &[][..], Some(lazy_trees))
    } else {
        let sources = {
            let _cx = cx.span("names: the lock's fetch plan and the sources store opened");
            StoreSources::new(lock_whole, Folder::of(reader, env, opened)?, PackView::open_source_put, heap).map_err(|e| NamesError::Sources(std::format!("{e}")))?
        };
        let sources = std::boxed::Box::leak(std::boxed::Box::new(sources));
        let mut fetched = Fetched { store: sources, paths, lock: lock_whole, alloc: bump };
        let mut packages = crate::proc_callers::Placing::new(&mut fetched, heap);
        let listed = {
            let _cx = cx.span("names: the crate list and the toolchain's packs opened");
            blaze_world::open_list(&mut shelf, env.toolchain, crate::graph::TRIPLE, graph, lock, &mut packages, bump).map_err(&world_err)?
        };
        let listed: &'f Listed<'f, crate::toolchain::Held<'static>> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(listed, frame));
        let world = {
            let _cx = cx.span("names: the packages placed and the world made");
            blaze_world::world(graph, lock, listed, &mut packages, checkout, bump).map_err(&world_err)?
        };
        (world, packages.finish().leak_slice(), None)
    };
    let _cx = cx.span("names: the world's public crates and scripts");
    let from_archive = |unit: blaze_unit_graph::UnitId| {
        let package = lock.package(graph.units[unit.0 as usize].package);
        matches!(package.source, LockSource::Registry { .. }) && matches!(package.checksum, blaze_manifest::lock::Checksum::Sha256(_))
    };
    let mut crate_public: blaze_own::Vec<bool, Alloc<'static>> = blaze_own::Vec::with_capacity_in(world.crates.len(), heap);
    crate_public.extend(world.crates.iter().map(|krate| match krate.origin {
        blaze_world::Origin::Toolchain(_) => true,
        blaze_world::Origin::Unit(unit) => from_archive(unit),
    }));
    let mut scripts = std::vec::Vec::new_in(Global);
    for (at, krate) in world.crates.iter().enumerate() {
        if let blaze_world::Script::Read { subject } = krate.script {
            scripts.push((at, Name::new(Kind::Script, subject)));
        }
    }
    let mut script_roots = std::vec::Vec::new_in(Global);
    let mut script_modules = std::vec::Vec::new_in(Global);
    let mut placements = std::vec::Vec::new_in(Global);
    for (at, script) in script_units(world.crates.iter().map(|krate| krate.origin), |unit| graph.units[unit.index()].build_script) {
        let ran_as = crate::scripts::placement(Some(home), graph, script, heap);
        placements.push((at, ran_as));
        let script = &graph.units[script.index()];
        script_roots.push((at, script.root.as_str()));
        let mut features = std::vec::Vec::new_in(Global);
        features.extend(script.features.iter().map(|feature| feature.as_str()));
        let subject = crate::script_modules::subject(ran_as, script.name.as_str(), script.version.as_str(), &features);
        script_modules.push(crate::script_modules::ScriptModule { subject, crate_at: at, ran_as, package: script.name.as_str(), version: script.version.as_str(), features });
    }
    script_modules.sort_unstable_by_key(|module| (module.subject, module.crate_at));
    for (at, _) in scripts.iter() {
        if placements.binary_search_by_key(at, |(krate, _)| *krate).is_err() {
            return Err(NamesError::World(std::format!(
                "the crate `{}` reads a build script's run, and no unit of it has a build script (ruling D115 (3))",
                world.crates[*at].input.name
            )));
        }
    }
    Ok(NamesWorld { world, crate_public, heap, scripts, script_roots, script_modules, graph, placed, lazy_trees, frame, opened })
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
    // The toolchain's of them: their records (`CrateScope` and `ImplBucket`) are kept under the
    // toolchain's root (the list's `TOOLCHAIN` flag).
    let list = &names.list;
    let mut toolchain = std::vec::Vec::with_capacity_in(list.len().saturating_mul(2), Global);
    for r in (0..list.len()).filter(|r| CrateGraph::toolchain(list, *r)) {
        let key = list.key(r);
        toolchain.push(blaze_drive::key::crate_scope_name(&key).subject);
        toolchain.push(blaze_ladder::front2::buckets::impl_tables_name(key).subject);
    }
    store.toolchain_crates(&toolchain);
}

/// The `CrateScope` parts the graph's record names: one per crate of the world, by subject,
/// its source the same key's digest. Owed (drive.md, RUNNER 16): every part runs, so a
/// `#![no_core]` workspace still resolves all 15 toolchain crates cold; a `Need` locates its
/// name among its home's parts, so a toolchain crate cannot simply be left out here: the
/// drive needs a part that is located but run only when wanted.
pub(crate) fn parts(names: &Names<'_>) -> std::vec::Vec<(Name, ContentId), Global> {
    let list = &names.list;
    let mut parts = std::vec::Vec::with_capacity(list.len());
    for r in 0..list.len() {
        let key = list.key(r);
        parts.push((blaze_drive::key::crate_scope_name(&key), ContentId::of(SOURCE_DOMAIN, &key.0.0)));
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

#[cfg(test)]
mod script_units_tests;

#[cfg(test)]
mod unresolved_tests {
    use blaze_report::frame::words::{Area, say, wording};
    use blaze_surface::{AmbiguityKind, PathKind, Unresolved};

    fn expect(why: Unresolved, kind: PathKind, segments: &[&str], segment: usize, expected_code: &str, expected: &str) {
        let alloc = blaze_own::testing::heap().alloc();
        let mut message = blaze_own::Vec::new_in(alloc);
        super::unresolved_args(&mut message, why, kind, segments, segment);
        let args: std::vec::Vec<_> = core::str::from_utf8(&message).expect("diagnostic text").split('\x1f').collect();
        let mut headline = blaze_own::AString::new_in(alloc);
        let mut label = blaze_own::AString::new_in(alloc);
        let code = say(Area::Cli, super::rule_of(why), &args, &mut headline, &mut label).expect("unresolved wording");
        assert_eq!(code, expected_code);
        assert_eq!(headline.as_str(), expected);
    }

    #[test]
    fn missing_binding() {
        expect(Unresolved::NotFound, PathKind::Plain, &["gone"], 0, "unresolved", "no `gone` in this scope");
        expect(Unresolved::NotFound, PathKind::Crate, &["m", "gone"], 1, "unresolved", "no `gone` in `crate::m`");
    }

    #[test]
    fn private_binding() {
        expect(Unresolved::Private, PathKind::Crate, &["hidden", "f"], 0, "unresolved-private", "`crate::hidden` is private in this scope");
    }

    #[test]
    fn non_module_segment() {
        expect(Unresolved::NotAModule, PathKind::Plain, &["m", "f", "item"], 1, "unresolved-not-a-module", "`m::f` is neither a module nor an enum");
    }

    #[test]
    fn super_above_root() {
        expect(Unresolved::NoParent, PathKind::Super(2), &["f"], 0, "unresolved-super-above-root", "`super` goes above the crate root in `super::super::f`");
    }

    #[test]
    fn dollar_crate_without_dependency() {
        let kind = PathKind::DollarCrate(blaze_id::CrateItemKey(super::ContentId([1; 32])));
        expect(Unresolved::NoCrate, kind, &["f"], 0, "unresolved-dollar-crate", "the crate named by `$crate` in `$crate::f` is not a dependency");
    }

    #[test]
    fn ambiguous_binding() {
        expect(Unresolved::Ambiguous(AmbiguityKind::GlobVsGlob), PathKind::SelfMod, &["f"], 0, "unresolved-ambiguous", "`self::f` is ambiguous (glob vs glob)");
    }

    #[test]
    fn wrong_macro_kind() {
        expect(Unresolved::WrongMacroKind, PathKind::Abs, &["macros", "make"], 1, "unresolved-wrong-macro-kind", "`::macros::make` names a different kind of macro than this call requires");
    }

    #[test]
    fn derive_after_attribute_macro() {
        expect(
            Unresolved::DeriveAfterAttrMacro,
            PathKind::Plain,
            &["reth_codecs", "add_arbitrary_tests"],
            1,
            "unresolved-derive-after-attribute",
            "attribute macro `reth_codecs::add_arbitrary_tests` was refused because a `#[derive]` is written after it on the same item",
        );
    }

    #[test]
    fn each_reason_has_a_distinct_rule_with_its_argument_count() {
        // A new variant breaks the exhaustive match in rule_of. Add its case here with its rule.
        const REASONS: [Unresolved; 8] = [
            Unresolved::NotFound,
            Unresolved::Private,
            Unresolved::Ambiguous(AmbiguityKind::GlobVsGlob),
            Unresolved::NotAModule,
            Unresolved::NoParent,
            Unresolved::NoCrate,
            Unresolved::WrongMacroKind,
            Unresolved::DeriveAfterAttrMacro,
        ];
        let rules = REASONS.map(super::rule_of);
        let mut message = blaze_own::Vec::new_in(blaze_own::testing::heap().alloc());
        for (i, why) in REASONS.into_iter().enumerate() {
            let rule = rules[i];
            assert!(!rules[..i].contains(&rule), "{why:?} repeats rule {rule}");
            assert!(super::placed(rule), "{why:?} must be placed at its item");
            let row = wording(Area::Cli, rule).expect("each unresolved reason has a wording row");
            message.clear();
            super::unresolved_args(&mut message, why, PathKind::Plain, &["f"], 0);
            assert_eq!(message.split(|b| *b == 0x1F).count(), usize::from(row.args), "{why:?}");
        }
    }
}

pub(crate) use scope::{DepExports, FindingText, Resolution, SCOPE_FRAME, SCOPE_JOBS, ScopeJob, hand_payload, resolve, sysroot_given};
