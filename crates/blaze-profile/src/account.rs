//! One accounting per build: the change it saw, the work it did, and for each piece of work the
//! cause that made it necessary (docs/profiling.md, "`accounts`").
//!
//! The profile records a lot (spans, `query_counts`, `provenance`, `misses`), and until
//! 2026-11-12 nothing added it up: a no-op that wrote 53 inputs, 46 units made again in every
//! build of reth, and 820,026 queries rerun under "up to date" were each found by a lane that
//! measured a real project by hand. The account is the sum, made for every build, from the rows
//! the build already gives the history:
//!
//! - **The change** ([`Writer`]): the engine's input writes (`query_counts.written`), by who
//!   writes that kind of input.
//! - **The work**: the engine's rows run, by cause, and verified; and each step computed again
//!   (a unit, a unit's record that was not kept, a build script's run, a function's code), by
//!   where its chain of reasons ends ([`End`]).
//! - **The sums** ([`Account::unbalanced`]): each stage's causes add up to its total. A row run
//!   or a step made that no cause counts is [`AccountRow::ran_uncaused`] or
//!   [`AccountRow::by_unexplained`]: "unexplained" is a number that must be zero, and one that
//!   is not is a bug in the code that started the work.
//! - **Proportion** ([`Account::alarms`]): the work set against the size of the change, with
//!   thresholds that name a warning.
//!
//! [`Ledger`] is the same accounting for code that starts work: [`Ledger::did`] takes the
//! [`Cause`] as an argument, so a site cannot start work and leave out why, and
//! [`Ledger::close`] follows every cause to a change or a standing reason.
//!
//! Nothing here reads a clock, a file or a lock: an account is a function of rows.

use std::{collections::BTreeSet, fmt::Write as _};

use arrow_array::{ArrayRef, RecordBatch, StringArray, UInt64Array};
use arrow_schema::ArrowError;

use crate::{batches, facts::QueryCountRow, schema};

/// Who writes an input of the query engine, told by the input's kind.
///
/// The engine counts a write per input kind (`query_counts.written`) and no write says who
/// made it, so the kind is what the account has. A kind no row of [`Writer::of_input`] names is
/// [`Writer::Unattributed`], which is counted, shown and held at zero by a test over the
/// tree's `#[blaze_query::input]`s: a new input says who writes it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Writer {
    /// A file's bytes (`FileText`, `IncludedFile`): a person's edit, a build script's output or
    /// a file blaze itself wrote; the path of each is in the step's reason ([`Origin`]).
    Source,
    /// The process environment as the build was asked with (`ProcessEnv`).
    Environment,
    /// The build's plan: the crate graph, the source roots, the settings, the proc macros, the
    /// front end's code identity.
    Plan,
    /// What blaze publishes into the engine from its own results (resolutions, impl slices,
    /// frozen maps, kept constants): a write of blaze's own, never a change a person made.
    Derived,
    /// An input kind this table does not name.
    Unattributed,
}

impl Writer {
    pub const ALL: [Writer; 5] = [
        Writer::Source,
        Writer::Environment,
        Writer::Plan,
        Writer::Derived,
        Writer::Unattributed,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Writer::Source => "source",
            Writer::Environment => "environment",
            Writer::Plan => "plan",
            Writer::Derived => "derived",
            Writer::Unattributed => "unattributed",
        }
    }

    /// Who writes the input `kind`, as `query_counts.query` names it (the input's type).
    pub fn of_input(kind: &str) -> Writer {
        // A name as the engine gives it may carry its path (`base_db::FileText`).
        let kind = kind.rsplit("::").next().unwrap_or(kind);
        match kind {
            "FileText" | "IncludedFile" => Writer::Source,
            "ProcessEnv" => Writer::Environment,
            "AllCrates" | "Crate" | "PrimaryCrates" | "LibraryRoots" | "LocalRoots"
            | "SourceRootInput" | "FileSourceRootInput" | "FrontSettings" | "NamedPaths"
            | "PathBucket" | "FrontEnd" => Writer::Plan,
            // Where the database keeps its entries and macro definitions: set once by the
            // driver as it opens the store, as a plan's setting is.
            "EntryStore" | "KeptDefs" => Writer::Plan,
            "BuiltinDerives" | "CandidateSlice" | "CrateResolution" | "DepImages"
            | "DirectorySlot" | "FieldOrder" | "FrozenDefMaps" | "FrozenImpls"
            | "FrozenLangItems" | "ImplIdentities" | "InherentSlice" | "KeptConsts"
            | "PagedDefMaps" | "FrozenKept" | "ScopeInput" | "ServedBodies"
            | "SliceDirectory" | "SourceNames" | "TraitImplsSlice" => Writer::Derived,
            // `CrateSlot` (base-db: one input a thing given to one crate) is written by a
            // plan and by blaze's own publications alike: its writes are attributed by the
            // slot's kind ([`Writer::of_slot`], [`Seen::slots`]), never by this name.
            _ => Writer::Unattributed,
        }
    }

    /// Who writes a crate slot of `kind`: the type name of `base_db::Crate::give`'s `K`, as
    /// `base_db::take_given` says it (its path and its generic arguments are left out).
    pub fn of_slot(kind: &str) -> Writer {
        let kind = kind.split('<').next().unwrap_or(kind);
        let kind = kind.rsplit("::").next().unwrap_or(kind);
        match kind {
            // The proc macros a plan loaded for the crate, and whether the plan asks for it.
            "ProcMacros" | "PrimaryFlag" => Writer::Plan,
            // What blaze publishes of the crate from its own results, and the slot a served or
            // withdrawn dependency's entry has beside each of its maps.
            "CrateResolution" | "DirectorySlot" | "ImageSlot" | "DefMapSlot" | "LangItemsSlot"
            | "PagesSlot" | "ImplTablesSlot" | "SurfaceSlot" | "RuntimeSlot"
            | "KeptSlot" => Writer::Derived,
            _ => Writer::Unattributed,
        }
    }
}

/// The engine's name of the input every crate slot is ([`Writer::of_slot`]).
pub const CRATE_SLOT: &str = "CrateSlot";

/// A build's writes of crate slots, by who writes that kind of slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Slots {
    pub plan: u64,
    pub derived: u64,
    /// Writes of a kind [`Writer::of_slot`] does not name.
    pub unattributed: u64,
}

impl Slots {
    /// The writes `given` lists, each a kind with its writes (`base_db::take_given`).
    pub fn of<'a>(given: impl IntoIterator<Item = (&'a str, u64)>) -> Slots {
        let mut slots = Slots::default();
        for (kind, writes) in given {
            match Writer::of_slot(kind) {
                Writer::Plan => slots.plan += writes,
                Writer::Derived => slots.derived += writes,
                Writer::Source | Writer::Environment | Writer::Unattributed => {
                    slots.unattributed += writes;
                }
            }
        }
        slots
    }
}

/// Where a changed file is, which says who could have written it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Origin {
    /// A file of the workspace outside its build directory: a person's edit (or a tool's on
    /// their behalf).
    Workspace,
    /// A file under a build directory (`target/`, an `OUT_DIR`): a build script's output or
    /// another step's.
    Built,
    /// A lock file (`Cargo.lock`, `blaze.lock`): written by blaze or cargo.
    Lock,
    /// A file outside the workspace: a dependency's source, the toolchain's.
    Outside,
    /// No file: a variable, a flag, a setting, the toolchain.
    Input,
}

impl Origin {
    pub const fn name(self) -> &'static str {
        match self {
            Origin::Workspace => "a workspace file",
            Origin::Built => "a built file",
            Origin::Lock => "a lock file",
            Origin::Outside => "a file outside the workspace",
            Origin::Input => "an input that is no file",
        }
    }

    /// Where the file at the absolute `path` is, for a build of `workspace`.
    pub fn of_file(path: &str, workspace: &str) -> Origin {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        if name == "Cargo.lock" || name == "blaze.lock" {
            return Origin::Lock;
        }
        let workspace = workspace.trim_end_matches(['/', '\\']);
        let inside = !workspace.is_empty()
            && path.strip_prefix(workspace).is_some_and(|rest| rest.starts_with(['/', '\\']));
        let built = path.split(['/', '\\']).any(|part| part == "target" || part == "out");
        match (inside, built) {
            (_, true) => Origin::Built,
            (true, false) => Origin::Workspace,
            (false, false) => Origin::Outside,
        }
    }
}

/// Where a step's chain of reasons ends, as the account counts it: `blaze why`'s closed set
/// (`blaze_api::Reason`), a changed input with where it is.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum End {
    /// An input of the build changed.
    Changed { origin: Origin, input: String },
    /// Nothing was ever kept of it.
    First,
    /// Its result or its record was dropped since it was last computed.
    Evicted,
    /// The build that last computed it failed or was stopped, and kept nothing.
    Left,
    /// It was computed because that was asked for.
    Forced,
    /// The same inputs gave another key: a determinism bug.
    Unstable,
    /// No recorded reason.
    Unexplained,
}

impl End {
    /// The order a step with several ends is counted by: the first of these it has. A bug
    /// first, so no change hides one; then the changes, a person's before blaze's own; then
    /// the standing reasons.
    const fn rank(&self) -> u8 {
        match self {
            End::Unexplained => 0,
            End::Unstable => 1,
            End::Changed { origin: Origin::Workspace, .. } => 2,
            End::Changed { origin: Origin::Input, .. } => 3,
            End::Changed { origin: Origin::Outside, .. } => 4,
            End::Changed { origin: Origin::Lock, .. } => 5,
            End::Changed { origin: Origin::Built, .. } => 6,
            End::Left => 7,
            End::Evicted => 8,
            End::Forced => 9,
            End::First => 10,
        }
    }
}

/// One step a build computed again, as the account takes it: `misses.step`, and the ends of
/// its chain (`blaze why`'s `Rebuild::ends`). No end at all is counted unexplained.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Step {
    pub step: String,
    pub ends: Vec<End>,
}

/// The `misses.step` of a unit whose record was looked up and not found.
pub const STEP_UNIT: &str = "unit";
/// The `misses.step` of a unit made whose record was not kept.
pub const STEP_UNKEPT: &str = "unit record";
/// The `misses.step` of a build script's run.
pub const STEP_SCRIPT: &str = "build script run";
/// What the `misses.step` of a function's code begins with.
pub const STEP_CODE: &str = "codegen";

/// A row of the schema's `accounts` table: one build's account.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct AccountRow {
    pub build_id: u64,
    // The change.
    pub written: u64,
    pub written_source: u64,
    pub written_environment: u64,
    pub written_plan: u64,
    pub written_derived: u64,
    pub written_unattributed: u64,
    // The engine's work.
    pub executed: u64,
    pub verified: u64,
    pub ran_new: u64,
    pub ran_changed: u64,
    pub ran_untracked: u64,
    pub ran_cycle: u64,
    pub ran_evicted: u64,
    /// Runs no cause counts: over the query functions, `executed` less the causes where that
    /// is more than nothing.
    pub ran_uncaused: u64,
    /// The sums that do not hold ([`Account::unbalanced`]): how many.
    pub unbalanced: u64,
    // The steps.
    pub steps: u64,
    pub units: u64,
    pub unkept: u64,
    pub scripts: u64,
    pub functions: u64,
    pub other_steps: u64,
    pub links: u64,
    // The steps, by where their chain ends: each step once.
    pub by_workspace_file: u64,
    pub by_built_file: u64,
    pub by_lock_file: u64,
    pub by_outside_file: u64,
    pub by_input: u64,
    pub by_first: u64,
    pub by_evicted: u64,
    pub by_left: u64,
    pub by_forced: u64,
    pub by_unstable: u64,
    pub by_unexplained: u64,
    /// The distinct inputs the steps' chains name as changed.
    pub changes: u64,
    /// The first of them, by name: what a warning names.
    pub first_change: Option<String>,
    // What the profile lost by this build.
    /// Rows the history dropped since the daemon began, of every table, by this build's end.
    pub history_dropped: u64,
    /// The alarms, by name, joined with `, `; none when the build's work fits its change.
    pub alarms: Option<String>,
}

/// One sum of an account that does not hold: a bug in the code that counts, or in the code
/// that did the work and gave no cause.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Unbalanced {
    /// What is summed: `rows run of <query>`, `inputs written`, `steps by end`, `steps by kind`.
    pub what: String,
    pub total: u64,
    pub by_cause: u64,
}

/// What a build's account warns of, each with the numbers it rests on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Alarm {
    /// Work with no recorded cause: steps `blaze why` cannot explain, rows no cause counts.
    Unexplained { steps: u64, rows: u64 },
    /// A sum that does not hold.
    Unbalanced { sums: u64 },
    /// Writes of input kinds no [`Writer`] names.
    Unattributed { writes: u64 },
    /// Nothing changed, and work was done all the same.
    WorkWithoutChange { rows: u64, steps: u64 },
    /// Inputs were written and no row ran for a changed read: the writes were of equal values.
    NeedlessWrites { writes: u64 },
    /// More steps were made again than the change's size bounds.
    Disproportionate { steps: u64, changes: u64, input: Option<String> },
    /// More rows were run or verified than the writes bound.
    RowsPerWrite { rows: u64, writes: u64 },
    /// Units were made whose record was not kept: the next build makes them again.
    Unkept { units: u64 },
}

/// The most rows a build of no change may run for a reason other than a first run before
/// [`Alarm::WorkWithoutChange`]: what a handful of untracked reads cost.
pub const QUIET_ROWS: u64 = 64;
/// The most steps made again for each changed input before [`Alarm::Disproportionate`].
pub const STEPS_PER_CHANGE: u64 = 16;
/// The most rows run for a change or verified for each input written before
/// [`Alarm::RowsPerWrite`].
pub const ROWS_PER_WRITE: u64 = 10_000;

impl Alarm {
    /// Its name: what `accounts.alarms` lists and a `known` row of a harness names.
    pub const fn name(&self) -> &'static str {
        match self {
            Alarm::Unexplained { .. } => "unexplained",
            Alarm::Unbalanced { .. } => "unbalanced",
            Alarm::Unattributed { .. } => "unattributed",
            Alarm::WorkWithoutChange { .. } => "work-without-change",
            Alarm::NeedlessWrites { .. } => "needless-writes",
            Alarm::Disproportionate { .. } => "disproportionate",
            Alarm::RowsPerWrite { .. } => "rows-per-write",
            Alarm::Unkept { .. } => "unkept",
        }
    }

    /// The warning, as a person reads it: what happened, in numbers, and the command that
    /// lists it.
    pub fn text(&self, build: u64) -> String {
        // alloc: a warning's text, once an alarm
        match self {
            Alarm::Unexplained { steps, rows } => format!(
                "build {build} did work with no recorded cause, which is a bug in blaze: {steps} steps (`blaze why --unexplained`), {rows} rows run that no cause counts (`blaze explain counts_by_cause build={build}`)"
            ),
            Alarm::Unbalanced { sums } => format!(
                "build {build}'s account does not add up in {sums} sums, which is a bug in blaze: `blaze explain account build={build}`"
            ),
            Alarm::Unattributed { writes } => format!(
                "build {build} wrote {writes} inputs of a kind the account cannot attribute (`blaze explain writes build={build}`): name its writer in blaze-profile's `account::Writer::of_input`"
            ),
            Alarm::WorkWithoutChange { rows, steps } => format!(
                "build {build} saw no change and still ran {rows} rows and made {steps} steps again: `blaze why`, `blaze explain untracked_reads build={build}`"
            ),
            Alarm::NeedlessWrites { writes } => format!(
                "build {build} wrote {writes} inputs and no row ran for a changed read, so the values written were the ones held: `blaze explain writes build={build}`"
            ),
            Alarm::Disproportionate { steps, changes, input } => format!(
                "this build redid {steps} steps for a change to {changes} input{}{}: `blaze why`",
                if *changes == 1 { "" } else { "s" },
                input.as_deref().map(|it| format!(" ({it})")).unwrap_or_default(),
            ),
            Alarm::RowsPerWrite { rows, writes } => format!(
                "build {build} ran or verified {rows} rows for {writes} inputs written: `blaze explain why_reran build={build}`"
            ),
            Alarm::Unkept { units } => format!(
                "build {build} made {units} units whose record was not kept, so the next build makes them again whatever changes: `blaze why --all`"
            ),
        }
    }
}

/// A build's account, made from its rows.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Account {
    pub row: AccountRow,
    /// The sums that do not hold.
    pub unbalanced: Vec<Unbalanced>,
}

/// What a build gives its account besides its `query_counts` and its steps.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Seen {
    /// Images linked (`links` rows).
    pub links: u64,
    /// The build's writes of crate slots by kind: what its `CrateSlot` writes are attributed
    /// by. Writes the engine counted of `CrateSlot` past these are unattributed.
    pub slots: Slots,
    /// Rows the history dropped so far ([`crate::supervise::Dropped::total`]).
    pub history_dropped: u64,
}

impl Account {
    /// The account of build `build_id` from what the engine counted (`counts`, its own rows
    /// alone), the steps it computed again with their ends (`steps`), and `seen`.
    pub fn of(build_id: u64, counts: &[QueryCountRow], steps: &[Step], seen: Seen) -> Account {
        let mut row = AccountRow {
            build_id,
            links: seen.links,
            history_dropped: seen.history_dropped,
            ..AccountRow::default()
        };
        // alloc: once a build, the sums that fail (none, unless blaze has a bug)
        let mut unbalanced = Vec::new();
        for it in counts {
            row.written += it.written;
            if it.query.rsplit("::").next() == Some(CRATE_SLOT) {
                // By the slot's kind: the plan's first, then blaze's own, and what the kinds
                // counted do not cover (a kind with no writer named, or a write the kinds'
                // count missed) is unattributed.
                let plan = seen.slots.plan.min(it.written);
                let derived = seen.slots.derived.min(it.written - plan);
                row.written_plan += plan;
                row.written_derived += derived;
                row.written_unattributed += it.written - plan - derived;
            } else {
                match Writer::of_input(it.query) {
                    Writer::Source => row.written_source += it.written,
                    Writer::Environment => row.written_environment += it.written,
                    Writer::Plan => row.written_plan += it.written,
                    Writer::Derived => row.written_derived += it.written,
                    Writer::Unattributed => row.written_unattributed += it.written,
                }
            }
            row.executed += it.executed;
            row.verified += it.verified;
            row.ran_new += it.ran_new;
            row.ran_changed += it.ran_changed;
            row.ran_untracked += it.ran_untracked;
            row.ran_cycle += it.ran_cycle;
            row.ran_evicted += it.evicted_asked;
            // The sum the engine promises (`blaze_query::db::Counts`): its causes add up to
            // its runs, a query function at a time.
            let by_cause =
                it.ran_new + it.ran_changed + it.ran_untracked + it.ran_cycle + it.evicted_asked;
            if by_cause != it.executed {
                row.ran_uncaused += it.executed.saturating_sub(by_cause);
                // alloc: a sum that failed
                unbalanced.push(Unbalanced {
                    what: format!("rows run of `{}`", it.query),
                    total: it.executed,
                    by_cause,
                });
            }
        }
        // alloc: once a build, the changed inputs its steps name
        let mut changed: BTreeSet<&str> = BTreeSet::new();
        for step in steps {
            row.steps += 1;
            match step.step.as_str() {
                STEP_UNIT => row.units += 1,
                STEP_UNKEPT => row.unkept += 1,
                STEP_SCRIPT => row.scripts += 1,
                it if it.starts_with(STEP_CODE) => row.functions += 1,
                _ => row.other_steps += 1,
            }
            for end in &step.ends {
                if let End::Changed { input, .. } = end {
                    changed.insert(input.as_str());
                }
            }
            // Each step once, by the first of its ends in `End::rank`'s order; none is a bug.
            match step.ends.iter().min_by_key(|it| it.rank()) {
                None | Some(End::Unexplained) => row.by_unexplained += 1,
                Some(End::Unstable) => row.by_unstable += 1,
                Some(End::Changed { origin, .. }) => match origin {
                    Origin::Workspace => row.by_workspace_file += 1,
                    Origin::Built => row.by_built_file += 1,
                    Origin::Lock => row.by_lock_file += 1,
                    Origin::Outside => row.by_outside_file += 1,
                    Origin::Input => row.by_input += 1,
                },
                Some(End::Left) => row.by_left += 1,
                Some(End::Evicted) => row.by_evicted += 1,
                Some(End::Forced) => row.by_forced += 1,
                Some(End::First) => row.by_first += 1,
            }
        }
        row.changes = changed.len() as u64;
        row.first_change = changed.first().map(|it| (*it).to_owned()); // alloc: once a build
        let mut account = Account { row, unbalanced };
        let engine = account.unbalanced.len();
        account.sums();
        // The row's own sums hold by how it is counted above: one that fails is a bug here.
        // The engine's (a query function's causes against its runs) are its counters', read
        // while its threads count, and are reported, never asserted.
        debug_assert_eq!(
            account.unbalanced.len(),
            engine,
            "a build's account does not add up: {:?}",
            &account.unbalanced[engine..]
        );
        account.row.unbalanced = account.unbalanced.len() as u64;
        let alarms = account.alarms();
        if !alarms.is_empty() {
            // alloc: once a build that alarms
            let names: Vec<&str> = alarms.iter().map(Alarm::name).collect();
            account.row.alarms = Some(names.join(", "));
        }
        account
    }

    /// The sums of the row's own columns, each held: the writers add up to the writes, the
    /// ends to the steps, the kinds to the steps. One that fails joins [`Account::unbalanced`].
    fn sums(&mut self) {
        let row = &self.row;
        let sums = [
            (
                "inputs written",
                row.written,
                row.written_source
                    + row.written_environment
                    + row.written_plan
                    + row.written_derived
                    + row.written_unattributed,
            ),
            ("steps by end", row.steps, row.caused() + row.standing() + row.bugs()),
            (
                "steps by kind",
                row.steps,
                row.units + row.unkept + row.scripts + row.functions + row.other_steps,
            ),
        ];
        for (what, total, by_cause) in sums {
            if total != by_cause {
                // alloc: a sum that failed
                self.unbalanced.push(Unbalanced { what: what.to_owned(), total, by_cause });
            }
        }
    }

    /// What the account warns of: none for a build whose work is explained and fits its change.
    pub fn alarms(&self) -> Vec<Alarm> {
        let row = &self.row;
        let mut alarms = Vec::new(); // alloc: once a build, usually empty
        if row.by_unexplained + row.by_unstable + row.ran_uncaused > 0 {
            alarms.push(Alarm::Unexplained {
                steps: row.by_unexplained + row.by_unstable,
                rows: row.ran_uncaused,
            });
        }
        // The row's count where the sums themselves were not kept (a row read back).
        let sums = row.unbalanced.max(self.unbalanced.len() as u64);
        if sums > 0 {
            alarms.push(Alarm::Unbalanced { sums });
        }
        if row.written_unattributed > 0 {
            alarms.push(Alarm::Unattributed { writes: row.written_unattributed });
        }
        // A change is what came from outside: a derived write is blaze's own.
        let outside = row.written_source + row.written_environment + row.written_plan;
        let reran = row.ran_changed + row.ran_untracked + row.ran_cycle + row.ran_evicted;
        let redone = row.steps - row.by_first.min(row.steps);
        if outside == 0 && row.changes == 0 && (reran > QUIET_ROWS || row.caused() + row.bugs() > 0)
        {
            alarms.push(Alarm::WorkWithoutChange { rows: reran, steps: redone });
        }
        if row.written > 0 && row.ran_changed == 0 && row.changes == 0 && row.ran_new == 0 {
            alarms.push(Alarm::NeedlessWrites { writes: row.written });
        }
        if row.caused() > STEPS_PER_CHANGE.saturating_mul(row.changes.max(1)) {
            alarms.push(Alarm::Disproportionate {
                steps: row.caused(),
                changes: row.changes,
                input: row.first_change.clone(), // clone: a warning's text, once an alarm
            });
        }
        let touched = row.ran_changed + row.verified;
        if outside > 0 && touched > ROWS_PER_WRITE.saturating_mul(outside) {
            alarms.push(Alarm::RowsPerWrite { rows: touched, writes: outside });
        }
        if row.unkept > 0 {
            alarms.push(Alarm::Unkept { units: row.unkept });
        }
        alarms
    }

    /// The account in one line, for `blaze slow` and the build's end.
    pub fn line(&self) -> String {
        let row = &self.row;
        let mut out = String::new(); // alloc: a line, once a build
        _ = write!(
            out,
            "{} inputs written ({} source, {} environment, {} plan, {} derived, {} unattributed); \
             {} rows run ({} first, {} changed, {} untracked, {} cycle, {} evicted, {} uncaused), {} verified; \
             {} steps ({} units, {} unkept, {} scripts, {} functions) for {} changed inputs; {} unexplained",
            row.written,
            row.written_source,
            row.written_environment,
            row.written_plan,
            row.written_derived,
            row.written_unattributed,
            row.executed,
            row.ran_new,
            row.ran_changed,
            row.ran_untracked,
            row.ran_cycle,
            row.ran_evicted,
            row.ran_uncaused,
            row.verified,
            row.steps,
            row.units,
            row.unkept,
            row.scripts,
            row.functions,
            row.changes,
            row.by_unexplained + row.by_unstable + row.ran_uncaused,
        );
        out
    }
}

impl AccountRow {
    /// The account of a row read back from the history: its sums' details are not kept, their
    /// count is ([`AccountRow::unbalanced`]).
    pub fn account(self) -> Account {
        Account { row: self, unbalanced: Vec::new() } // alloc: empty, allocates nothing
    }

    /// The steps whose chain ends in a change.
    pub fn caused(&self) -> u64 {
        self.by_workspace_file
            + self.by_built_file
            + self.by_lock_file
            + self.by_outside_file
            + self.by_input
    }

    /// The steps whose chain ends in a standing reason.
    pub fn standing(&self) -> u64 {
        self.by_first + self.by_evicted + self.by_left + self.by_forced
    }

    /// The steps whose chain ends in a bug of blaze's.
    pub fn bugs(&self) -> u64 {
        self.by_unstable + self.by_unexplained
    }
}

/// The `accounts` rows of `rows`.
pub fn accounts(rows: &[AccountRow]) -> Result<RecordBatch, ArrowError> {
    let count = |of: fn(&AccountRow) -> u64| -> ArrayRef {
        // alloc: a batch's column, once a flush
        std::sync::Arc::new(UInt64Array::from_iter_values(rows.iter().map(of)))
    };
    let text = |of: fn(&AccountRow) -> Option<&str>| -> ArrayRef {
        // alloc: a batch's column, once a flush
        std::sync::Arc::new(StringArray::from_iter(rows.iter().map(of)))
    };
    // alloc: a batch's columns, once a flush
    let columns: Vec<ArrayRef> = vec![
        count(|it| it.build_id),
        count(|it| it.written),
        count(|it| it.written_source),
        count(|it| it.written_environment),
        count(|it| it.written_plan),
        count(|it| it.written_derived),
        count(|it| it.written_unattributed),
        count(|it| it.executed),
        count(|it| it.verified),
        count(|it| it.ran_new),
        count(|it| it.ran_changed),
        count(|it| it.ran_untracked),
        count(|it| it.ran_cycle),
        count(|it| it.ran_evicted),
        count(|it| it.ran_uncaused),
        count(|it| it.unbalanced),
        count(|it| it.steps),
        count(|it| it.units),
        count(|it| it.unkept),
        count(|it| it.scripts),
        count(|it| it.functions),
        count(|it| it.other_steps),
        count(|it| it.links),
        count(|it| it.by_workspace_file),
        count(|it| it.by_built_file),
        count(|it| it.by_lock_file),
        count(|it| it.by_outside_file),
        count(|it| it.by_input),
        count(|it| it.by_first),
        count(|it| it.by_evicted),
        count(|it| it.by_left),
        count(|it| it.by_forced),
        count(|it| it.by_unstable),
        count(|it| it.by_unexplained),
        count(|it| it.changes),
        text(|it| it.first_change.as_deref()),
        count(|it| it.history_dropped),
        text(|it| it.alarms.as_deref()),
    ];
    RecordBatch::try_new(batches::schema_of(&schema::ACCOUNTS), columns)
}

// --- The ledger: causes as a required argument ---------------------------------------------

/// A change a build saw, in a [`Ledger`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct ChangeId(u32);

/// A piece of work a build did, in a [`Ledger`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct WorkId(u32);

/// A reason for work that is no change of this build.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Standing {
    /// Nothing was ever kept of it.
    First,
    /// Its value was let go for memory by the give-back named.
    Evicted { give_back: String },
    /// Its value was made before and not kept: why.
    NotKept { why: String },
    /// The build that last made it kept nothing.
    Left { since: u64 },
    /// It was asked for: how.
    Forced { how: String },
}

/// Why a piece of work was done: what [`Ledger::did`] takes, so no site can start work and
/// leave it out.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Cause {
    /// A change of this build.
    Change(ChangeId),
    /// Other work of this build, whose result this work reads: its cause is this one's.
    Work(WorkId),
    /// A reason that stands whatever the build changed.
    Standing(Standing),
}

/// Where a piece of work's chain of causes ends ([`Ledger::close`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Root {
    Change(ChangeId),
    Standing(Standing),
    /// The chain ends nowhere: it names work the ledger does not hold, or work that names it
    /// back. Unexplained work, a bug in the site that gave the cause.
    Nowhere { why: String },
}

/// One stage's sum: what the stage says it did in all against what its causes count.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StageSum {
    pub stage: &'static str,
    pub total: u64,
    pub caused: u64,
}

/// A closed [`Ledger`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Closed {
    /// Each piece of work with its root, in the order it was done.
    pub work: Vec<(WorkId, &'static str, String, Root)>,
    /// The work whose chain ends nowhere: must be none.
    pub unexplained: u64,
    /// The stages whose causes do not add up to the total they said ([`Ledger::total`]).
    pub unbalanced: Vec<StageSum>,
    pub changes: u64,
}

/// One build's changes and work, each piece of work with the cause its site gave.
///
/// The invariant it holds, checked by [`Ledger::close`]: every piece of work chains to a
/// change of this build or to a standing reason, and each stage's pieces add up to the total
/// the stage said.
#[derive(Clone, Debug, Default)]
pub struct Ledger {
    changes: Vec<(Writer, String)>,
    work: Vec<(&'static str, String, Cause)>,
    totals: Vec<(&'static str, u64)>,
}

impl Ledger {
    pub fn new() -> Ledger {
        Ledger::default()
    }

    /// A change the build saw: who wrote it and what it is.
    pub fn changed(&mut self, writer: Writer, what: impl Into<String>) -> ChangeId {
        self.changes.push((writer, what.into())); // alloc: once a change
        ChangeId(self.changes.len() as u32 - 1)
    }

    /// A piece of work `stage` did on `what`, for `cause`. There is no way to say work with no
    /// cause: a site that does not know why it runs has a bug to fix, not a default to take.
    pub fn did(&mut self, stage: &'static str, what: impl Into<String>, cause: Cause) -> WorkId {
        self.work.push((stage, what.into(), cause)); // alloc: once a piece of work
        WorkId(self.work.len() as u32 - 1)
    }

    /// What `stage` says it did in all, counted by the stage itself and not from its
    /// [`Ledger::did`] calls: the two are held equal by [`Ledger::close`].
    pub fn total(&mut self, stage: &'static str, total: u64) {
        match self.totals.iter_mut().find(|it| it.0 == stage) {
            Some(it) => it.1 += total,
            None => self.totals.push((stage, total)), // alloc: once a stage
        }
    }

    /// The ledger closed: each piece of work followed to its root, bounded by the work held
    /// (rule 18), and each stage's sum.
    pub fn close(&self) -> Closed {
        let mut work = Vec::with_capacity(self.work.len()); // alloc: once a build
        let mut unexplained = 0;
        for (at, (stage, what, _)) in self.work.iter().enumerate() {
            let root = self.root(at);
            if matches!(root, Root::Nowhere { .. }) {
                unexplained += 1;
            }
            work.push((WorkId(at as u32), *stage, what.clone(), root)); // clone: once a piece
        }
        let mut unbalanced = Vec::new(); // alloc: none unless a sum fails
        let mut stages: Vec<&'static str> = self.totals.iter().map(|it| it.0).collect(); // alloc: once a build
        for (stage, _, _) in &self.work {
            if !stages.contains(stage) {
                stages.push(*stage);
            }
        }
        for stage in stages {
            let caused = self.work.iter().filter(|it| it.0 == stage).count() as u64;
            // A stage that said no total says nothing to hold its pieces to.
            let Some(total) = self.totals.iter().find(|it| it.0 == stage).map(|it| it.1) else {
                continue;
            };
            if total != caused {
                unbalanced.push(StageSum { stage, total, caused });
            }
        }
        Closed { work, unexplained, unbalanced, changes: self.changes.len() as u64 }
    }

    /// The root of the work at `at`: its cause followed through the work it names, at most
    /// once through each piece.
    fn root(&self, at: usize) -> Root {
        let mut at = at;
        // Bounded: each step goes to work done earlier, or ends.
        for _ in 0..=self.work.len() {
            match &self.work[at].2 {
                // clone: a root, once a piece of work
                Cause::Standing(why) => return Root::Standing(why.clone()),
                Cause::Change(id) => {
                    return match (id.0 as usize) < self.changes.len() {
                        true => Root::Change(*id),
                        // alloc: an error
                        false => Root::Nowhere {
                            why: format!("it names change {}, which this build did not see", id.0),
                        },
                    };
                }
                Cause::Work(id) => {
                    let to = id.0 as usize;
                    if to >= at {
                        // alloc: an error
                        return Root::Nowhere {
                            why: format!(
                                "it names work {}, which was not done before it in this build",
                                id.0
                            ),
                        };
                    }
                    at = to;
                }
            }
        }
        unreachable!("each step of a chain goes to earlier work, so it ends within the work held")
    }

    /// The change `id` is: who wrote it and what.
    pub fn change(&self, id: ChangeId) -> Option<(Writer, &str)> {
        self.changes.get(id.0 as usize).map(|it| (it.0, it.1.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    fn counts(query: &'static str) -> QueryCountRow {
        QueryCountRow {
            build_id: 1,
            query,
            executed: 0,
            verified: 0,
            reran: 0,
            evicted_asked: 0,
            evicted: 0,
            dropped: 0,
            reads_recorded: 0,
            reads_kept: 0,
            reads_most: 0,
            written: 0,
            ran_new: 0,
            ran_changed: 0,
            ran_untracked: 0,
            ran_cycle: 0,
            validity: crate::facts::Validity::default(),
        }
    }

    fn step(step: &str, ends: Vec<End>) -> Step {
        Step { step: step.to_owned(), ends }
    }

    fn edit(path: &str) -> End {
        End::Changed { origin: Origin::of_file(path, "/w"), input: path.to_owned() }
    }

    #[test]
    fn a_body_edits_account_adds_up_and_alarms_nothing() {
        let rows = [
            QueryCountRow { written: 1, ..counts("FileText") },
            QueryCountRow { executed: 12, ran_changed: 12, verified: 340, ..counts("infer_query") },
        ];
        let steps = [
            step(STEP_UNIT, vec![edit("/w/src/lib.rs")]),
            step("codegen dev template", vec![edit("/w/src/lib.rs")]),
        ];
        let account = Account::of(7, &rows, &steps, Seen { links: 1, ..Seen::default() });
        assert_eq!(account.unbalanced, Vec::<Unbalanced>::new());
        assert_eq!(account.alarms(), Vec::<Alarm>::new());
        let row = &account.row;
        assert_eq!((row.written, row.written_source, row.executed, row.ran_changed), (1, 1, 12, 12));
        assert_eq!((row.steps, row.units, row.functions, row.by_workspace_file), (2, 1, 1, 2));
        assert_eq!((row.changes, row.first_change.as_deref()), (1, Some("/w/src/lib.rs")));
        assert_eq!(row.alarms, None);
        // The row is the table's: a column each, in the schema's order.
        let batch = accounts(std::slice::from_ref(row)).unwrap();
        assert_eq!(batch.num_columns(), schema::ACCOUNTS.columns.len());
        assert_eq!(batch.num_rows(), 1);
    }

    /// The control of the sum check: a run the engine counted under no cause is a sum that
    /// does not hold, a count of its own and an alarm.
    #[test]
    fn a_run_with_no_cause_is_uncaused_unbalanced_and_alarmed() {
        let rows = [QueryCountRow { executed: 5, ran_changed: 3, written: 1, ..counts("item_tree") }];
        let account = Account::of(2, &rows, &[], Seen::default());
        assert_eq!(account.row.ran_uncaused, 2);
        assert_eq!(
            account.unbalanced,
            [Unbalanced { what: "rows run of `item_tree`".to_owned(), total: 5, by_cause: 3 }]
        );
        let names: Vec<&str> = account.alarms().iter().map(Alarm::name).collect();
        assert!(names.contains(&"unexplained") && names.contains(&"unbalanced"), "{names:?}");
        assert_eq!(account.row.unbalanced, 1);
    }

    #[test]
    fn a_step_with_an_unexplained_end_is_counted_unexplained_whatever_else_it_names() {
        let steps = [
            step(STEP_UNIT, vec![edit("/w/src/lib.rs"), End::Unexplained]),
            step(STEP_UNIT, Vec::new()),
            step(STEP_SCRIPT, vec![End::Forced]),
            step(STEP_UNIT, vec![End::First]),
        ];
        let account = Account::of(3, &[], &steps, Seen::default());
        let row = &account.row;
        assert_eq!((row.by_unexplained, row.by_forced, row.by_first, row.by_workspace_file), (2, 1, 1, 0));
        assert_eq!(account.unbalanced, Vec::<Unbalanced>::new());
        assert_eq!(account.alarms()[0], Alarm::Unexplained { steps: 2, rows: 0 });
    }

    /// The proportionality warning: a build that wrote inputs it did not need to (the no-op
    /// that wrote 53, 2026-11-12) is named, with the writes; the same build with no write is
    /// quiet (the control).
    #[test]
    fn a_build_that_rewrites_inputs_nothing_needed_is_warned_of() {
        let wrote = [QueryCountRow { written: 53, ..counts("ScopeInput") }];
        let account = Account::of(4, &wrote, &[], Seen::default());
        assert_eq!(account.alarms(), [Alarm::NeedlessWrites { writes: 53 }]);
        assert_eq!(account.row.alarms.as_deref(), Some("needless-writes"));
        assert!(account.alarms()[0].text(4).contains("wrote 53 inputs"));
        let quiet = Account::of(4, &[counts("ScopeInput")], &[], Seen::default());
        assert_eq!(quiet.alarms(), Vec::<Alarm>::new());
    }

    #[test]
    fn work_with_no_change_and_work_past_the_changes_size_are_warned_of() {
        // 46 units made again with nothing written and a reason that is no change.
        let steps: Vec<Step> = (0..46).map(|_| step(STEP_UNIT, vec![End::Unexplained])).collect();
        let reran = [QueryCountRow { executed: 820_026, ran_untracked: 820_026, ..counts("identity_of") }];
        let account = Account::of(5, &reran, &steps, Seen::default());
        let names: Vec<&str> = account.alarms().iter().map(Alarm::name).collect();
        assert_eq!(names, ["unexplained", "work-without-change"]);
        // One file changed and 40 units followed: past 16 a change.
        let steps: Vec<Step> = (0..40).map(|_| step(STEP_UNIT, vec![edit("/w/a.rs")])).collect();
        let wrote = [QueryCountRow { written: 1, executed: 9, ran_changed: 9, ..counts("FileText") }];
        let account = Account::of(6, &wrote, &steps, Seen::default());
        let alarms = account.alarms();
        assert_eq!(
            alarms,
            [Alarm::Disproportionate { steps: 40, changes: 1, input: Some("/w/a.rs".to_owned()) }]
        );
        assert_eq!(
            alarms[0].text(6),
            "this build redid 40 steps for a change to 1 input (/w/a.rs): `blaze why`"
        );
        // 16 is within the bound.
        let steps: Vec<Step> = (0..16).map(|_| step(STEP_UNIT, vec![edit("/w/a.rs")])).collect();
        assert_eq!(Account::of(6, &wrote, &steps, Seen::default()).alarms(), Vec::<Alarm>::new());
    }

    #[test]
    fn an_unkept_unit_and_an_unattributed_write_are_each_an_alarm() {
        let rows = [QueryCountRow { written: 2, executed: 1, ran_new: 1, ..counts("NewInputNobodyNamed") }];
        let steps = [step(STEP_UNKEPT, vec![End::Forced])];
        let account = Account::of(8, &rows, &steps, Seen::default());
        let names: Vec<&str> = account.alarms().iter().map(Alarm::name).collect();
        assert_eq!(names, ["unattributed", "unkept"]);
    }

    /// A crate slot's writes are attributed by the slot's kind, not by the input's name: the
    /// plan's proc macros to the plan, blaze's publications to blaze, and a write no kind
    /// counted (the control) to nobody, which alarms.
    #[test]
    fn a_crate_slots_writes_are_attributed_by_its_kind() {
        let wrote = [QueryCountRow { written: 5, executed: 5, ran_new: 5, ..counts("base_db::CrateSlot") }];
        let slots = Slots::of([
            ("hir_expand::proc_macro::ProcMacros", 2),
            ("hir_def::nameres::frozen::DefMapSlot<'static>", 1),
            ("hir_def::nameres::delta::publish::CrateResolution", 1),
        ]);
        assert_eq!(slots, Slots { plan: 2, derived: 2, unattributed: 0 });
        let account = Account::of(9, &wrote, &[], Seen { slots, ..Seen::default() });
        let row = &account.row;
        assert_eq!((row.written_plan, row.written_derived, row.written_unattributed), (2, 2, 1));
        assert_eq!(account.unbalanced, Vec::<Unbalanced>::new());
        assert_eq!(account.alarms(), [Alarm::Unattributed { writes: 1 }]);
        let whole = Slots::of([("ProcMacros", 2), ("DirectorySlot", 3), ("SomeNewKind", 4)]);
        assert_eq!(whole, Slots { plan: 2, derived: 3, unattributed: 4 });
    }

    #[test]
    fn a_file_is_placed_by_its_path() {
        assert_eq!(Origin::of_file("/w/src/lib.rs", "/w"), Origin::Workspace);
        assert_eq!(Origin::of_file("/w/src/lib.rs", "/w/"), Origin::Workspace);
        assert_eq!(Origin::of_file("/w/target/debug/build/x/out/gen.rs", "/w"), Origin::Built);
        assert_eq!(Origin::of_file("/w/Cargo.lock", "/w"), Origin::Lock);
        assert_eq!(Origin::of_file("/home/u/.cargo/registry/src/serde/lib.rs", "/w"), Origin::Outside);
        assert_eq!(Origin::of_file("/w2/src/lib.rs", "/w"), Origin::Outside);
    }

    /// The guard: every input of the query engine the tree defines has a writer named, so a
    /// build's writes are attributed whole. A new `#[blaze_query::input]` fails here until
    /// [`Writer::of_input`] names who writes it.
    #[test]
    fn every_input_of_the_tree_has_a_writer() {
        fn walk(dir: &std::path::Path, into: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if path.is_dir() {
                    if !["target", "tests", "benches", "blaze-query", "blaze-query-macros"].contains(&name.as_str()) {
                        walk(&path, into);
                    }
                } else if name.ends_with(".rs") && name != "tests.rs" {
                    into.push(path);
                }
            }
        }
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut files = Vec::new();
        walk(&crates, &mut files);
        let mut unnamed = Vec::new();
        let mut inputs = 0;
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else { continue };
            let lines: Vec<&str> = text.lines().collect();
            for (at, line) in lines.iter().enumerate() {
                if !line.trim_start().starts_with("#[blaze_query::input") {
                    continue;
                }
                let Some(name) = lines[at + 1..].iter().take(8).find_map(|it| {
                    let rest = it.split("struct ").nth(1)?;
                    Some(rest.split(|c: char| !c.is_alphanumeric() && c != '_').next()?.to_owned())
                }) else {
                    continue;
                };
                inputs += 1;
                // A crate slot's writer is its kind's (`Writer::of_slot`), held below.
                if name != CRATE_SLOT && Writer::of_input(&name) == Writer::Unattributed {
                    unnamed.push(format!("{name} ({})", file.display()));
                }
            }
            // Every kind a crate is given (`Crate::give::<K>`) has a writer too.
            for (at, _) in text.match_indices(".give::<") {
                let kind: String = text[at + ".give::<".len()..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
                    .collect();
                // A generic giver's `K`, and this test's own text, name no kind.
                if !kind.is_empty() && kind != "K" && Writer::of_slot(&kind) == Writer::Unattributed {
                    unnamed.push(format!("the crate slot kind {kind} ({})", file.display()));
                }
            }
        }
        assert!(inputs >= 20, "the walk found {inputs} inputs: it no longer reads the tree's");
        assert_eq!(unnamed, Vec::<String>::new(), "inputs with no writer in `Writer::of_input`");
    }

    // --- The simulated build sequence -------------------------------------------------------

    /// The reference model: a project of files and units, each unit reading some files and
    /// some earlier units; a build makes again exactly the units that read a changed file or a
    /// unit made again, and each says why through the ledger.
    struct Project {
        /// For each unit, the files it reads and the earlier units it reads.
        units: Vec<(Vec<usize>, Vec<usize>)>,
        built: bool,
    }

    /// What is planted in the model's build.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Plant {
        None,
        /// A unit nothing changed for is made again, its site giving a cause of an earlier
        /// build (the id of work this build has not done): a recompute started without a cause.
        Causeless,
        /// A unit is made again and never said to the ledger, while the stage counts it.
        Unsaid,
    }

    impl Project {
        /// One build after `edited` files changed: the ledger of what it did.
        fn build(&mut self, edited: &BTreeSet<usize>, plant: Plant) -> (Ledger, BTreeSet<usize>) {
            let mut ledger = Ledger::new();
            let changes: BTreeMap<usize, ChangeId> = edited
                .iter()
                .map(|file| (*file, ledger.changed(Writer::Source, format!("file {file}"))))
                .collect();
            let mut made: BTreeMap<usize, WorkId> = BTreeMap::new();
            let mut total = 0;
            for (unit, (files, deps)) in self.units.iter().enumerate() {
                let cause = match self.built {
                    false => Some(Cause::Standing(Standing::First)),
                    true => files
                        .iter()
                        .find_map(|file| changes.get(file).map(|id| Cause::Change(*id)))
                        .or_else(|| deps.iter().find_map(|dep| made.get(dep).map(|id| Cause::Work(*id)))),
                };
                let cause = match (cause, plant) {
                    (None, Plant::Causeless) if unit == self.units.len() - 1 => {
                        Some(Cause::Work(WorkId(u32::MAX)))
                    }
                    (cause, _) => cause,
                };
                let Some(cause) = cause else { continue };
                total += 1;
                if plant == Plant::Unsaid && unit == 0 {
                    continue;
                }
                made.insert(unit, ledger.did("unit", format!("unit {unit}"), cause));
            }
            ledger.total("unit", total);
            self.built = true;
            (ledger, made.keys().copied().collect())
        }

        /// The oracle's own answer: the units a change of `edited` reaches.
        fn reached(&self, edited: &BTreeSet<usize>) -> BTreeSet<usize> {
            let mut reached = BTreeSet::new();
            for (unit, (files, deps)) in self.units.iter().enumerate() {
                if files.iter().any(|it| edited.contains(it)) || deps.iter().any(|it| reached.contains(it)) {
                    reached.insert(unit);
                }
            }
            reached
        }
    }

    /// A drawn project and its edits, built in sequence under `plant` (in the last build), and
    /// what the oracle found wrong: nothing, when every recompute chains to a change.
    fn simulate(tc: &hegel::TestCase, plant: Plant) -> Vec<String> {
        use hegel::generators as gs;
        let files = 1 + tc.draw(gs::integers::<usize>().max_value(5));
        let count = 2 + tc.draw(gs::integers::<usize>().max_value(6));
        let mut units = Vec::new();
        for unit in 0..count {
            let reads: Vec<usize> = tc.draw(gs::vecs(gs::integers::<usize>().max_value(files - 1)).max_size(3));
            let deps: Vec<usize> = match unit {
                0 => Vec::new(),
                _ => tc.draw(gs::vecs(gs::integers::<usize>().max_value(unit - 1)).max_size(2)),
            };
            units.push((reads, deps));
        }
        if plant == Plant::Causeless {
            // A unit nothing reaches: the planted recompute is of it.
            units.push((Vec::new(), Vec::new()));
        }
        if plant == Plant::Unsaid {
            // The unit left unsaid reads the file the last build edits, so it is made again.
            units[0].0.push(0);
        }
        let mut project = Project { units, built: false };
        let builds = 2 + tc.draw(gs::integers::<usize>().max_value(4));
        let mut wrong = Vec::new();
        for build in 0..builds {
            let edited: BTreeSet<usize> = match build {
                0 => BTreeSet::new(),
                _ => tc.draw(gs::vecs(gs::integers::<usize>().max_value(files - 1)).max_size(2)).into_iter().collect(),
            };
            let first = !project.built;
            let last = build + 1 == builds;
            let mut edited = edited;
            if last && plant == Plant::Unsaid {
                edited.insert(0);
            }
            let expected = match first {
                true => (0..project.units.len()).collect(),
                false => project.reached(&edited),
            };
            let (ledger, made) = project.build(&edited, if last { plant } else { Plant::None });
            let closed = ledger.close();
            if closed.unexplained != 0 {
                wrong.push(format!("build {build}: {} pieces of work chain to no change", closed.unexplained));
            }
            if !closed.unbalanced.is_empty() {
                wrong.push(format!("build {build}: {:?}", closed.unbalanced));
            }
            if made != expected && plant == Plant::None {
                wrong.push(format!("build {build}: made {made:?}, the change reaches {expected:?}"));
            }
            for (_, _, what, root) in &closed.work {
                match root {
                    Root::Standing(Standing::First) if first => {}
                    Root::Change(id) if !first => {
                        let (writer, name) = ledger.change(*id).unwrap();
                        let file: usize = name.strip_prefix("file ").unwrap().parse().unwrap();
                        if writer != Writer::Source || !edited.contains(&file) {
                            wrong.push(format!("build {build}: {what} chains to {name}, which did not change"));
                        }
                    }
                    Root::Nowhere { .. } => {}
                    other => wrong.push(format!("build {build}: {what} has the root {other:?}")),
                }
            }
        }
        wrong
    }

    /// The invariant: in every build of a sequence of edits, each unit made again chains
    /// through the units it reads to a file that changed in that build (the first build's to
    /// "first time"), the units made are exactly those the change reaches, and the stage's
    /// total is the pieces its causes count.
    #[hegel::test(test_cases = 200)]
    fn every_recompute_of_a_simulated_build_sequence_chains_to_a_change(tc: hegel::TestCase) {
        assert_eq!(simulate(&tc, Plant::None), Vec::<String>::new());
    }

    /// The control: a recompute started with a cause that is no work of its build fails the
    /// oracle.
    #[hegel::test(test_cases = 50)]
    fn the_oracle_catches_a_recompute_started_without_a_cause(tc: hegel::TestCase) {
        let wrong = simulate(&tc, Plant::Causeless);
        assert!(wrong.iter().any(|it| it.contains("chain to no change")), "{wrong:?}");
    }

    /// The control of the stage sum: work the stage counted and no cause was given for.
    #[hegel::test(test_cases = 50)]
    fn the_oracle_catches_work_a_stage_counted_and_never_said(tc: hegel::TestCase) {
        let wrong = simulate(&tc, Plant::Unsaid);
        assert!(wrong.iter().any(|it| it.contains("StageSum")), "{wrong:?}");
    }

    #[test]
    fn a_chain_that_names_later_work_or_an_unseen_change_ends_nowhere() {
        let mut ledger = Ledger::new();
        let change = ledger.changed(Writer::Source, "a.rs");
        let a = ledger.did("unit", "a", Cause::Change(change));
        let b = ledger.did("unit", "b", Cause::Work(a));
        ledger.did("unit", "c", Cause::Work(WorkId(9)));
        ledger.did("unit", "d", Cause::Change(ChangeId(4)));
        ledger.did("unit", "e", Cause::Standing(Standing::Evicted { give_back: "bodies".to_owned() }));
        let closed = ledger.close();
        assert_eq!(closed.unexplained, 2);
        assert_eq!(closed.work[b.0 as usize].3, Root::Change(change));
        assert!(matches!(closed.work[2].3, Root::Nowhere { .. }));
        assert!(matches!(closed.work[3].3, Root::Nowhere { .. }));
        assert_eq!(closed.unbalanced, Vec::<StageSum>::new());
    }
}
