//! The names records of a program's crates, made ONE way for `blaze check` and `blaze run`
//! (rule 30): front2's `Records` over front_load's `StoreLoad`, its `Log`, and the texts a body's
//! macro calls expand with ([`names_records_with_snapshot`]). body_types.rs `checker` makes them for a file's
//! owner; `blaze run` for the program's crate ([`names_in`]), which the lazy parent opens on its first
//! need of them (run_parent.rs `CommandFront`, blaze-ladder make_one.rs `NamesRecords`).
//!
//! WHICH CRATES `blaze run` READS: the program's crate (the member crate rooted at the picked
//! program's root, run_package.rs `Program::root`, in the crate list the `UnitGraph` record
//! holds) and its check scope (blaze-world check_scope.rs, the one rule of which crates a body
//! may see), each by its `CrateScope` record, the owner at 0: exactly what `blaze check` reads
//! for that root.
//!
//! WHERE THE RECORDS ARE FOUND. Every row is the engine's stored answer's ([`FromEngine`],
//! engine_check.rs `Standing`): the `UnitGraph`'s row and each crate's `CrateScope` row. A
//! test suite carries the rows it reads in [`Stood`]'s form, made in place from the answer
//! ([`found_tests_answered`]); nothing is put at a certificate, since the next start reads the
//! same answer.
//!
//! FORM (little-endian): [`MAGIC`] (16 bytes), the `UnitGraph` row's `at` and `out` (32 + 32),
//! the count `n` (u32), then `n` rows of (the crate's index in the list u32, its `CrateScope`
//! row's `at` 32, `out` 32), strictly increasing by index. Checked whole when opened.

use core::fmt;

use blaze_drive::{Input, Key, Name};
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::front2::body_host::Sourced;
use blaze_ladder::front2::source::{Log, Records};
use blaze_ladder::run::make_one::NamesRecords;
use blaze_own::{AVec, Alloc, Vec};
use blaze_core::SharedSet;
use blaze_scope::items_record::ItemsRecordError;
use blaze_scope::tests_record::{ShouldPanic, TestRow, TestsRecord, TestsRecordError};
use blaze_world::crate_list::{CrateGraph, CrateList};
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::Frame;
use core::cell::RefCell;
use std::alloc::Global;

use crate::CliError;
use crate::body_types::{Kept, PackHome, SharedTrees, SyncLazyTrees, SyncPackDirs, TreesSeen};
use crate::engine_records::{EngineRecords, ReadCounts, ReadError};
use crate::front_load::{CrateRecords, StoreLoad};
use crate::job_texts::{JobTexts, Packs};
use crate::proc_runner::ProcRunner;
use crate::store::{SharedSnapshot, Store, StoreSnapshot};

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
    #[expect(
        dead_code,
        reason = "no longer made: the edition is the load's (front_load.rs `CrateRecords::edition`); the variant goes with the port that makes `names_records` infallible, its matches in body_types.rs and `open` with it (lane S4, NOT DONE)"
    )]
    Items { at: usize, why: ItemsRecordError },
}


/// THE ONE WAY names records are made (rule 30): over `crates` in reader order (the owner at 0),
/// each crate a proc-macro caller by its place and its items record's edition, the texts served
/// from the checkout at `root`, the list's `packs` (opened before, or each at its first read:
/// job_texts.rs `Packs`) and `corrected` files, with `read` the checkout
/// files already read (a body job's own file), all kept in `frame` for the caller's lifetime.
/// `env` is the environment of the crate at 0 (a body job's owner, body_types.rs `owner_env`):
/// what a body's `env!` reads; every other crate's `env!` is refused by name (body_host.rs).
/// Nothing is read of a crate here: `Records::new` opens each on the first question about it.
/// COST: no items-record open here (each crate's edition is its load's); the records open each
/// crate's items record once, at its first question.
/// The worker borrows its phase's snapshot for expansion texts and its planner's outlines
/// for current file ranges.
#[expect(clippy::too_many_arguments, reason = "the names records' edge: each argument one input of them")]
pub(crate) fn names_records_with_snapshot<'n>(
    cx: &'n Cx,
    root: &'n Dir<ReadWrite>,
    crates: &[Crate<'n>],
    files: (impl Into<Packs<'n>>, Option<&'n [crate::engine_tasks::Outlined<'n>]>),
    corrected: &'n [(u32, &'n str, &'n str)],
    procs: Option<&'n ProcRunner<'static>>,
    read: std::vec::Vec<(&'n str, Option<&'n str>), Global>,
    env: Option<&'n blaze_scope::crate_env::CrateEnv<'n>>,
    snapshot: Option<&'n StoreSnapshot>,
    expansion_keys: Option<&'n SharedSet<Key, blaze_own::SharedAlloc<'static>>>,
    frame: &'n Frame,
    g: Alloc<'static>,
) -> Result<Opened<'n>, NamesError> {
    names_records_over(cx, root, ScopeCrates::Opened(crates), files, corrected, procs, read, env, snapshot, expansion_keys, frame, g)
}

/// The crates names records are read over, in reader order (the owner at 0): THE SEAM of piece C
/// (a) (rule 78), as front_load.rs [`CratesFrom`] serves them. [`ScopeCrates::Opened`]: each
/// crate's records opened before (the Heads stage, `blaze run`, the bodies' oracle arm);
/// [`ScopeCrates::ByNeed`]: the owner's opened, every other crate's at its first question.
#[derive(Clone, Copy)]
pub(crate) enum ScopeCrates<'a, 'n> {
    Opened(&'a [Crate<'n>]),
    ByNeed(ByNeedScope<'n>),
}

/// A scope whose crates past the owner are opened by need ([`ScopeCrates::ByNeed`]).
#[derive(Clone, Copy)]
pub(crate) struct ByNeedScope<'n> {
    pub(crate) own: Crate<'n>,
    pub(crate) rest: &'n [crate::front_load::Unopened],
    pub(crate) records: &'n dyn crate::front_load::OpenByNeed,
}

impl<'n> ScopeCrates<'_, 'n> {
    pub(crate) fn len(&self) -> usize {
        match self {
            ScopeCrates::Opened(crates) => crates.len(),
            ScopeCrates::ByNeed(scope) => scope.rest.len() + 1,
        }
    }

    /// Reader crate `k`'s package directory (`CrateInput::shown_from`), known without its record.
    pub(crate) fn package(&self, k: usize) -> Option<&'n str> {
        match self {
            ScopeCrates::Opened(crates) => crates.get(k).map(|one| one.package),
            ScopeCrates::ByNeed(scope) => match k {
                0 => Some(scope.own.package),
                k => scope.rest.get(k - 1).map(|one| one.package),
            },
        }
    }

    /// Reader crate `k`'s records, opened by need; `None` when there is no crate `k` or its
    /// record did not open (a job that asks it is refused by its load: front_load.rs
    /// `StoreLoad::refused`).
    pub(crate) fn records(&self, k: usize) -> Option<CrateRecords<'n>> {
        match self {
            ScopeCrates::Opened(crates) => crates.get(k).map(|one| one.records),
            ScopeCrates::ByNeed(scope) => match k {
                0 => Some(scope.own.records),
                k => scope.rest.get(k - 1).and_then(|one| scope.records.open(one).ok()),
            },
        }
    }
}

/// [`names_records_with_snapshot`] over either source of crates ([`ScopeCrates`]). On
/// [`ScopeCrates::ByNeed`], only the owner is listed as a caller at setup. Every crate after it
/// is a proc-macro caller too: its edition is read from its records, opened at its first call
/// (job_texts.rs `JobTexts::caller`). A body job lowers another crate's body (front2
/// opaque_auto.rs, the defining body of an `impl Trait`), and a call written there runs at
/// that body's crate. The run is keyed by the call and by the caller's package and edition
/// (rule 21), so it is the same whichever job asks for it. `env!` is still read for the owner
/// only: a dependency's `env!` is refused by name (job_texts.rs `crate_env`); a run's names
/// records ([`names_in`]) give every crate its own ([`RunEnvs`], through [`names_records_envs`]).
#[expect(clippy::too_many_arguments, reason = "the names records' edge: each argument one input of them")]
pub(crate) fn names_records_over<'n>(
    cx: &'n Cx,
    root: &'n Dir<ReadWrite>,
    crates: ScopeCrates<'_, 'n>,
    files: (impl Into<Packs<'n>>, Option<&'n [crate::engine_tasks::Outlined<'n>]>),
    corrected: &'n [(u32, &'n str, &'n str)],
    procs: Option<&'n ProcRunner<'static>>,
    read: std::vec::Vec<(&'n str, Option<&'n str>), Global>,
    env: Option<&'n blaze_scope::crate_env::CrateEnv<'n>>,
    snapshot: Option<&'n StoreSnapshot>,
    expansion_keys: Option<&'n SharedSet<Key, blaze_own::SharedAlloc<'static>>>,
    frame: &'n Frame,
    g: Alloc<'static>,
) -> Result<Opened<'n>, NamesError> {
    names_records_envs(cx, root, crates, files, corrected, crate::job_texts::Procs::of(procs), read, Envs { owner: env, rest: None }, snapshot, expansion_keys, frame, g)
}

/// The environments a names records' texts serve ([`JobTexts::env`], [`JobTexts::envs`]): the
/// owner's (crate 0), and every other crate's made by need (`None`: refused by name).
#[derive(Clone, Copy)]
pub(crate) struct Envs<'n> {
    pub(crate) owner: Option<&'n blaze_scope::crate_env::CrateEnv<'n>>,
    pub(crate) rest: Option<&'n dyn crate::job_texts::EnvsByNeed<'n>>,
}

/// [`names_records_over`] with the environments of [`Envs`]: THE ONE BODY of the names records.
#[expect(clippy::too_many_arguments, reason = "the names records' edge: each argument one input of them")]
pub(crate) fn names_records_envs<'n>(
    cx: &'n Cx,
    root: &'n Dir<ReadWrite>,
    crates: ScopeCrates<'_, 'n>,
    files: (impl Into<Packs<'n>>, Option<&'n [crate::engine_tasks::Outlined<'n>]>),
    corrected: &'n [(u32, &'n str, &'n str)],
    procs: crate::job_texts::Procs<'n>,
    read: std::vec::Vec<(&'n str, Option<&'n str>), Global>,
    envs: Envs<'n>,
    snapshot: Option<&'n StoreSnapshot>,
    expansion_keys: Option<&'n SharedSet<Key, blaze_own::SharedAlloc<'static>>>,
    frame: &'n Frame,
    g: Alloc<'static>,
) -> Result<Opened<'n>, NamesError> {
    let count = crates.len();
    let mut crate_names: Vec<&'n str, &'n Frame> = Vec::with_capacity_in(count, frame);
    let mut packages: Vec<&'n str, &'n Frame> = Vec::with_capacity_in(count, frame);
    // Each crate as a proc-macro caller, by its place, which is its `krate` in the records made
    // below in this order: its index in the names world and its edition.
    let mut callers: Vec<(usize, usize, blaze_expand::Edition), &'n Frame> = Vec::with_capacity_in(count, frame);
    // On a scope opened by need, every crate past the owner is a caller whose edition is read at
    // its first call (job_texts.rs `JobTexts::caller`).
    let mut by_need: Option<(&'n [crate::front_load::Unopened], &'n dyn crate::front_load::OpenByNeed)> = None;
    let from = match crates {
        ScopeCrates::Opened(crates) => {
            let mut records: Vec<CrateRecords<'n>, &'n Frame> = Vec::with_capacity_in(count, frame);
            for (at, one) in crates.iter().enumerate() {
                records.push(one.records);
                crate_names.push(one.name);
                packages.push(one.package);
                // The edition the load's one open read (front_load.rs `CrateRecords::edition`):
                // the section is not opened again here; the records made below open it once.
                callers.push((at, one.world_at, one.records.edition));
            }
            crate::front_load::CratesFrom::Opened(records.into_bump_slice())
        }
        ScopeCrates::ByNeed(scope) => {
            crate_names.push(scope.own.name);
            packages.push(scope.own.package);
            callers.push((0, scope.own.world_at, scope.own.records.edition));
            by_need = Some((scope.rest, scope.records));
            for one in scope.rest {
                crate_names.push(one.name);
                packages.push(one.package);
            }
            crate::front_load::CratesFrom::ByNeed { own: scope.own.records, rest: scope.rest, records: scope.records }
        }
    };
    let (crate_names, packages): (&'n [&'n str], &'n [&'n str]) = (crate_names.into_bump_slice(), packages.into_bump_slice());
    let callers: &'n [(usize, usize, blaze_expand::Edition)] = callers.into_bump_slice();
    let texts: &'n JobTexts<'n> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(
        JobTexts {
            cx,
            root,
            frame,
            packs: files.0.into(),
            corrected,
            // The first crate's environment (a body job's owner): what its bodies' `env!` reads;
            // every other crate's made at its first ask, when the caller gave a maker.
            env: envs.owner.map(|env| (0, env)),
            envs: envs.rest,
            read: RefCell::new(read),
            unread: RefCell::new(blaze_own::Vec::new_in(frame)),
            data: RefCell::new(std::vec::Vec::new()),
            left: RefCell::new(std::vec::Vec::new()),
            procs,
            own: None,
            callers,
            by_need,
            run_stopped: core::cell::Cell::new(None),
            activity: None,
        },
        frame,
    ));
    let storage = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(blaze_own::Erased::new(frame), frame));
    let load: &'n StoreLoad<'n> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(StoreLoad::from_crates(from, packages, texts, snapshot, expansion_keys, files.1, storage.handle()), frame));
    let log: &'n Log<'n> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(Log::new(crate_names, g), frame));
    let keys = load.keys(frame);
    let records: &'n Records<'n> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(Records::new(keys, load, log, frame, g), frame));
    let sources: &'n Sourced<'n, JobTexts<'n>> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(Sourced(texts), frame));
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

/// Where a run finds the rows of the names it reads: the `UnitGraph`'s, and each crate's
/// `CrateScope` row by its index in the list and its name.
pub(crate) trait Answered {
    fn graph(&self) -> Option<Input>;
    fn crate_input(&self, at: usize, name: Name) -> Option<Input>;
    /// The row of the build-script run `name` of the crate at list index `at` (body_types.rs
    /// `owner_script`): what the crate's environment joins ([`RunEnvs`]). `None`: no row, or
    /// a row that failed.
    fn script_input(&self, at: usize, name: Name) -> Option<Input>;
}

/// The rows of the engine's stored answer (engine_check.rs `Standing`): the unit graph's row,
/// and each crate's `CrateScope` row as the check's names stage gave it (`NamesStage::input_of`:
/// its record key and out, `None` for a crate that failed). Every row is the answer's, so a
/// start keeps no record of its own of them: the next start reads the same answer.
#[derive(Clone, Copy)]
pub(crate) struct FromEngine<'s> {
    pub(crate) graph: Input,
    pub(crate) names: &'s dyn Fn(Name) -> Option<Input>,
}

impl Answered for FromEngine<'_> {
    fn graph(&self) -> Option<Input> {
        Some(self.graph)
    }

    fn crate_input(&self, _at: usize, name: Name) -> Option<Input> {
        (self.names)(name)
    }

    fn script_input(&self, _at: usize, name: Name) -> Option<Input> {
        (self.names)(name)
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
    /// A crates' build-script rows over another graph row than the crates' rows
    /// ([`Stood::with_scripts`]).
    Graph,
}

impl fmt::Display for FormError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormError::Short { len } => write!(f, "{len} bytes, shorter than the head's {HEAD}"),
            FormError::Magic => write!(f, "its first 16 bytes are not `blaze run names1`"),
            FormError::Rows { count } => write!(f, "it names {count} crates, more than the {MOST_ROWS} a record may"),
            FormError::Length { len, want } => write!(f, "{len} bytes where its count makes {want}"),
            FormError::Unsorted { row } => write!(f, "row {row}'s crate index is not above the row before it"),
            FormError::Graph => f.write_str("its build-script rows are over another `UnitGraph` row than its crates' rows"),
        }
    }
}

/// The rows a request that stood reads: the record of its certificate, checked whole, read in
/// place (each crate a binary search over its rows), and the crates' build-script rows in the
/// same row form ([`Stood::with_scripts`]; none for a record opened alone).
#[derive(Clone, Copy)]
pub(crate) struct Stood<'b> {
    graph: Input,
    rows: &'b [u8],
    scripts: &'b [u8],
}

/// The row of crate index `at` among `rows` (rows of [`ROW`] bytes, strictly increasing by
/// index), named `name`: a binary search.
fn row_at(rows: &[u8], at: usize, name: Name) -> Option<Input> {
    let want = u32::try_from(at).ok()?;
    let (mut low, mut high) = (0, rows.len() / ROW);
    while low < high {
        let mid = low + (high - low) / 2;
        let here = u32_at(rows, mid * ROW);
        if here == want {
            let row = mid * ROW;
            return Some(Input { name, at: Key(ContentId(word(rows, row + 4))), out: ContentId(word(rows, row + 36)) });
        }
        if here < want {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    None
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
        Ok(Stood { graph, rows, scripts: &[] })
    }

    /// These rows with `scripts`' rows as the crates' build-script rows: a second record of
    /// this form (its rows each a crate's `Script` row by the crate's index) over the same
    /// graph row; another graph's is refused.
    pub(crate) fn with_scripts(self, scripts: Stood<'b>) -> Result<Stood<'b>, FormError> {
        if scripts.graph != self.graph {
            return Err(FormError::Graph);
        }
        Ok(Stood { scripts: scripts.rows, ..self })
    }
}

impl Answered for Stood<'_> {
    fn graph(&self) -> Option<Input> {
        Some(self.graph)
    }

    fn crate_input(&self, at: usize, name: Name) -> Option<Input> {
        row_at(self.rows, at, name)
    }

    fn script_input(&self, at: usize, name: Name) -> Option<Input> {
        row_at(self.scripts, at, name)
    }
}

/// The record of `graph` and `rows` (each crate's index and row; any order, one each) in `out`.
pub(crate) fn encode<A: core::alloc::Allocator>(graph: Input, rows: &[(usize, Input)], out: &mut Vec<u8, A>, scratch: Alloc<'_>) -> Result<(), RunNamesError> {
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
    /// The build-script row of each crate of `crates` that has one, by its index in `list`,
    /// sorted: what that crate's environment joins, made at its first `env!` ([`RunEnvs`]).
    scripts: AVec<'static, (usize, Input)>,
    lang: AVec<'static, usize>,
    /// The program's own crate, by its index in `list`.
    owner: usize,
}

impl Program {
    /// The program's crates for its native image (engine_native.rs `image_for`): its own crate
    /// first, then every other crate of its check scope, each by its key and its names row.
    /// Read from the one crate list this program already holds: the unit graph is not read
    /// again for it. `closed` is not a fact the image reads, and is `false` for every crate.
    pub(crate) fn closure_in<'a>(&self, alloc: Alloc<'a>) -> AVec<'a, crate::engine_seeds::CrateAt> {
        let at_of = |at: usize, names_at: Option<Key>| {
            let key = self.list.key(at);
            let mut prefix = [0u8; 8];
            prefix.copy_from_slice(&key.0.0[..8]);
            crate::engine_seeds::CrateAt { prefix, key, names_at, member: self.list.member(at), closed: false, toolchain: CrateGraph::toolchain(&self.list, at) }
        };
        let mut closure = AVec::with_capacity_in(self.crates.len() + 1, alloc);
        let own = self.crates.iter().find(|(at, _)| *at == self.owner).map(|(_, input)| input.at);
        closure.push(at_of(self.owner, own));
        closure.extend(self.crates.iter().filter(|(at, _)| *at != self.owner).map(|(at, input)| at_of(*at, Some(input.at))));
        closure
    }
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
    /// No member crate of a `Bin` unit has its root at `root`, the picked program's crate root
    /// (run_package.rs `Program::root`; [`program_row`]).
    NoProgramCrate { root: std::boxed::Box<str> },
    /// Two `Bin` units' member crates have their root at `root` ([`program_row`]).
    TwoProgramCrates { root: std::boxed::Box<str>, first: &'static str, second: &'static str },
    /// No member crate is at place `at` of the list ([`Owner::At`]).
    NoMemberAt { at: usize },
    TestGraph(blaze_unit_graph::record::RecordRefused),
    TestMap { unit: usize, why: &'static str },
    TestTarget { package: &'static str, crate_name: &'static str, why: &'static str },
    TestRecord { package: &'static str, crate_name: &'static str, why: TestsRecordError },
    /// The answer holds no `CrateScope` row for a crate the program's check reads.
    NotAnswered { crate_name: &'static str },
    /// A crate's `CrateScope` record could not be read in place as its names, items and
    /// expansion texts (engine_records.rs: not in the store, not of this form, from the arena, or
    /// misaligned), by the record. Boxed, as `Store` is: the error holds a `LoadError`, whose
    /// `Store` holds a `CliError`, which holds this type.
    Records { crate_name: &'static str, error: std::boxed::Box<ReadError, Global> },
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
            RunNamesError::NoProgramCrate { root } => write!(f, "no binary unit's member crate has its root at `{root}`, the program `blaze run` starts ({at})"),
            RunNamesError::TwoProgramCrates { root, first, second } => write!(f, "two binary units' crates, `{first}` and `{second}`, have their root at `{root}` ({at})"),
            RunNamesError::NoMemberAt { at: place } => write!(f, "the crate list has no member crate at place {place} ({at})"),
            RunNamesError::TestGraph(why) => write!(f, "the start's unit graph: {why}"),
            RunNamesError::TestMap { unit, why } => write!(f, "the start's map of units to crates, at graph unit {unit}: {why} ({at})"),
            RunNamesError::TestTarget { package, crate_name, why } => write!(f, "package `{package}`, test target crate `{crate_name}`: {why} ({at})"),
            RunNamesError::TestRecord { package, crate_name, why } => write!(f, "package `{package}`, target crate `{crate_name}` tests record: {why} ({at})"),
            RunNamesError::NotAnswered { crate_name } => write!(f, "the answer holds no `CrateScope` row for crate `{crate_name}`, which the program's check reads ({at})"),
            RunNamesError::Records { crate_name, error } => write!(f, "crate `{crate_name}`: {error} ({at})"),
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

/// Crate `crate_name`'s record not read, boxed (`RunNamesError::Records`).
fn unread(crate_name: &'static str, error: ReadError) -> RunNamesError {
    RunNamesError::Records { crate_name, error: std::boxed::Box::new_in(error, Global) }
}

/// Which member crate of the list is the program's.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Owner {
    /// The one at this place in the list: `run`'s binary ([`program_row`], by its unit's kind)
    /// or `test`'s unit ([`TestUnit`]), each of which shares its root with another crate (the
    /// binary's test unit; the library a test unit is compiled from), so never found by root.
    At(usize),
}

/// One runnable Cargo unit, even when the unit has no test functions.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TestUnit<'m> {
    pub(crate) at: usize,
    pub(crate) key: blaze_id::CrateItemKey,
    pub(crate) package: &'static str,
    pub(crate) target: &'m str,
    pub(crate) tests: TestRows<'m>,
    /// Its edition, as its items record says (front_load.rs `CrateRecords::edition`): what its
    /// code is built for (run_parent.rs `builder_for`).
    pub(crate) edition: blaze_surface::Edition,
}

/// A harness supplies one row per marked function. A custom harness supplies its own main
/// as the single runnable entry, and has no tests section in its scope record.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TestRows<'m> {
    Harness(TestsRecord<'m>),
    Main(blaze_id::ItemKey),
}

impl<'m> TestRows<'m> {
    pub(crate) fn harnessed(self) -> bool {
        matches!(self, Self::Harness(_))
    }

    pub(crate) fn len(self) -> usize {
        match self {
            Self::Harness(tests) => tests.len(),
            Self::Main(_) => 1,
        }
    }

    pub(crate) fn row(self, at: usize) -> Result<Option<TestRow<'m>>, TestsRecordError> {
        match self {
            Self::Harness(tests) => tests.row(at),
            Self::Main(key) => Ok((at == 0).then_some(TestRow { key, path: "main", ignore: None, should_panic: ShouldPanic::No, bench: false })),
        }
    }
}

pub(crate) struct TestInventory<'m> {
    pub(crate) suites: AVec<'static, (Found<'m>, TestUnit<'m>)>,
    walked: Option<CrateList<'static>>,
}

impl TestInventory<'_> {
    pub(crate) fn list(&self) -> Option<&CrateList<'static>> {
        self.walked.as_ref()
    }
}

/// One row of the graph's unit-to-crate map per unit, including those with no crate.
/// The world's indices and the crate-list indices have different orders: only their
/// crate keys join them. Root paths are shared by libraries, binaries and their harnesses.
/// Each unit's row of the crate list `list` ([`test_map`]: 4 bytes a unit, `u32::MAX` for a
/// unit that is no crate), kept in the store under the sealed graph's out: the names world is
/// made only on its miss. Copied into `frame` (a few bytes a unit).
/// COST: one get; on a miss, the names world once a command and one put.
#[expect(clippy::too_many_arguments, reason = "the start's edge: each argument one thing it holds")]
fn unit_map<'f, 'm>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store<'m>,
    facts: &mut crate::work::Facts<'f, impl crate::sweep::Stamps>,
    graph: &Input,
    list: &CrateList<'_>,
    heap: &'static blaze_own::Heap,
    frame: &'f Frame,
) -> Result<&'f [u8], RunNamesError> {
    let mut digest = Hasher::new("blaze test: unit rows of a sealed unit graph 1");
    digest.id(&graph.out);
    let map_key = Key(digest.finish());
    if let Some(held) = store.get(cx, map_key).map_err(stored)? {
        return Ok(frame.alloc_slice_copy(held.bytes));
    }
    let erased = blaze_own::Erased::new(frame);
    crate::body_types::names_world(cx, root, store, facts, graph, frame, erased.handle())
        .map_err(stored)?.map_err(|why| RunNamesError::Scope { rule: why })?;
    let names = facts.names.as_ref().ok_or(RunNamesError::WorldNotMade)?;
    let made = test_map(names, list, heap.alloc())?;
    store.put(cx, map_key, &made).map_err(stored)?;
    Ok(frame.alloc_slice_copy(&made))
}

/// `run`'s program crate: the crate list's row of the unit of kind `Bin` (never `for_host`)
/// whose crate root is `program_root` (checkout-relative: run_package.rs `Program::root`).
///
/// Chosen by the UNIT's kind, through the unit map: the engine's unit graph is made with test
/// units (engine_check.rs `checking`, `Graph::new(.., true, ..)`), so a binary's file is the
/// root of two crates, its `Bin` unit's and its `Test { bin }` unit's. Their two rows of the
/// crate list hold the same name, root, package, binary name and member; only their keys
/// differ (the test unit's cfg holds `test`: blaze-world list.rs `unit_crate_key`), and the
/// list holds no cfg, so the row alone cannot say which is the program.
/// COST: [`unit_map`], the unit graph's decode on a map hit, O(units).
#[expect(clippy::too_many_arguments, reason = "the start's edge: each argument one thing it holds")]
pub(crate) fn program_row<'f, 'm>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    answered: &dyn Answered,
    store: &mut dyn Store<'m>,
    facts: &mut crate::work::Facts<'f, impl crate::sweep::Stamps>,
    program_root: &str,
    heap: &'static blaze_own::Heap,
    frame: &'f Frame,
) -> Result<usize, RunNamesError> {
    let _cx = cx.span("run names: the program's unit");
    let graph = answered.graph().ok_or(RunNamesError::NoGraph)?;
    let list = facts.bodies.list(cx, store, &graph, frame).map_err(stored)?.ok_or(RunNamesError::WorldNotMade)?;
    let mapping = unit_map(cx, root, store, facts, &graph, &list, heap, frame)?;
    let decoded;
    let units = match facts.names.as_ref().and_then(|names| names.made()) {
        Some(made) => &made.graph().units,
        None => {
            let bytes = crate::work::payload(cx, store, &graph, frame).map_err(stored)?;
            decoded = blaze_unit_graph::record::decode(bytes.unit_graph(graph.name).map_err(stored)?.graph, heap.alloc()).map_err(RunNamesError::TestGraph)?;
            &decoded.graph.units
        }
    };
    if mapping.len() != units.len() * 4 {
        return Err(RunNamesError::TestMap { unit: units.len(), why: "the number of indices differs from the unit graph" });
    }
    let mut owner: Option<usize> = None;
    for (index, unit) in units.iter().enumerate() {
        if unit.for_host || !matches!(unit.kind, blaze_unit_graph::UnitKind::Bin(_)) {
            continue;
        }
        let at = u32_at(mapping, index * 4) as usize;
        // `u32::MAX`: a unit that is no crate of the list.
        if at >= CrateGraph::len(&list) || !list.member(at) || list.root(at).strip_prefix("/0/") != Some(program_root) {
            continue;
        }
        match owner {
            Some(first) if first != at => {
                return Err(RunNamesError::TwoProgramCrates { root: std::boxed::Box::from(program_root), first: CrateGraph::name(&list, first), second: CrateGraph::name(&list, at) });
            }
            _ => owner = Some(at),
        }
    }
    owner.ok_or_else(|| RunNamesError::NoProgramCrate { root: std::boxed::Box::from(program_root) })
}

fn test_map(
    names: &crate::names::Names<'_>,
    list: &CrateList<'_>,
    alloc: Alloc<'static>,
) -> Result<AVec<'static, u8>, RunNamesError> {
    let world = names.made().ok_or(RunNamesError::WorldNotMade)?.world();
    let mut bytes = AVec::with_capacity_in(world.crate_of_unit.len() * 4, alloc);
    for (unit, krate) in world.crate_of_unit.iter().enumerate() {
        let row = match krate {
            None => u32::MAX,
            Some(at) => {
                let key = world.crates[*at as usize].input.key;
                let row = list.find(key).ok_or(RunNamesError::TestMap { unit, why: "the world's crate key is absent from its crate list" })?;
                u32::try_from(row).map_err(|_| RunNamesError::TestMap { unit, why: "its crate index exceeds u32" })?
            }
        };
        bytes.extend_from_slice(&row.to_le_bytes());
    }
    Ok(bytes)
}

/// Cargo's runnable targets. The graph includes extra targets compiled only to check them,
/// including examples and benches by default. A required feature has to be enabled on the unit.
fn test_target<'a>(unit: &'a blaze_unit_graph::Unit<'_>) -> Option<(&'static str, &'a str, bool)> {
    use blaze_unit_graph::UnitKind;
    let (kind, extra) = match &unit.kind {
        UnitKind::Test { bin: None } => {
            // `package/lib` names its sole library. Show a distinct manifest library name too.
            let target = if unit.crate_name == unit.name { "" } else { unit.crate_name.as_str() };
            return Some(("lib", target, true));
        }
        UnitKind::Test { bin: Some(name) } => return Some(("bin", name.as_str(), true)),
        UnitKind::IntegrationTest(extra) => ("test", extra),
        UnitKind::Example(extra) => ("example", extra),
        UnitKind::Bench(extra) => ("bench", extra),
        _ => return None,
    };
    (extra.test && extra.required_features.iter().all(|f| unit.features.binary_search(f).is_ok()))
        .then_some((kind, extra.name.as_str(), extra.harness))
}

/// Find the user-written main by the same item facts as the program entry. An associated
/// function named main is not the program's entry.
fn main_test(
    records: CrateRecords<'_>, package: &'static str, crate_name: &'static str, alloc: Alloc<'_>,
) -> Result<blaze_id::ItemKey, RunNamesError> {
    let items_error = |why| RunNamesError::Items { crate_name, why };
    let items = blaze_scope::items_record::ItemsRecord::open(records.items, alloc).map_err(items_error)?;
    for row in 0..items.items() {
        let Some(item) = items.identity_at(row).map_err(items_error)? else { continue };
        if item.module == items.root_module() && item.name == Some("main") && item.kind == blaze_surface::ItemKind::Function
            && items.member_owner(item.key).map_err(items_error)?.is_none()
        {
            return Ok(item.key);
        }
    }
    Err(RunNamesError::TestTarget { package, crate_name, why: "the harnessless target has no main function in its crate root" })
}

/// The test suites of `selected` over the engine's stored answer (engine_start.rs `run_tests`):
/// every row is the answer's ([`FromEngine`]), so nothing is put at a certificate and nothing is
/// asked again. The rows cover all the answer's crates, independent of the selection; the
/// separate graph map binds each Cargo unit number to its row of this exact crate list.
#[expect(clippy::too_many_arguments, reason = "the start's edge: each argument one thing it holds")]
pub(crate) fn found_tests_answered<'s, 'f: 's, 'm: 's>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    answered: &dyn Answered,
    store: &mut dyn Store<'m>,
    facts: &mut crate::work::Facts<'f, impl crate::sweep::Stamps>,
    read: &mut EngineRecords<'m>,
    selected: &[&str],
    bin: Option<&str>,
    heap: &'static blaze_own::Heap,
    frame: &'f Frame,
) -> Result<TestInventory<'s>, RunNamesError> {
    let (answer, list) = rows_of(cx, answered, store, facts, heap, frame)?;
    tests_of(cx, root, answer, list, store, facts, read, selected, bin, heap, frame)
}

/// The graph's row and each listed crate's row of `answered`, in [`Stood`]'s form (in `frame`),
/// with the graph's crate list. COST: the list's read (once a command) and O(crates).
fn rows_of<'f, 'm>(
    cx: &Cx,
    answered: &dyn Answered,
    store: &mut dyn Store<'m>,
    facts: &mut crate::work::Facts<'f, impl crate::sweep::Stamps>,
    heap: &'static blaze_own::Heap,
    frame: &'f Frame,
) -> Result<(Stood<'f>, CrateList<'static>), RunNamesError> {
    let graph = answered.graph().ok_or(RunNamesError::NoGraph)?;
    let list = facts.bodies.list(cx, store, &graph, frame).map_err(stored)?.ok_or(RunNamesError::WorldNotMade)?;
    let mut rows = AVec::new_in(heap.alloc());
    // Each crate's build-script row too: what its environment joins when one of its bodies is
    // lowered by need ([`RunEnvs`]). A row naming a script of no kind is skipped here and
    // refused by name at that crate's first `env!` (body_types.rs `owner_script`).
    let mut scripts = AVec::new_in(heap.alloc());
    for at in 0..CrateGraph::len(&list) {
        if let Some(input) = answered.crate_input(at, blaze_drive::key::crate_scope_name(&list.key(at))) {
            rows.push((at, input));
        }
        if let Ok(Some(script)) = crate::body_types::owner_script(&list, at)
            && let Some(input) = answered.script_input(at, script)
        {
            scripts.push((at, input));
        }
    }
    let mut encoded = Vec::with_capacity_in(HEAD + ROW * rows.len(), frame);
    encode(graph, &rows, &mut encoded, heap.alloc())?;
    let encoded: &'f [u8] = encoded.into_bump_slice();
    let mut encoded_scripts = Vec::with_capacity_in(HEAD + ROW * scripts.len(), frame);
    encode(graph, &scripts, &mut encoded_scripts, heap.alloc())?;
    let encoded_scripts: &'f [u8] = encoded_scripts.into_bump_slice();
    let stood = Stood::open(encoded).map_err(RunNamesError::Form)?;
    let stood = stood.with_scripts(Stood::open(encoded_scripts).map_err(RunNamesError::Form)?).map_err(RunNamesError::Form)?;
    Ok((stood, list))
}

/// The suites of `selected` over `answer`'s rows and the graph's crate list `list`; with `bin`
/// (`--bin <name>`), only that binary target's suite (R3).
#[expect(clippy::too_many_arguments, reason = "the start's edge: each argument one thing it holds")]
fn tests_of<'s, 'f: 's, 'm: 's>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    answer: Stood<'s>,
    list: CrateList<'static>,
    store: &mut dyn Store<'m>,
    facts: &mut crate::work::Facts<'f, impl crate::sweep::Stamps>,
    read: &mut EngineRecords<'m>,
    selected: &[&str],
    bin: Option<&str>,
    heap: &'static blaze_own::Heap,
    frame: &'f Frame,
) -> Result<TestInventory<'s>, RunNamesError> {
    let graph = answer.graph;
    let mapping = unit_map(cx, root, store, facts, &graph, &list, heap, frame)?;
    let decoded;
    let units = match facts.names.as_ref().and_then(|names| names.made()) {
        Some(made) => &made.graph().units,
        None => {
            let bytes = crate::work::payload(cx, store, &graph, frame).map_err(stored)?;
            decoded = blaze_unit_graph::record::decode(bytes.unit_graph(graph.name).map_err(stored)?.graph, heap.alloc()).map_err(RunNamesError::TestGraph)?;
            &decoded.graph.units
        }
    };
    if mapping.len() != units.len() * 4 {
        return Err(RunNamesError::TestMap { unit: units.len(), why: "the number of indices differs from the unit graph" });
    }
    let snapshot = run_snapshot(store)?;
    let mut suites = AVec::new_in(heap.alloc());
    for (index, unit) in units.iter().enumerate() {
        let Some((kind, target_name, harness)) = test_target(unit) else { continue };
        if let Some(bin) = bin && (kind != "bin" || target_name != bin) {
            continue;
        }
        // run_package::test_packages orders the selection by package name.
        if selected.binary_search(&unit.name.as_str()).is_err() {
            continue;
        }
        let at = u32_at(mapping, index * 4) as usize;
        if at >= CrateGraph::len(&list) || !list.member(at) {
            return Err(RunNamesError::TestMap { unit: index, why: "the selected target has no member crate at its mapped index" });
        }
        let name = CrateGraph::name(&list, at);
        let key = list.key(at);
        let input = answer.crate_input(at, blaze_drive::key::crate_scope_name(&key)).ok_or(RunNamesError::NotAnswered { crate_name: name })?;
        let records = read.records(cx, store, &input, key, heap.alloc()).map_err(|why| unread(name, why))?;
        let Some((package, version)) = list.env_facts(at).package else {
            return Err(RunNamesError::TestMap { unit: index, why: "the selected target's crate has no package" });
        };
        if package != unit.name.as_str() || version != unit.version.as_str() || name != unit.crate_name.as_str() {
            return Err(RunNamesError::TestMap { unit: index, why: "the mapped crate does not belong to this unit" });
        }
        let tests = if harness {
            let bytes = read.tests(cx, store, &input).map_err(|why| unread(name, why))?;
            if bytes.is_empty() {
                return Err(RunNamesError::TestTarget { package, crate_name: name, why: "the harnessed target's scope has no tests record" });
            }
            let rows = TestsRecord::open(bytes).map_err(|why| RunNamesError::TestRecord { package, crate_name: name, why })?;
            if rows.key() != key {
                return Err(RunNamesError::TestTarget { package, crate_name: name, why: "the tests record belongs to a different unit" });
            }
            TestRows::Harness(rows)
        } else {
            TestRows::Main(main_test(records, package, name, heap.alloc())?)
        };
        let target = if target_name.is_empty() {
            kind
        } else {
            crate::work::held_str(bstd::mem::format_in!(frame, "{kind}:{target_name}").as_str(), frame)
        };
        suites.push((Found::Stood(answer, snapshot.clone()), TestUnit { at, key, package, target, tests, edition: records.edition }));
    }
    suites.sort_unstable_by(|(_, a), (_, b)| (a.package, a.target, a.key).cmp(&(b.package, b.target, b.key)));
    Ok(TestInventory { suites, walked: Some(list) })
}

/// The program's crates from `answered`: the crate list of its `UnitGraph` row (read once a
/// command, `Kept::list`), the member crate `owner` names ([`Owner`]: `run`'s from its `Bin`
/// unit, [`program_row`]; `test`'s by its unit's place), its `CrateScope`
/// record's `no_core`, and its check scope's crates, each by its row.
/// COST: the list's read and the program's records once a command (in place, through `read`:
/// engine_records.rs); the scope, O(crates + edges).
#[expect(clippy::too_many_arguments, reason = "the StartProgram site's edge: each argument one thing it holds")]
pub(crate) fn program_of<'m>(
    cx: &Cx,
    answered: &dyn Answered,
    owner: Owner,
    store: &mut dyn Store<'m>,
    kept: &mut Kept,
    read: &mut EngineRecords<'m>,
    heap: &'static blaze_own::Heap,
    scratch: &Frame,
) -> Result<Program, RunNamesError> {
    let _cx = cx.span("run names: the program's crates");
    let graph = answered.graph().ok_or(RunNamesError::NoGraph)?;
    let list = kept.list(cx, store, &graph, scratch).map_err(stored)?.ok_or(RunNamesError::WorldNotMade)?;
    let owner = match owner {
        Owner::At(at) if at < CrateGraph::len(&list) && list.member(at) => at,
        Owner::At(at) => return Err(RunNamesError::NoMemberAt { at }),
    };
    let input_of = |at: usize| answered.crate_input(at, blaze_drive::key::crate_scope_name(&list.key(at))).ok_or(RunNamesError::NotAnswered { crate_name: CrateGraph::name(&list, at) });
    let own = input_of(owner)?;
    let records = read.records(cx, store, &own, list.key(owner), heap.alloc()).map_err(|error| unread(CrateGraph::name(&list, owner), error))?;
    // A `#![no_core]` program links no sysroot crate (body_types.rs `prepare`, the same read).
    let no_core = blaze_scope::names_record::NamesRecord::open(records.names_proof).crate_root().head().no_core;
    let scope = blaze_world::check_scope::check_scope_in(&list, owner, no_core, heap.alloc()).map_err(|refused| RunNamesError::Scope { rule: refused.rule() })?;
    let mut crates: AVec<'static, (usize, Input)> = AVec::new_in(heap.alloc());
    let mut scripts: AVec<'static, (usize, Input)> = AVec::new_in(heap.alloc());
    for at in scope.crates() {
        crates.push((at, input_of(at)?));
        // A row naming a script of no kind, or a script with no row in the answer, is refused
        // by name at the crate's first `env!` ([`RunEnvs`]), never here: a crate whose bodies
        // read no environment needs none.
        if let Ok(Some(script)) = crate::body_types::owner_script(&list, at)
            && let Some(input) = answered.script_input(at, script)
        {
            scripts.push((at, input));
        }
    }
    scripts.sort_unstable_by_key(|(at, _)| *at);
    let mut lang: AVec<'static, usize> = AVec::new_in(heap.alloc());
    lang.extend(scope.lang_places());
    Ok(Program { graph, list, crates, scripts, lang, owner })
}

/// Where the program's names rows are, found when the program starts: the program's crates
/// (`run`, [`found_answered`]), or a test suite's rows of the answer in [`Stood`]'s form
/// ([`found_tests_answered`]), its crates found when the parent first needs them.
pub(crate) enum Found<'b> {
    Walked(Program, SharedSnapshot),
    Stood(Stood<'b>, SharedSnapshot),
}

fn run_snapshot(store: &dyn Store<'_>) -> Result<SharedSnapshot, RunNamesError> {
    store.snapshot().map_err(stored)?.ok_or_else(|| stored(CliError::Text { what: "the run names reader needs a committed store snapshot for expansion text" }))
}

impl Found<'_> {
    /// The crate list the program's crates were found in (what run_ids.rs `RunIds::new` takes);
    /// `None` for a suite's rows.
    pub(crate) fn list(&self) -> Option<&CrateList<'static>> {
        match self {
            Found::Walked(program, _) => Some(&program.list),
            Found::Stood(..) => None,
        }
    }

    /// The program [`found_answered`] found; `None` for a suite's rows.
    pub(crate) fn program(&self) -> Option<&Program> {
        match self {
            Found::Walked(program, _) => Some(program),
            Found::Stood(..) => None,
        }
    }

    /// The committed store's snapshot the names records read in place: what a runner made by
    /// need reads the unit graph's record from (engine_start.rs `RunProcs`).
    pub(crate) fn snapshot(&self) -> &StoreSnapshot {
        match self {
            Found::Walked(_, snapshot) | Found::Stood(_, snapshot) => &**snapshot,
        }
    }
}

/// [`Found`] over the engine's stored answer (engine_start.rs `run_program`): the program's
/// crates found from `answered`'s rows ([`FromEngine`]), always; nothing put at a certificate,
/// since the next start reads the same answer. COST: as [`program_of`].
#[expect(clippy::too_many_arguments, reason = "the start's edge: each argument one thing it holds")]
pub(crate) fn found_answered<'m>(
    cx: &Cx,
    answered: &dyn Answered,
    owner: Owner,
    store: &mut dyn Store<'m>,
    kept: &mut Kept,
    read: &mut EngineRecords<'m>,
    heap: &'static blaze_own::Heap,
    scratch: &Frame,
) -> Result<Found<'m>, RunNamesError> {
    let program = program_of(cx, answered, owner, store, kept, read, heap, scratch)?;
    Ok(Found::Walked(program, run_snapshot(store)?))
}

/// The names records of `found` (the lazy parent's `names` closure, called at most once): for a
/// suite's rows the program's crates are found from them first ([`program_of`] over [`Stood`]).
#[expect(clippy::too_many_arguments, reason = "the StartProgram site's edge: each argument one thing it holds")]
pub(crate) fn names_of<'n, 'm: 'n>(
    cx: &'n Cx,
    root: &'n Dir<ReadWrite>,
    found: &'n Found<'_>,
    owner: Owner,
    store: &mut dyn Store<'m>,
    kept: &mut Kept,
    read: &mut EngineRecords<'m>,
    env: &'n crate::fetch::FetchEnv<'_>,
    dirs: &'n SyncPackDirs,
    procs: crate::job_texts::Procs<'n>,
    seen: &'n TreesSeen,
    frame: &'n Frame,
    heap: &'static blaze_own::Heap,
) -> Result<NamesRecords<'n>, RunNamesError> {
    let _cx = cx.span("run names: open the names records");
    let crates = crates_of(cx, found, owner, store, kept, read, env, dirs, frame, heap)?;
    names_in(cx, root, crates, procs, seen, frame)
}

/// The [`RunCrates`] of `found` ([`names_of`]'s first half, on the command thread): what every
/// names records of the program are made of, each code worker's too (test_drive.rs `CodePool`).
#[expect(clippy::too_many_arguments, reason = "the StartProgram site's edge: each argument one thing it holds")]
pub(crate) fn crates_of<'n, 'm: 'n>(
    cx: &Cx,
    found: &'n Found<'_>,
    owner: Owner,
    store: &mut dyn Store<'m>,
    kept: &mut Kept,
    read: &mut EngineRecords<'m>,
    env: &'n crate::fetch::FetchEnv<'_>,
    dirs: &'n SyncPackDirs,
    frame: &'n Frame,
    heap: &'static blaze_own::Heap,
) -> Result<RunCrates<'n>, RunNamesError> {
    match found {
        Found::Walked(program, snapshot) => read_crates(cx, program, snapshot, store, kept, read, env, dirs, frame, heap),
        Found::Stood(rows, snapshot) => {
            let program = program_of(cx, rows, owner, store, kept, read, heap, frame)?;
            read_crates(cx, &program, snapshot, store, kept, read, env, dirs, frame, heap)
        }
    }
}

/// What the run's parent read to make its names records, its closing line (rule 16):
/// `names read: crates K, sections in place B bytes, copied C bytes; trees opened T of N`. The
/// crates are the records opened (engine_records.rs `ReadCounts`: a second ask is a hit, not
/// counted again); the trees, the list's packs mapped at their first read ([`TreesSeen`]).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct NamesRead {
    pub(crate) reads: ReadCounts,
    pub(crate) trees_opened: u32,
    pub(crate) trees: u32,
    /// The body ids' line beside it (run_ids.rs `IdsLine`), when the run made them.
    pub(crate) ids: Option<crate::run_ids::IdsLine>,
}

impl NamesRead {
    pub(crate) fn of(reads: ReadCounts, seen: &TreesSeen) -> NamesRead {
        let (trees_opened, trees) = seen.opened();
        NamesRead { reads, trees_opened, trees, ids: None }
    }

    /// With the body ids' line, printed as a second `run` line of the profile.
    pub(crate) fn with_ids(self, ids: crate::run_ids::IdsLine) -> NamesRead {
        NamesRead { ids: Some(ids), ..self }
    }
}

impl fmt::Display for NamesRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "names read: crates {}, sections in place {} bytes, copied {} bytes; trees opened {} of {}",
            self.reads.records,
            self.reads.sections.saturating_sub(self.reads.copied),
            self.reads.copied,
            self.trees_opened,
            self.trees
        )?;
        // phases.rs prints this value after `run          `: the second line is indented alike.
        match self.ids {
            Some(ids) => write!(f, "\nrun          {ids}"),
            None => Ok(()),
        }
    }
}

/// The environment of each crate of a run's names records, made at the first ask for it
/// (job_texts.rs `EnvsByNeed`): what a body of that crate lowered by need reads through
/// `env!`/`option_env!`, its includes' package folder and `file!()`, made by the one
/// construction `blaze check` makes its body owner's with (body_types.rs `row_env`, rule 30):
/// the crate list's facts of its row and, when it has a build script, the payload of its
/// `Script` row in the answer, read in place from the committed snapshot. The Reference: `env!`
/// reads the environment "of the crate being compiled"; for a dependency that is ITS package's
/// `CARGO_PKG_*` and `CARGO_MANIFEST_DIR` and its build script's `cargo:rustc-env`.
///
/// THE READ (rule 21): every answer a body's expansion takes from it is in that call's read
/// list (blaze-expand cx.rs: `ReadKind::Env` with the value's digest; an include's bytes), and
/// what it is made from is the answer's: the crate list is the `UnitGraph` row's and the script
/// row the answer's, both under the answer's root, which certifies the run (start_list.rs
/// `Certificate::of_answer`): an edited `version` or a changed `rustc-env` is another answer.
/// COST: per crate asked, its list row and at most one record read in place; never at setup.
pub(crate) struct RunEnvs<'n> {
    list: &'n CrateList<'static>,
    /// Reader crate `k`'s row of `list` (the names records' order, the owner at 0).
    rows: &'n [usize],
    /// [`Program::scripts`]: each crate's build-script row by its row of `list`, sorted.
    scripts: &'n [(usize, Input)],
    snapshot: &'n StoreSnapshot,
    /// The home whose file area holds a native script's stored `OUT_DIR` files.
    home: Option<&'n str>,
    frame: &'n Frame,
    /// Reader crate `k`'s environment, or the words of why it could not be made: made once.
    made: Vec<core::cell::OnceCell<Result<&'n blaze_scope::crate_env::CrateEnv<'n>, &'static str>>, &'n Frame>,
}

impl<'n> RunEnvs<'n> {
    pub(crate) fn new(list: &'n CrateList<'static>, rows: &'n [usize], scripts: &'n [(usize, Input)], snapshot: &'n StoreSnapshot, home: Option<&'n str>, frame: &'n Frame) -> RunEnvs<'n> {
        let mut made = Vec::with_capacity_in(rows.len(), frame);
        made.resize_with(rows.len(), core::cell::OnceCell::new);
        RunEnvs { list, rows, scripts, snapshot, home, frame, made }
    }

    /// The one making of list row `at`'s environment.
    fn make(&self, at: usize) -> Result<&'n blaze_scope::crate_env::CrateEnv<'n>, &'static str> {
        let script = match crate::body_types::owner_script(self.list, at)? {
            None => None,
            Some(name) => {
                let found = self.scripts.binary_search_by_key(&at, |(row, _)| *row).ok().and_then(|i| self.scripts.get(i)).filter(|(_, input)| input.name == name);
                let (_, input) = found.ok_or("its build-script run has no row in the answer, or a row that failed (run_names.rs `Program::scripts`)")?;
                let bytes = self.snapshot.get(input.at).map_err(|_| "its build-script record could not be read from the store's snapshot")?;
                let bytes = bytes.ok_or("its build-script record is not in the store's snapshot")?;
                match blaze_drive::record::open(bytes) {
                    Ok(blaze_drive::record::Stored::Whole(view)) => Some(view.payload),
                    Ok(blaze_drive::record::Stored::Shape(_)) => return Err("its build-script record is a shape, with no payload"),
                    Err(_) => return Err("its build-script record is not one of this tool's records"),
                }
            }
        };
        crate::body_types::row_env(self.list, at, script, self.home, self.frame)
    }
}

impl<'n> crate::job_texts::EnvsByNeed<'n> for RunEnvs<'n> {
    fn env_of(&self, krate: usize) -> Option<&'n blaze_scope::crate_env::CrateEnv<'n>> {
        let (&at, slot) = self.rows.get(krate).zip(self.made.get(krate))?;
        slot.get_or_init(|| self.make(at)).ok()
    }
}

/// What every names records of one program are made of, read ONCE on the command thread
/// ([`read_crates`]: each crate's records read in place through the store's mapping): the crate
/// list, the crates, each crate's list row and build-script row, the lang crates, the list's
/// corrected texts and where its trees are opened from. All borrowed and read only, so it is
/// `Sync`: the command's code workers (test_drive.rs `CodePool`, D194) each make their own names
/// records of it ([`names_in`]), and the command thread makes its own the same way ([`names_of`]).
///
/// WHAT IS SHARED, AND WHAT IS NOT (D194 piece 2): shared read only are the crates' records
/// (views of the store's mapping), the list, the snapshot, the corrected texts, the command-wide
/// lazy pack cache ([`SyncLazyTrees`]), the command-scoped pack directories ([`SyncPackDirs`]),
/// and the packs' home; per maker of names records ([`names_in`], one a worker a suite) are its
/// open/refusal counter ([`TreesSeen`]), the environments made by need ([`RunEnvs`]: `OnceCell`),
/// and the records' own memo (`Records`, the `Log`, the job texts), all in that maker's frame.
#[derive(Clone, Copy)]
pub(crate) struct RunCrates<'n> {
    list: &'n CrateList<'static>,
    crates: &'n [Crate<'n>],
    /// Reader crate `k`'s row of `list` (the owner at 0).
    rows: &'n [usize],
    /// [`Program::scripts`], sorted by row.
    scripts: &'n [(usize, Input)],
    lang: &'n [usize],
    corrected: &'static [(u32, &'static str, &'static str)],
    /// Command-wide lazy pack opener (`Kept::lazy_trees`), shared across every suite and worker
    /// so each pack is verified and mapped at most once.
    packs: &'static SyncLazyTrees<'static>,
    /// Command-scoped pack directory cache (`shelf` and `sources`), dropped cleanly when the
    /// command returns instead of leaking one secondary driver per suite.
    dirs: &'n SyncPackDirs,
    snapshot: &'n StoreSnapshot,
    /// Where the list's packs are opened from (the home, its file tree, the heap a mapping is
    /// kept on); its `home` is also where a native script's stored `OUT_DIR` files are.
    from: PackHome<'n>,
    /// The command's heap: the names records' tables (`names_records_envs`'s `g`).
    heap: &'static blaze_own::Heap,
}

/// The [`RunCrates`] of `program`, in `frame`: each crate's records read IN PLACE from the
/// store's mapping through `read` (engine_records.rs, the one reader of `blaze check` too).
#[expect(clippy::too_many_arguments, reason = "the StartProgram site's edge: each argument one thing it holds")]
pub(crate) fn read_crates<'n, 'm: 'n>(
    cx: &Cx,
    program: &Program,
    snapshot: &'n StoreSnapshot,
    store: &mut dyn Store<'m>,
    kept: &mut Kept,
    read: &mut EngineRecords<'m>,
    env: &'n crate::fetch::FetchEnv<'_>,
    dirs: &'n SyncPackDirs,
    frame: &'n Frame,
    heap: &'static blaze_own::Heap,
) -> Result<RunCrates<'n>, RunNamesError> {
    let list: &'n CrateList<'static> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(program.list, frame));
    let mut crates: Vec<Crate<'n>, &'n Frame> = Vec::with_capacity_in(program.crates.len(), frame);
    for (at, input) in program.crates.iter() {
        let crate_name = CrateGraph::name(list, *at);
        let records: CrateRecords<'m> = read.records(cx, store, input, list.key(*at), heap.alloc()).map_err(|error| RunNamesError::Records { crate_name, error: std::boxed::Box::new_in(error, Global) })?;
        crates.push(Crate { records, name: crate_name, package: list.shown_from(*at), world_at: *at });
    }
    let (packs, corrected) = kept.lazy_trees(env, program.graph.out, program.list).map_err(|why| RunNamesError::Trees { why })?;
    let mut rows: Vec<usize, &'n Frame> = Vec::with_capacity_in(program.crates.len(), frame);
    rows.extend(program.crates.iter().map(|(at, _)| *at));
    let mut scripts: Vec<(usize, Input), &'n Frame> = Vec::with_capacity_in(program.scripts.len(), frame);
    scripts.extend_from_slice(&program.scripts);
    let mut lang: Vec<usize, &'n Frame> = Vec::with_capacity_in(program.lang.len(), frame);
    lang.extend_from_slice(&program.lang);
    Ok(RunCrates { list, crates: crates.into_bump_slice(), rows: rows.into_bump_slice(), scripts: scripts.into_bump_slice(), lang: lang.into_bump_slice(), corrected, packs, dirs, snapshot, from: PackHome::of(env), heap })
}

/// The names records over `crates`, in `frame`, on the calling thread (`cx`'s): the list's trees
/// each opened at the first read of a text from it (counted in `seen`), every crate's
/// environment made at its first ask, and `procs` the proc-macro runner a body's call reads.
/// Everything it makes is this caller's own (the doc of [`RunCrates`]); nothing in `crates` is
/// written.
pub(crate) fn names_in<'n>(
    cx: &'n Cx,
    root: &'n Dir<ReadWrite>,
    crates: RunCrates<'n>,
    procs: crate::job_texts::Procs<'n>,
    seen: &'n TreesSeen,
    frame: &'n Frame,
) -> Result<NamesRecords<'n>, RunNamesError> {
    let RunCrates { list, crates: opened, rows, scripts, lang, corrected, packs, dirs, snapshot, from, heap } = crates;
    // The trees: shared command-wide mapping cache and command-scoped pack directories, so
    // multi-suite `blaze test` never leaks per-suite drivers/FDs or duplicate pack mappings.
    let trees = SharedTrees::new(packs, from.fs, dirs, seen);
    // Every crate's environment, each made at its first ask ([`RunEnvs`]): a body lowered by
    // need reads its own crate's, as `blaze check` gives a body its owner's.
    let envs: &'n RunEnvs<'n> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(RunEnvs::new(list, rows, scripts, snapshot, from.home, frame), frame));
    let envs = Envs { owner: None, rest: Some(envs) };
    let trees = (trees, None);
    let made = names_records_envs(cx, root, ScopeCrates::Opened(opened), trees, corrected, procs, std::vec::Vec::new_in(Global), envs, Some(snapshot), None, frame, heap.alloc()).map_err(|error| match error {
        NamesError::Items { at, why } => RunNamesError::Items { crate_name: opened.get(at).map_or("?", |one| CrateGraph::name(list, one.world_at)), why },
    })?;
    Ok(NamesRecords { records: made.records, frame, lang_crates: lang, body_sources: Some(made.sources) })
}
