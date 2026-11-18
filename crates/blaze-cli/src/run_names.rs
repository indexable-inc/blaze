//! The names records of a program's crates, made ONE way for `blaze check` and `blaze run`
//! (rule 30): front2's `Records` over front_load's `StoreLoad`, its `Log`, and the texts a body's
//! macro calls expand with ([`names_records`]). body_types.rs `checker` makes them for a file's
//! owner; `blaze run` for the program's crate ([`open`]), which the lazy parent opens on its first
//! need of them (run_parent.rs `CommandFront`, blaze-ladder make_one.rs `NamesRecords`).
//!
//! WHICH CRATES `blaze run` READS: the program's crate (the member crate rooted at the picked
//! program's root, run_package.rs `Program::root`, in the crate list the `UnitGraph` record
//! holds) and its check scope (blaze-world check_scope.rs, the one rule of which crates a body
//! may see), each by its `CrateScope` record, the owner at 0: exactly what `blaze check` reads
//! for that root.
//!
//! WHERE THE RECORDS ARE FOUND. A run that WALKED (run_ids.rs `walked`) settled every one of
//! those names this request: each is `Request::settled_input`'s row ([`Walked`]). A run that
//! STOOD on its certificate holds no row: the request read only the certificate, and the
//! answer's root and pages are read by blaze-drive alone (`read_cert` and `read_root` are not
//! public). So each walk writes the rows the names records read as ONE record at its certificate
//! ([`put`], [`inputs_key`]: the certificate holds the whole tool, so a new blaze is a new key),
//! and a run that stands reads it in place ([`Stood`]): one get when the program starts, the
//! rest only when the parent first needs a name. A hit whose record the store does not hold
//! (dropped) is answered in full: the command drops the request before the program starts and
//! asks again cold (main.rs `AnswerInFull`), once; never a guess.
//!
//! FORM (little-endian): [`MAGIC`] (16 bytes), the `UnitGraph` row's `at` and `out` (32 + 32),
//! the count `n` (u32), then `n` rows of (the crate's index in the list u32, its `CrateScope`
//! row's `at` 32, `out` 32), strictly increasing by index. Checked whole when opened.

use core::fmt;

use blaze_drive::{Input, Key, Name};
use blaze_fetch::PackView;
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::front2::body_host::Sourced;
use blaze_ladder::front2::source::{Log, Records};
use blaze_ladder::run::make_one::NamesRecords;
use blaze_own::{AVec, Alloc, Vec};
use blaze_scope::items_record::{ItemsRecord, ItemsRecordError};
use blaze_world::crate_list::{CrateGraph, CrateList};
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::Frame;
use core::cell::RefCell;
use std::alloc::Global;

use crate::CliError;
use crate::body_types::Kept;
use crate::front_load::{CrateRecords, StoreLoad};
use crate::job_texts::JobTexts;
use crate::proc_runner::ProcRunner;
use crate::store::Store;

#[cfg(test)]
#[path = "run_names_tests.rs"]
mod tests;

/// One crate the names records read: its records, its name, where the world placed its package
/// (`CrateInput::shown_from`) and its index in the names world (the proc-macro runner's key).
#[derive(Clone, Copy)]
pub(crate) struct Crate<'n> {
    pub(crate) records: CrateRecords<'n>,
    pub(crate) name: &'n str,
    pub(crate) package: &'n str,
    pub(crate) world_at: usize,
}

/// The names records over some crates, and what was made beside them.
pub(crate) struct Opened<'n> {
    pub(crate) records: &'n Records<'n>,
    /// The loader under them: what a check asks which expansion texts it did not serve.
    pub(crate) load: &'n StoreLoad<'n>,
    /// The texts a body's macro calls expand with (front2 body_host.rs).
    pub(crate) sources: &'n Sourced<'n, JobTexts<'n>>,
}

/// Why the names records over some crates could not be made: the crate by its place among
/// them (the caller names it: its words are the caller's crates').
#[derive(Debug)]
pub(crate) enum NamesError {
    /// A crate's items record could not be opened for its edition.
    Items { at: usize, why: ItemsRecordError },
}


/// THE ONE WAY names records are made (rule 30): over `crates` in reader order (the owner at 0),
/// each crate a proc-macro caller by its place and its items record's edition, the texts served
/// from the checkout at `root`, the list's `packs` and `corrected` files, with `read` the checkout
/// files already read (a body job's own file), all kept in `frame` for the caller's lifetime.
/// `env` is the environment of the crate at 0 (a body job's owner, body_types.rs `owner_env`):
/// what a body's `env!` reads; every other crate's `env!` is refused by name (body_host.rs).
/// Nothing is read of a crate here: `Records::new` opens each on the first question about it.
/// COST: one items-record open per crate, for its edition.
#[expect(clippy::too_many_arguments, reason = "the names records' edge: each argument one input of them")]
pub(crate) fn names_records<'n>(
    cx: &'n Cx,
    root: &'n Dir<ReadWrite>,
    crates: &[Crate<'n>],
    packs: &'n [PackView<'n>],
    corrected: &'n [(u32, &'n str, &'n str)],
    procs: Option<&'n ProcRunner<'static>>,
    read: std::vec::Vec<(&'n str, Option<&'n str>), Global>,
    env: Option<&'n blaze_scope::crate_env::CrateEnv<'n>>,
    frame: &'n Frame,
    g: Alloc<'static>,
) -> Result<Opened<'n>, NamesError> {
    let mut records: Vec<CrateRecords<'n>, &'n Frame> = Vec::with_capacity_in(crates.len(), frame);
    let mut crate_names: Vec<&'n str, &'n Frame> = Vec::with_capacity_in(crates.len(), frame);
    let mut packages: Vec<&'n str, &'n Frame> = Vec::with_capacity_in(crates.len(), frame);
    // Each crate as a proc-macro caller, by its place, which is its `krate` in the records made
    // below from `records` in this order: its index in the names world and its edition.
    let mut callers: Vec<(usize, usize, blaze_expand::Edition), &'n Frame> = Vec::with_capacity_in(crates.len(), frame);
    for (at, one) in crates.iter().enumerate() {
        records.push(one.records);
        crate_names.push(one.name);
        packages.push(one.package);
        let edition = ItemsRecord::open(one.records.items, g).map_err(|why| NamesError::Items { at, why })?.edition();
        callers.push((at, one.world_at, edition));
    }
    let (records, crate_names, packages): (&'n [CrateRecords<'n>], &'n [&'n str], &'n [&'n str]) = (records.leak(), crate_names.leak(), packages.leak());
    let callers: &'n [(usize, usize, blaze_expand::Edition)] = callers.leak();
    let texts: &'n JobTexts<'n> = blaze_own::Box::leak(blaze_own::Box::new_in(
        JobTexts {
            cx,
            root,
            frame,
            packs,
            corrected,
            // The first crate's environment (a body job's owner): what its bodies' `env!` reads.
            env: env.map(|env| (0, env)),
            read: RefCell::new(read),
            data: RefCell::new(std::vec::Vec::new()),
            left: RefCell::new(std::vec::Vec::new()),
            procs,
            own: None,
            callers,
            run_stopped: core::cell::Cell::new(None),
        },
        frame,
    ));
    let load: &'n StoreLoad<'n> = blaze_own::Box::leak(blaze_own::Box::new_in(StoreLoad::new(records, packages, texts, g), frame));
    let log: &'n Log<'n> = blaze_own::Box::leak(blaze_own::Box::new_in(Log::new(crate_names, g), frame));
    let keys = load.keys(g);
    let records: &'n Records<'n> = blaze_own::Box::leak(blaze_own::Box::new_in(Records::new(keys, load, log, frame, g), frame));
    let sources: &'n Sourced<'n, JobTexts<'n>> = blaze_own::Box::leak(blaze_own::Box::new_in(Sourced(texts), frame));
    Ok(Opened { records, load, sources })
}

/// The record's first 16 bytes.
pub(crate) const MAGIC: [u8; 16] = *b"blaze run names1";

/// The head: the magic, the `UnitGraph` row, the count.
const HEAD: usize = 16 + 32 + 32 + 4;

/// One crate's row: its index, its row's `at` and `out`.
const ROW: usize = 4 + 32 + 32;

/// The most crates a record names (rule 18): far above any program's check scope.
pub(crate) const MOST_ROWS: usize = 1 << 20;

const KEY_DOMAIN: &str = "blaze-cli run names inputs 1";

/// The key of the record of certificate `cert`: the certificate holds the tool and the request.
pub(crate) fn inputs_key(cert: Key) -> Key {
    let mut h = Hasher::new(KEY_DOMAIN);
    h.id(&cert.0);
    Key(h.finish())
}

/// Where a run finds the rows of the names it reads: the `UnitGraph`'s, and each crate's
/// `CrateScope` row by its index in the list and its name.
pub(crate) trait Answered {
    fn graph(&self) -> Option<Input>;
    fn crate_input(&self, at: usize, name: Name) -> Option<Input>;
}

/// The rows a request that walked settled.
pub(crate) struct Walked<'q, 'r>(pub(crate) &'q blaze_drive::Request<'r>);

impl Answered for Walked<'_, '_> {
    fn graph(&self) -> Option<Input> {
        self.0.settled_input(Name::unit_graph())
    }

    fn crate_input(&self, _at: usize, name: Name) -> Option<Input> {
        self.0.settled_input(name)
    }
}

/// Why a record's bytes are not one of this form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FormError {
    Short { len: usize },
    Magic,
    Rows { count: usize },
    Length { len: usize, want: usize },
    Unsorted { row: usize },
}

impl fmt::Display for FormError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormError::Short { len } => write!(f, "{len} bytes, shorter than the head's {HEAD}"),
            FormError::Magic => write!(f, "its first 16 bytes are not `blaze run names1`"),
            FormError::Rows { count } => write!(f, "it names {count} crates, more than the {MOST_ROWS} a record may"),
            FormError::Length { len, want } => write!(f, "{len} bytes where its count makes {want}"),
            FormError::Unsorted { row } => write!(f, "row {row}'s crate index is not above the row before it"),
        }
    }
}

/// The rows a request that stood reads: the record of its certificate, checked whole, read in
/// place (each crate a binary search over its rows).
#[derive(Clone, Copy)]
pub(crate) struct Stood<'b> {
    graph: Input,
    rows: &'b [u8],
}

fn word(b: &[u8], at: usize) -> [u8; 32] {
    let mut w = [0u8; 32];
    w.copy_from_slice(&b[at..at + 32]);
    w
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl<'b> Stood<'b> {
    /// `bytes` checked whole: the magic, the count against [`MOST_ROWS`] and the length, each
    /// row's index above the one before. O(rows).
    pub(crate) fn open(bytes: &'b [u8]) -> Result<Stood<'b>, FormError> {
        if bytes.len() < HEAD {
            return Err(FormError::Short { len: bytes.len() });
        }
        if bytes[..16] != MAGIC {
            return Err(FormError::Magic);
        }
        let count = u32_at(bytes, 80) as usize;
        if count > MOST_ROWS {
            return Err(FormError::Rows { count });
        }
        let want = HEAD + count * ROW;
        if bytes.len() != want {
            return Err(FormError::Length { len: bytes.len(), want });
        }
        let rows = &bytes[HEAD..];
        for row in 1..count {
            if u32_at(rows, row * ROW) <= u32_at(rows, (row - 1) * ROW) {
                return Err(FormError::Unsorted { row });
            }
        }
        let graph = Input { name: Name::unit_graph(), at: Key(ContentId(word(bytes, 16))), out: ContentId(word(bytes, 48)) };
        Ok(Stood { graph, rows })
    }
}

impl Answered for Stood<'_> {
    fn graph(&self) -> Option<Input> {
        Some(self.graph)
    }

    fn crate_input(&self, at: usize, name: Name) -> Option<Input> {
        let want = u32::try_from(at).ok()?;
        let (mut low, mut high) = (0, self.rows.len() / ROW);
        while low < high {
            let mid = low + (high - low) / 2;
            let here = u32_at(self.rows, mid * ROW);
            if here == want {
                let row = mid * ROW;
                return Some(Input { name, at: Key(ContentId(word(self.rows, row + 4))), out: ContentId(word(self.rows, row + 36)) });
            }
            if here < want {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        None
    }
}

/// The record of `graph` and `rows` (each crate's index and row; any order, one each) in `out`.
pub(crate) fn encode(graph: Input, rows: &[(usize, Input)], out: &mut AVec<'_, u8>, scratch: Alloc<'_>) -> Result<(), RunNamesError> {
    if rows.len() > MOST_ROWS {
        return Err(RunNamesError::Form(FormError::Rows { count: rows.len() }));
    }
    let mut sorted: AVec<'_, (u32, Input)> = AVec::with_capacity_in(rows.len(), scratch);
    for (at, input) in rows.iter() {
        let at = u32::try_from(*at).map_err(|_| RunNamesError::Form(FormError::Rows { count: *at }))?;
        sorted.push((at, *input));
    }
    sorted.sort_unstable_by_key(|(at, _)| *at);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&graph.at.0.0);
    out.extend_from_slice(&graph.out.0);
    out.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
    for (row, (at, input)) in sorted.iter().enumerate() {
        if row > 0 && sorted[row - 1].0 == *at {
            return Err(RunNamesError::Form(FormError::Unsorted { row }));
        }
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&input.at.0.0);
        out.extend_from_slice(&input.out.0);
    }
    Ok(())
}

/// The program's crates as the answer has them: the crate list, the program's crate, and each
/// crate of its check scope in reader order (the program's at 0) with its `CrateScope` row; the
/// lang crates by their place among those.
pub(crate) struct Program {
    graph: Input,
    list: CrateList<'static>,
    crates: AVec<'static, (usize, Input)>,
    lang: AVec<'static, usize>,
}

/// Why the names records of `blaze run` could not be had.
#[derive(Debug)]
pub(crate) enum RunNamesError {
    /// The answer holds no `UnitGraph` row: no lock matched, so no crate list.
    NoGraph,
    /// A store read or put, or the crate list's read (work.rs `payload`); boxed, as `CliError`
    /// holds this type.
    Store(std::boxed::Box<CliError, Global>),
    /// The `UnitGraph` record holds no crate list: its world could not be made (its finding says why).
    WorldNotMade,
    /// No member crate of the list has its root at `root`, the picked program's crate root
    /// (run_package.rs `Program::root`).
    NoProgramCrate { root: std::boxed::Box<str> },
    /// The request started a program, but the command picked none: only `blaze run` picks one
    /// (main.rs `request`, run_package.rs `pick`).
    NonePicked,
    /// Two crates of the list have their root at `root`.
    TwoProgramCrates { root: std::boxed::Box<str>, first: &'static str, second: &'static str },
    /// The answer holds no `CrateScope` row for a crate the program's check reads.
    NotAnswered { crate_name: &'static str },
    /// A crate's `CrateScope` record could not be read as its names and items.
    Records { crate_name: &'static str, why: &'static str },
    /// The program's check scope was refused (blaze-world check_scope.rs), by its rule.
    Scope { rule: &'static str },
    /// The list's packs could not be opened (body_types.rs `open_trees`).
    Trees { why: &'static str },
    /// A crate's items record could not be opened for its edition ([`NamesError::Items`]).
    Items { crate_name: &'static str, why: ItemsRecordError },
    /// The record of the certificate is not of this form.
    Form(FormError),
}

impl fmt::Display for RunNamesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = "blaze-cli run_names.rs";
        match self {
            RunNamesError::NoGraph => write!(f, "the answer holds no `UnitGraph` row (no lock file matched), so the program's crates are not known ({at})"),
            RunNamesError::Store(error) => write!(f, "the program's names records: {error}"),
            RunNamesError::WorldNotMade => write!(f, "the `UnitGraph` record holds no crate list: the crates' world could not be made (its finding says why; {at})"),
            RunNamesError::NonePicked => write!(f, "the request started a program, but this command picked none: only `blaze run` picks one (run_package.rs `pick`; {at})"),
            RunNamesError::NoProgramCrate { root } => write!(f, "no member crate of the crate list has its root at `{root}`, the program `blaze run` starts ({at})"),
            RunNamesError::TwoProgramCrates { root, first, second } => write!(f, "two crates of the crate list, `{first}` and `{second}`, have their root at `{root}` ({at})"),
            RunNamesError::NotAnswered { crate_name } => write!(f, "the answer holds no `CrateScope` row for crate `{crate_name}`, which the program's check reads ({at})"),
            RunNamesError::Records { crate_name, why } => write!(f, "crate `{crate_name}`: {why}"),
            RunNamesError::Scope { rule } => write!(f, "the program's check scope was refused: {rule}"),
            RunNamesError::Trees { why } => write!(f, "{why}"),
            RunNamesError::Items { crate_name, why } => write!(f, "the program's names records: the items record of `{crate_name}`: {why:?}"),
            RunNamesError::Form(error) => write!(f, "the record of the program's names rows at the certificate: {error} ({at})"),
        }
    }
}

/// A store's refusal, boxed (`RunNamesError::Store`).
fn stored(error: CliError) -> RunNamesError {
    RunNamesError::Store(std::boxed::Box::new_in(error, Global))
}

/// The program's crates from `answered`: the crate list of its `UnitGraph` row (read once a
/// command, `Kept::list`), the member crate rooted at `program_root` (the picked program's crate
/// root, checkout-relative: run_package.rs `Program::root`, `bin/tempo/src/main.rs` for a member
/// binary), its `CrateScope` record's `no_core`, and its check scope's crates, each by its row.
/// COST: the list's read and the program's records once a command; the scope, O(crates + edges).
pub(crate) fn program_of(
    cx: &Cx,
    answered: &dyn Answered,
    program_root: &str,
    store: &mut dyn Store<'_>,
    kept: &mut Kept,
    heap: &'static blaze_own::Heap,
    scratch: &Frame,
) -> Result<Program, RunNamesError> {
    let _cx = cx.span("run names: the program's crates");
    let graph = answered.graph().ok_or(RunNamesError::NoGraph)?;
    let list = kept.list(cx, store, &graph, scratch).map_err(stored)?.ok_or(RunNamesError::WorldNotMade)?;
    let mut owner: Option<usize> = None;
    for at in 0..CrateGraph::len(&list) {
        if list.member(at) && list.root(at).strip_prefix("/0/") == Some(program_root) {
            if let Some(first) = owner {
                return Err(RunNamesError::TwoProgramCrates { root: std::boxed::Box::from(program_root), first: CrateGraph::name(&list, first), second: CrateGraph::name(&list, at) });
            }
            owner = Some(at);
        }
    }
    let owner = owner.ok_or_else(|| RunNamesError::NoProgramCrate { root: std::boxed::Box::from(program_root) })?;
    let input_of = |at: usize| answered.crate_input(at, blaze_drive::key::crate_scope_name(&list.key(at))).ok_or(RunNamesError::NotAnswered { crate_name: CrateGraph::name(&list, at) });
    let own = input_of(owner)?;
    let records = kept.records(cx, store, &own, list.key(owner), heap).map_err(|why| RunNamesError::Records { crate_name: CrateGraph::name(&list, owner), why })?;
    // A `#![no_core]` program links no sysroot crate (body_types.rs `prepare`, the same read).
    let no_core = blaze_scope::names_record::NamesRecord::open(records.names_proof).crate_root().head().no_core;
    let scope = blaze_world::check_scope::check_scope_in(&list, owner, no_core, heap.alloc()).map_err(|refused| RunNamesError::Scope { rule: refused.rule() })?;
    let mut crates: AVec<'static, (usize, Input)> = AVec::new_in(heap.alloc());
    for at in scope.crates() {
        crates.push((at, input_of(at)?));
    }
    let mut lang: AVec<'static, usize> = AVec::new_in(heap.alloc());
    lang.extend(scope.lang_places());
    Ok(Program { graph, list, crates, lang })
}

/// On a walk: the record of the program's rows at `cert` ([`inputs_key`]), so a run that
/// stands on the certificate finds the names records ([`Stood`]). One put.
pub(crate) fn put(cx: &Cx, program: &Program, cert: Key, store: &mut dyn Store<'_>, heap: &'static blaze_own::Heap) -> Result<(), RunNamesError> {
    let _cx = cx.span("run names: write the program's rows");
    let mut bytes: AVec<'static, u8> = AVec::with_capacity_in(HEAD + program.crates.len() * ROW, heap.alloc());
    encode(program.graph, &program.crates, &mut bytes, heap.alloc())?;
    store.put(cx, inputs_key(cert), &bytes).map_err(stored)
}

/// On a hit: the record of `cert`'s rows, or `None` when the store holds none (the command then
/// answers in full). One get; the bytes are checked whole.
pub(crate) fn stood<'m>(cx: &Cx, cert: Key, store: &mut dyn Store<'m>) -> Result<Option<Stood<'m>>, RunNamesError> {
    let Some(held) = store.get(cx, inputs_key(cert)).map_err(stored)? else { return Ok(None) };
    let bytes: &'m [u8] = held.bytes;
    Stood::open(bytes).map(Some).map_err(RunNamesError::Form)
}

/// Where the program's names rows are, found when the program starts: on a walk, the program's
/// crates (made now, and written at the certificate, [`put`]); on a hit, the certificate's record.
pub(crate) enum Found<'b> {
    Walked(Program),
    Stood(Stood<'b>),
}

impl Found<'_> {
    /// The crate list a walk read (what run_ids.rs `for_run` takes); `None` on a hit.
    pub(crate) fn list(&self) -> Option<&CrateList<'static>> {
        match self {
            Found::Walked(program) => Some(&program.list),
            Found::Stood(_) => None,
        }
    }
}

/// [`Found`] for the request `req` at its `Effect::StartProgram`, before the program starts;
/// `None`: the request stood and the store holds no record of its rows, so the command answers in
/// full (main.rs `CliError::AnswerInFull`). A walk: [`program_of`] and one put; a hit: one get.
pub(crate) fn found<'m>(
    cx: &Cx,
    req: &blaze_drive::Request<'_>,
    program_root: &str,
    store: &mut dyn Store<'m>,
    kept: &mut Kept,
    heap: &'static blaze_own::Heap,
    scratch: &Frame,
) -> Result<Option<Found<'m>>, RunNamesError> {
    if crate::run_ids::walked(req) {
        let program = program_of(cx, &Walked(req), program_root, store, kept, heap, scratch)?;
        put(cx, &program, req.certificate(), store, heap)?;
        return Ok(Some(Found::Walked(program)));
    }
    Ok(stood(cx, req.certificate(), store)?.map(Found::Stood))
}

/// The names records of `found` (the lazy parent's `names` closure, called at most once): on a
/// hit the program's crates are found from the record first ([`program_of`] over [`Stood`]).
#[expect(clippy::too_many_arguments, reason = "the StartProgram site's edge: each argument one thing it holds")]
pub(crate) fn names_of<'n>(
    cx: &'n Cx,
    root: &'n Dir<ReadWrite>,
    found: &Found<'_>,
    program_root: &str,
    store: &mut dyn Store<'_>,
    kept: &mut Kept,
    env: &crate::fetch::FetchEnv<'_>,
    procs: Option<&'n ProcRunner<'static>>,
    frame: &'n Frame,
    heap: &'static blaze_own::Heap,
) -> Result<NamesRecords<'n>, RunNamesError> {
    match found {
        Found::Walked(program) => open(cx, root, program, store, kept, env, procs, frame, heap),
        Found::Stood(rows) => {
            let program = program_of(cx, rows, program_root, store, kept, heap, frame)?;
            open(cx, root, &program, store, kept, env, procs, frame, heap)
        }
    }
}

/// The names records of `program` (the lazy parent's `names`), in `frame`: each crate's records
/// (already read, `Kept`), the list's packs, and the command's proc-macro runner when the names
/// world was made this command (`None`: a body's proc-macro call is refused by name).
#[expect(clippy::too_many_arguments, reason = "the StartProgram site's edge: each argument one thing it holds")]
pub(crate) fn open<'n>(
    cx: &'n Cx,
    root: &'n Dir<ReadWrite>,
    program: &Program,
    store: &mut dyn Store<'_>,
    kept: &mut Kept,
    env: &crate::fetch::FetchEnv<'_>,
    procs: Option<&'n ProcRunner<'static>>,
    frame: &'n Frame,
    heap: &'static blaze_own::Heap,
) -> Result<NamesRecords<'n>, RunNamesError> {
    let _cx = cx.span("run names: open the names records");
    let list = &program.list;
    let mut crates: Vec<Crate<'n>, &Frame> = Vec::with_capacity_in(program.crates.len(), frame);
    for (at, input) in program.crates.iter() {
        let crate_name = CrateGraph::name(list, *at);
        let records = kept.records(cx, store, input, list.key(*at), heap).map_err(|why| RunNamesError::Records { crate_name, why })?;
        crates.push(Crate { records, name: crate_name, package: list.shown_from(*at), world_at: *at });
    }
    let (packs, corrected) = kept.trees(cx, env, program.graph.out, list, frame).map_err(|why| RunNamesError::Trees { why })?;
    // No crate's environment yet: a run's bodies' `env!` is refused by name (the next piece:
    // each crate's `CrateList::env_facts`, as body_types.rs `owner_env` makes the owner's).
    let opened = names_records(cx, root, &crates, packs, corrected, procs, std::vec::Vec::new_in(Global), None, frame, heap.alloc()).map_err(|error| match error {
        NamesError::Items { at, why } => RunNamesError::Items { crate_name: program.crates.get(at).map_or("?", |(at, _)| CrateGraph::name(list, *at)), why },
    })?;
    let mut lang: Vec<usize, &'n Frame> = Vec::with_capacity_in(program.lang.len(), frame);
    lang.extend_from_slice(&program.lang);
    Ok(NamesRecords { records: opened.records, frame, lang_crates: lang.leak(), body_sources: Some(opened.sources) })
}
