//! THE EDIT'S PLANNER (D187): which tasks an edit of checkout files enters, found forward from
//! what moved, over the stored answer read in place. It replaces, for an edit the stored answer
//! certifies everywhere else, the planner loops (engine_seeds.rs, engine_check.rs's pipelined
//! loop, engine_heads.rs `complete`) and every producer-header read (pages/headers.rs): a row is
//! found on its own page (`page_of`), never through a header shard.
//!
//! The steps, each a function here so the dry run (engine_dry.rs) and the `Ready` edit path
//! share one home (rule 30):
//!
//! 1. [`moved_cells`]: an outlined file's cells today against the ones its stored row defines
//!    (a cell gone is [`ABSENT`] today, a new one was [`ABSENT`]).
//! 2. [`keyed`]: the parts whose source moved (a body, a signature, a lint unit): entered by
//!    their keys whatever their reads say. A part new today has no row yet.
//! 3. [`readers`]: each moved cell's readers, from the readers section of the cell's own page.
//! 4. [`forward`]: each woken task's stored read list scanned; it enters when a read of a moved
//!    cell saw another digest than today's. A read of a cell the edit did not move holds its
//!    stored digest by the precondition (the answer certifies every other file), so it is not
//!    looked up. Woken tasks times their reads: never a sweep.
//!
//! What an entering task then defines can move more cells (a signature edit moves its head's
//! cell): the Ready driver's merge finds those readers the same way, on the pages of the tasks
//! it merged ([`readers`] again, one wave at a time), so this planner never guesses a second
//! wave. Measured on tempo (the scoreboard's edit, dry): 5 cells moved, 2 tasks woken and both
//! entering, 3 pages, 0 header shards, 2.3 ms warm for all four steps.

use blaze_drive::cells::Cell;
use blaze_drive::key::{ABSENT, Name};
use blaze_drive::{Locate, Part};
use blaze_engine::pages::VerifiedPage;
use blaze_engine::{CellId, SVec};
use blaze_ids::ContentId;

use crate::CliError;
use crate::engine_answer::Stored;

/// Every task the outlined files plan, onto `into`: each file's outline, its bodies (none for a
/// file of a skipped target only, `skipped` sorted), its heads (when the crates' names are
/// planned, `heads`) and its lint units under `rules`. THE ONE PLAN OF A FILE for both drivers
/// (rule 30): the phase path's whole plan (engine_check.rs `checking`) and the ready path's
/// edited files call it, so a file's plan, and its digest (pages.rs `plan_digest`, which decides
/// whether the file is in the edit), are one function's: a ready edit that planned a file
/// without its heads would drop their rows.
/// `reach`: which member units reach each file (engine_body_plan.rs `Reach`, D191), made once a
/// request by engine_bodies.rs `BodiesStage::plan_reach`: a body's row is planned at each unit
/// that reaches its file; `None` (no crate list) plans each body's base row alone.
pub(crate) fn file_tasks<'h>(outlined: &[crate::engine_tasks::Outlined<'_>], skipped: &[Name], external: &[Name], heads: bool, rules: &blaze_lint::RuleSet, reach: Option<&crate::engine_body_plan::Reach>, alloc: blaze_own::SharedAlloc<'h>, into: &mut SVec<'h, blaze_engine::Planned>) {
    for one in outlined.iter() {
        into.push(blaze_engine::Planned { name: one.name, phase: blaze_engine::Phase::new(blaze_engine::Stage::Outlines, 0), source: one.outline_source, file: one.name });
    }
    into.extend(crate::engine_bodies::plan_views::<blaze_drive::view::Production>(outlined, skipped, external, reach, alloc));
    if heads {
        into.extend(crate::engine_heads::plan_items(outlined, external, alloc));
    }
    into.extend(crate::engine_lints::plan(outlined, external, rules, alloc));
}

/// Which driver `check` runs an edit on (`--driver=auto|ready|phases`, main.rs `Flags::driver`):
/// `Auto` (the default) runs the fast ready path whenever the edit is eligible and falls back
/// cleanly to the phase path on any typed [`ReadyRefused`]; `Ready` is strict (refuses rather than
/// falling back, for tests and diagnostics); `Phases` runs the phase path explicitly (D187).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Driver {
    Auto,
    Phases,
    Ready,
}

impl Driver {
    pub(crate) fn parse(word: &str) -> Option<Driver> {
        match word {
            "auto" => Some(Driver::Auto),
            "phases" => Some(Driver::Phases),
            "ready" => Some(Driver::Ready),
            _ => None,
        }
    }
}

/// The ready commit's planted bug (D165), the control of tests/engine_ready_edit_tests.rs:
/// shipped code passes `None`. `DropsMovedPage` leaves the first entered task's cause out of the
/// commit's `Kept` (blaze-engine pages.rs `Kept::after`) before `engine_answer::save_changes`
/// encodes it, so that row's page keeps the last request's cause and its key, and the root's,
/// is not the phase path's: the world test's oracle (the two drivers' roots equal) must then
/// fail. Planted in engine_check.rs `checking`'s commit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CommitPlant {
    None,
    #[cfg_attr(not(test), expect(dead_code, reason = "the plant: only tests/engine_ready_edit_tests.rs takes it (rule 23)"))]
    DropsMovedPage,
    /// The ready path's head entries, found on their pages (engine_seeds.rs `PagedHeads`),
    /// answer every name "no head task": a body reading a member's head is refused, so the
    /// answer is not the phase path's (the control of rule 78's seam).
    #[cfg_attr(not(test), expect(dead_code, reason = "the plant: only tests/engine_ready_edit_tests.rs takes it (rule 23)"))]
    PagedHeadsMiss,
    /// The ready setup's members taken from the members record (engine_members_record.rs) give
    /// no head subjects: a body reading a standing member's head is placed otherwise or refused,
    /// so the answer is not the phase path's (the control of the setup's seam, rule 78).
    #[cfg_attr(not(test), expect(dead_code, reason = "the plant: only tests/engine_ready_edit_tests.rs takes it (rule 23)"))]
    StandingDropsHeads,
    /// The ready setup reads back no visited member's tree (engine_check.rs `read_needed` gets no
    /// `closed` names, only the entering tasks' files): a visited member's file the edit did not
    /// touch has no outline, so its walk is refused by name (`Unoutlined`), typed. The control of
    /// piece B2: the reads it chooses are the ones the bodies' setup needs.
    #[cfg_attr(not(test), expect(dead_code, reason = "the plant: only tests/engine_ready_edit_tests.rs takes it (rule 23)"))]
    ReadsNoVisitedTree,
    /// Every member's listing row keyed by its unit and the tool alone, without its names
    /// record's `out` or its files' heads (engine_member_rows.rs `row_key`), on either driver:
    /// an edit that moves a row's fields reads the old row. The control of the row's key (piece
    /// A): the tests' oracle (`RowCounts::unequal`) sees it.
    #[cfg_attr(not(test), expect(dead_code, reason = "the plant: only tests/engine_ready_edit_tests.rs takes it (rule 23)"))]
    RowKeyUnitOnly,
    /// The ready setup's stand below the heads (engine_bodies.rs `groups`, piece A) compares a
    /// moved held file by its text, not its listing facts: no member holding an edited file
    /// stands, so every holder is visited again (the control of the visit count: a file two
    /// units hold is visited twice).
    #[cfg_attr(not(test), expect(dead_code, reason = "the plant: only tests/engine_ready_edit_tests.rs takes it (rule 23)"))]
    HoldersKeyedByText,
    /// The ready setup never visits a member it stood below its heads, even where an entered
    /// body no unit ranked before it holds needs its claims (engine_bodies.rs `groups`, piece A):
    /// a body only the test unit holds is then decided by no unit, so the answer is not the
    /// phase path's (the control of the deferral's decision).
    #[cfg_attr(not(test), expect(dead_code, reason = "the plant: only tests/engine_ready_edit_tests.rs takes it (rule 23)"))]
    DeferredNeverVisited,
}

/// One leaf of the sweep that differs from the stored answer's (sweep.rs: a stamp or content id
/// moved, or the leaf came or went): its kind, and whether it is in both.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Changed {
    pub(crate) kind: crate::sweep::LeafKind,
    /// Present in the stored sweep and today (a text edit), not added or removed.
    pub(crate) both: bool,
}

/// The sweep's changed leaves ([`choose`]'s input): each row of the stored table whose state
/// today differs (sweep.rs `Followed::changed`), by its kind, and whether it is present both in
/// the table and today. A moved layout with no row of its own moved is a manifest's change.
pub(crate) fn changed_leaves<'h>(followed: &crate::sweep::Followed<'_>, alloc: blaze_own::SharedAlloc<'h>) -> SVec<'h, Changed> {
    let mut changed = SVec::new_in(alloc);
    for (at, now) in followed.now.rows.iter().enumerate() {
        if !followed.changed(at) {
            continue;
        }
        let was_absent = followed.was[at].0 & crate::sweep::ABSENT != 0;
        changed.push(Changed { kind: now.kind, both: !now.absent() && !was_absent });
    }
    if followed.layout_moved && changed.iter().all(|one| matches!(one.kind, crate::sweep::LeafKind::Slot)) {
        changed.push(Changed { kind: crate::sweep::LeafKind::Manifest, both: true });
    }
    changed
}

/// Which path a `check` takes, decided before any task runs and named on the profile's
/// `driver:` line: the edit path on `Ready` over exactly the names the forward wake entered, or
/// the phase path and why. Made in two steps, each a typed decision: [`choose`] from the stored
/// answer's root alone (no row read), then [`admit`] from the wake's entering names (their kinds
/// alone). Under `--driver=ready` either step's refusal is the request's error
/// ([`ReadyRefused`]), never the phase path run quietly in its place (rule 3); the phase path is
/// the first-check path while its stages are unported, never a second try.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EditPath<'e> {
    /// Only source files' texts moved, every one already in the stored answer, the answer owes
    /// nothing, and every entering task is of a kind the `Ready` path runs ([`READY_KINDS`]).
    Ready(Entering<'e>),
    /// The phase path, and why.
    Phases(PhasesWhy),
}

/// What [`choose`] decided from the root: plan the edit forward (the wake reads the pages of
/// `files` edited files and their readers), or take the phase path, and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Chosen {
    Wake { files: usize },
    Phases(PhasesWhy),
}

/// The kinds the `Ready` edit path runs (engine_ready_check.rs: one `TaskKind` each). An
/// entering task of any other kind (a signature, a crate's scope, an impl bucket, a build script)
/// is refused by [`admit`] until its stage is ported.
pub(crate) const READY_KINDS: [blaze_drive::Kind; 4] = [blaze_drive::Kind::FileItems, blaze_drive::Kind::Given, blaze_drive::Kind::Lint, blaze_drive::Kind::BodyCheck];

/// Why `--driver=ready` cannot serve this request: the flag asked for a driver that cannot, so
/// the request stops and says why (rule 3: the other driver is never run in its place; rule
/// 19: the first entering name of another kind is named).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ReadyRefused {
    /// The stored answer's root says this is no text edit the wake plans ([`choose`]).
    Root(PhasesWhy),
    /// The first entering task, by name, of a kind the `Ready` path does not run yet.
    Kind { name: Name },
    /// The entering names were not sorted and each once (a bug of the caller).
    Unsorted { at: usize },
    /// The ready setup (engine_check.rs `ready_setup`) found no row of `name` in the stored
    /// answer: it reads the unit graph's row in place of the units stage.
    NoRow { name: Name },
    /// The unit graph's record was not read back, or its crate list does not open
    /// (engine_units.rs `BackRefused`, the units stage's own words).
    Graph(crate::engine_units::BackRefused),
    /// The graph's record holds no crate list: its world was not made when it was sealed, and
    /// the finding of that is made again only on the phase path.
    NoList,
    /// A row of the crate list whose fact this crate cannot read (names.rs `NamesError::ListFact`).
    ListFact { row: usize, what: &'static str },
    /// An edited file the last root keeps no place for (blaze-engine pages.rs `FilePlace`): the
    /// ready setup outlines an edited file from its place alone.
    NoPlace { file: Name },
    /// `targets` target(s) their features skip have no note in the last lines record: the ready
    /// tail carries each such note from it (engine_check.rs `lines_of`, a layout that did not
    /// move skips the same targets with the same files), and never walks the plan to make it.
    /// Every request since the note was a `FileItems` block's Told entry writes it.
    SkippedTargets { targets: usize },
    /// An edited file's outline names other files than its stored outline (a `mod x;` added,
    /// removed or moved, or an edition): the module tree moved, and the ready setup plans no
    /// file walk ([`wake`], the first file whose file parts differ).
    ModuleTree { file: Name },
    /// The ready plan reads which member units reach each file from the last members record
    /// (engine_body_plan.rs `Reach::of_members`, D191), and none opens over today's graph
    /// (`unit: None`), or it holds no entry of member `unit`: the phase path walks the trees.
    Members { unit: Option<Name> },
}

impl core::fmt::Display for ReadyRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReadyRefused::Root(why) => write!(f, "--driver=ready cannot serve this request: {} (engine_ready_edit.rs `choose`)", why.words()),
            ReadyRefused::Kind { name } => write!(f, "--driver=ready cannot serve this request: the edit enters {name}, a {} task, and the ready path runs only FileItems, Given, Lint and BodyCheck tasks yet (engine_ready_edit.rs `admit`)", name.kind.name()),
            ReadyRefused::Unsorted { at } => write!(f, "the entering names are not sorted and each once at {at} (engine_ready_edit.rs `admit`)"),
            ReadyRefused::NoRow { name } => write!(f, "--driver=ready cannot serve this request: the stored answer holds no row of {name}, which the ready setup reads in place of the units stage (engine_check.rs `ready_setup`)"),
            ReadyRefused::Graph(why) => write!(f, "--driver=ready cannot serve this request: {why} (engine_check.rs `ready_setup`)"),
            ReadyRefused::NoList => f.write_str("--driver=ready cannot serve this request: the unit graph's record holds no crate list (its world was not made) (engine_check.rs `ready_setup`)"),
            ReadyRefused::ListFact { row, what } => write!(f, "--driver=ready cannot serve this request: the crate list's row {row}: {what} (names.rs `of_list`)"),
            ReadyRefused::NoPlace { file } => write!(f, "--driver=ready cannot serve this request: the edited file {file} has no place in the stored answer's root (engine_check.rs `ready_setup`)"),
            ReadyRefused::SkippedTargets { targets } => write!(f, "--driver=ready cannot serve this request: {targets} target(s) skipped by their features have no note in the last lines record, which the ready path carries (engine_check.rs `ready_setup`; a request on the phase path writes it)"),
            ReadyRefused::ModuleTree { file } => write!(f, "--driver=ready cannot serve this request: the edited file {file} names other files than its stored outline (its module tree moved), and the ready path walks no module tree (engine_ready_edit.rs `wake`)"),
            ReadyRefused::Members { unit: None } => f.write_str("--driver=ready cannot serve this request: no members record opens over today's unit graph, and the ready plan reads which units reach each file from it (engine_bodies.rs `plan_reach`; a request on the phase path writes it)"),
            ReadyRefused::Members { unit: Some(unit) } => write!(f, "--driver=ready cannot serve this request: the last members record holds no entry of the member unit {unit}, and the ready plan reads which files its module tree reaches from it (engine_bodies.rs `plan_reach`; a request on the phase path writes it)"),
        }
    }
}

impl ReadyRefused {
    /// The [`PhasesWhy`] recorded on the profile's `driver:` line when [`Driver::Auto`] routes a
    /// ready-ineligible request to the phase path.
    pub(crate) fn phases_why(self) -> PhasesWhy {
        match self {
            ReadyRefused::Root(why) => why,
            ReadyRefused::Kind { name } => PhasesWhy::ReadyKind(name.kind),
            ReadyRefused::ModuleTree { .. } => PhasesWhy::ModuleTree,
            ReadyRefused::Members { .. } => PhasesWhy::Members,
            ReadyRefused::Unsorted { .. }
            | ReadyRefused::NoRow { .. }
            | ReadyRefused::Graph(_)
            | ReadyRefused::NoList
            | ReadyRefused::ListFact { .. }
            | ReadyRefused::NoPlace { .. }
            | ReadyRefused::SkippedTargets { .. } => PhasesWhy::ReadySetup,
        }
    }
}

/// The second step of the precondition: the wake's entering names (sorted, each once) admitted
/// to the `Ready` path when every one is of a kind it runs ([`READY_KINDS`]); a woken `Script`
/// task refuses with [`ReadyRefused::Root`]`(`[`PhasesWhy::ScriptRead`]`)`, else the first
/// unsupported task is refused by name ([`ReadyRefused::Kind`]). Reads no page: a name carries
/// its kind.
pub(crate) fn admit(names: &[Name]) -> Result<EditPath<'_>, ReadyRefused> {
    let entering = Entering::new(names).map_err(|unsorted| ReadyRefused::Unsorted { at: unsorted.at })?;
    if names.iter().any(|name| name.kind == blaze_drive::Kind::Script) {
        return Err(ReadyRefused::Root(PhasesWhy::ScriptRead));
    }
    match names.iter().find(|name| !READY_KINDS.contains(&name.kind)) {
        Some(&name) => Err(ReadyRefused::Kind { name }),
        None => Ok(EditPath::Ready(entering)),
    }
}

/// Why a `check` takes the phase path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PhasesWhy {
    /// `--driver=phases` asked explicitly for the phase path.
    Chosen,
    /// No stored answer for this checkout: a first check.
    NoAnswer,
    /// The stored answer owes tasks or cells (a fault, a fetch that failed): the request that
    /// reaches them enters them, which the phase path's plan does today.
    Owed { tasks: usize, cells: usize },
    /// A leaf that is not a source file moved (a manifest, the lock, a listing, the home's
    /// module listing): the units stage's facts may move, which only the phase path plans yet.
    Layout,
    /// A source file came or went: the module tree moved (the Units and Names stages' input).
    FileAddedOrRemoved,
    /// A build script's read of the checkout moved (sweep.rs `LeafKind::by_script`: a file, a
    /// listing, a stat or a git fact it read; the first edit after a clean tree moves its
    /// `git status` answer): its `Given` cell moves and wakes the `Script` tasks that read it,
    /// which only the phase path runs yet.
    ScriptRead,
    /// Nothing moved: the certificate answers (engine_check.rs's own path).
    NothingMoved,
    /// A tool moved since the stored answer (its pointer's tools digest, engine_answer.rs
    /// `Pointer::certifies_under`): the rows whose kind's producer moved are entered by their
    /// keys, which the wake (from moved files alone) does not find; the phase path plans them.
    ToolsMoved,
    /// The stored answer is another checkout's (engine_answer.rs `LATEST_ROOT`: this checkout
    /// had none): its leaves were all followed here and its journal places are not this
    /// checkout's, so no wake from changed files is proved; the phase path plans every file.
    OtherCheckout,
    /// The forward wake entered a task whose kind is not yet on the ready path (e.g. a
    /// signature, crate scope, or impl bucket).
    ReadyKind(blaze_drive::Kind),
    /// An edited file's outline changed the module tree (`mod ...;` added, removed, or moved).
    ModuleTree,
    /// No compatible members record opened over today's unit graph.
    Members,
    /// The stored answer lacked a ready-setup precondition (graph row, crate list, file place, or
    /// skipped-target note).
    ReadySetup,
}

/// What the stored answer a request starts from is to it: a precondition of the certificate
/// and of the wake.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StoredBase {
    /// This checkout's own answer, written under today's tools.
    Own,
    /// This checkout's own answer, written under other tools (engine_answer.rs
    /// `Pointer::certifies_under`).
    ToolsMoved,
    /// Another checkout's answer, the home's latest (engine_answer.rs `LATEST_ROOT`).
    OtherCheckout,
}

impl PhasesWhy {
    pub(crate) fn words(self) -> &'static str {
        match self {
            PhasesWhy::Chosen => "--driver=phases",
            PhasesWhy::NoAnswer => "no stored answer",
            PhasesWhy::Owed { .. } => "the stored answer owes tasks or cells",
            PhasesWhy::Layout => "a manifest, the lock or a listing moved",
            PhasesWhy::FileAddedOrRemoved => "a source file came or went",
            PhasesWhy::ScriptRead => "a build script's read of the checkout moved (a `Given` cell and the `Script` tasks reading it, not on the ready path yet)",
            PhasesWhy::NothingMoved => "nothing moved",
            PhasesWhy::ToolsMoved => "a tool moved since the stored answer (its rows are entered by their keys)",
            PhasesWhy::OtherCheckout => "the stored answer is another checkout's (this one had none)",
            PhasesWhy::ReadyKind(kind) => match kind {
                blaze_drive::Kind::Signature => "the edit enters a Signature task (not on the ready path yet)",
                blaze_drive::Kind::CrateScope => "the edit enters a CrateScope task (not on the ready path yet)",
                blaze_drive::Kind::ImplBucket => "the edit enters an ImplBucket task (not on the ready path yet)",
                _ => "the edit enters a task kind not on the ready path yet",
            },
            PhasesWhy::ModuleTree => "an edited file's module tree moved",
            PhasesWhy::Members => "no compatible members record opens over today's unit graph",
            PhasesWhy::ReadySetup => "the stored answer lacked a ready-setup precondition",
        }
    }
}

impl core::fmt::Display for EditPath<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EditPath::Ready(entering) => write!(f, "driver: ready ({} tasks entering)", entering.len()),
            EditPath::Phases(why) => write!(f, "driver: phases ({})", why.words()),
        }
    }
}

/// The first step of the precondition, from the driver chosen, the stored answer's root
/// (`None`: none stored), what that answer is to this request ([`StoredBase`]), and the sweep's
/// changed leaves. Reads no row and no header. Under `--driver=phases` the phase path; under
/// `--driver=auto` the wake when eligible else the phase path; under `--driver=ready` the wake,
/// or the refusal that stops the request ([`ReadyRefused::Root`]).
pub(crate) fn choose(driver: Driver, root: Option<&blaze_engine::pages::Root<'_>>, base: StoredBase, changed: &[Changed]) -> Result<Chosen, ReadyRefused> {
    if driver == Driver::Phases {
        return Ok(Chosen::Phases(PhasesWhy::Chosen));
    }
    let refuse = |why: PhasesWhy| match driver {
        Driver::Auto => Ok(Chosen::Phases(why)),
        Driver::Ready => Err(ReadyRefused::Root(why)),
        Driver::Phases => Ok(Chosen::Phases(PhasesWhy::Chosen)),
    };
    let Some(root) = root else { return refuse(PhasesWhy::NoAnswer) };
    match base {
        StoredBase::Own => {}
        StoredBase::ToolsMoved => return refuse(PhasesWhy::ToolsMoved),
        StoredBase::OtherCheckout => return refuse(PhasesWhy::OtherCheckout),
    }
    if !root.owed_tasks.is_empty() || !root.owed_cells.is_empty() {
        return refuse(PhasesWhy::Owed { tasks: root.owed_tasks.len(), cells: root.owed_cells.len() });
    }
    if changed.is_empty() {
        return refuse(PhasesWhy::NothingMoved);
    }
    if changed.iter().any(|one| !matches!(one.kind, crate::sweep::LeafKind::Slot) && !one.kind.by_script()) {
        return refuse(PhasesWhy::Layout);
    }
    if changed.iter().any(|one| matches!(one.kind, crate::sweep::LeafKind::Slot) && !one.both) {
        return refuse(PhasesWhy::FileAddedOrRemoved);
    }
    Ok(Chosen::Wake { files: changed.len() })
}

/// The edit's scope on `Ready`: exactly the names the forward wake entered ([`forward`]'s
/// `entering`, sorted and each once), and nothing else. A task outside it keeps its stored row:
/// the precondition ([`choose`]) admitted only edits whose every other row the stored answer
/// certifies, so nothing outside is owed by leaving it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Entering<'e> {
    names: &'e [Name],
}

/// The entering names were not sorted and unique: a caller's bug, refused (rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct EnteringUnsorted {
    pub(crate) at: usize,
}

impl<'e> Entering<'e> {
    pub(crate) fn new(names: &'e [Name]) -> Result<Entering<'e>, EnteringUnsorted> {
        match names.windows(2).position(|pair| pair[0] >= pair[1]) {
            Some(at) => Err(EnteringUnsorted { at }),
            None => Ok(Entering { names }),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }
}

impl blaze_engine::Scope for Entering<'_> {
    fn runs(&self, task: &blaze_engine::Planned) -> bool {
        self.names.binary_search(&task.name).is_ok()
    }
}

/// Why a task is in the woken set. `Keyed` orders first, so after a sort by name the one entry
/// a name keeps is its strongest reason.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Woken {
    /// Its part's source moved: entered by its key.
    Keyed,
    /// It read a moved cell.
    Reader,
}

/// A cell the edit moved: its digest in the stored answer and today ([`ABSENT`] for none).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Moved {
    pub(crate) id: CellId,
    pub(crate) was: ContentId,
    pub(crate) now: ContentId,
}

/// The page that holds `name`'s row, cells and reader lists, read in place (never a header
/// shard).
pub(crate) fn page<'s, 'a>(stored: &'s Stored<'a>, name: Name) -> Result<Option<&'s VerifiedPage<'a>>, CliError> {
    page_for(stored, name, blaze_engine::Ask::Unsaid)
}

/// [`page`] asked for `ask`'s purpose (engine_answer.rs `AskedByKind` tells asks by it).
pub(crate) fn page_for<'s, 'a>(stored: &'s Stored<'a>, name: Name, ask: blaze_engine::Ask) -> Result<Option<&'s VerifiedPage<'a>>, CliError> {
    stored.named_page(name, ask).map_err(crate::engine_answer::page_error)
}

/// Step 1: `file`'s cells that moved, `stored` its stored row's defines and `today` the ones its
/// outline defines now, both sorted by cell. Appended to `into` sorted by cell id (one definer),
/// so [`forward`] can binary-search them.
pub(crate) fn moved_cells(file: Name, stored: impl Iterator<Item = (Cell, ContentId)>, today: &[(Cell, ContentId)], into: &mut SVec<'_, Moved>) {
    let mut stored = stored.peekable();
    let mut today = today.iter().copied().peekable();
    loop {
        let (cell, was, now) = match (stored.peek().copied(), today.peek().copied()) {
            (None, None) => break,
            (Some(old), Some(new)) if old.0 == new.0 => {
                stored.next();
                today.next();
                (old.0, old.1, new.1)
            }
            (Some(old), Some(new)) if old.0 < new.0 => {
                stored.next();
                (old.0, old.1, ABSENT)
            }
            (Some(old), None) => {
                stored.next();
                (old.0, old.1, ABSENT)
            }
            (_, Some(new)) => {
                today.next();
                (new.0, ABSENT, new.1)
            }
        };
        if was != now {
            into.push(Moved { id: CellId { of: file, cell }, was, now });
        }
    }
}

/// Step 2: the parts of a file's outline whose source moved between its stored record (`old`)
/// and today's (`new`), each once, appended as [`Woken::Keyed`]. A part only in one of them
/// moved (a body added or removed).
pub(crate) fn keyed(old: &[Part], new: &[Part], into: &mut SVec<'_, (Name, Woken)>) {
    let source_of = |parts: &[Part], name: Name| parts.iter().find(|part| part.name == name).map(|part| part.locate);
    for part in new.iter().chain(old.iter().filter(|part| new.iter().all(|now| now.name != part.name))) {
        if matches!(part.locate, Locate::Source(_)) && source_of(new, part.name) != source_of(old, part.name) {
            into.push((part.name, Woken::Keyed));
        }
    }
}

/// What [`readers`] read: the reader entries in all, and the longest list with its cell.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct ReaderCount {
    pub(crate) entries: usize,
    pub(crate) largest: usize,
    pub(crate) largest_at: Option<CellId>,
}

/// Step 3: every reader of each `moved` cell, from the readers section of the cell's own page
/// (blaze-engine pages.rs `readers_slot`), appended to `woken` as [`Woken::Reader`]; then
/// `woken` sorted by name, each name once (its keyed entry kept).
pub(crate) fn readers(stored: &Stored<'_>, moved: &[Moved], woken: &mut SVec<'_, (Name, Woken)>) -> Result<ReaderCount, CliError> {
    let mut count = ReaderCount::default();
    for moved in moved {
        let Some(page) = page_for(stored, blaze_engine::pages::readers_slot(&moved.id), blaze_engine::Ask::Readers)? else { continue };
        let readers = page.readers(&moved.id);
        count.entries += readers.len();
        if readers.len() > count.largest {
            count.largest = readers.len();
            count.largest_at = Some(moved.id);
        }
        woken.extend(readers.map(|name| (name, Woken::Reader)));
    }
    woken.sort_unstable();
    woken.dedup_by_key(|(name, _)| *name);
    Ok(count)
}

/// What [`wake`] did, for the profile's `wake:` line.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct Woke {
    /// Edited files outlined against their stored rows.
    pub(crate) files: usize,
    /// Cells the edit moved, over every edited file.
    pub(crate) moved: usize,
    /// Tasks woken: each edited file's outline, its keyed parts and the moved cells' readers.
    pub(crate) woken: usize,
    pub(crate) readers: ReaderCount,
    pub(crate) forward: Forward,
}

impl core::fmt::Display for Woke {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "wake: {} edited file(s), {} cell(s) moved, {} task(s) woken, {} reader entries (largest list {}), {} read-list entries scanned ({} on moved cells), {} woken with no row",
            self.files, self.moved, self.woken, self.readers.entries, self.readers.largest, self.forward.scanned, self.forward.on_moved, self.forward.no_row,
        )
    }
}

/// THE FORWARD WAKE of `check --driver=ready` (steps 1 to 4 above, the dry run's, rule 30) over
/// the edit's files (`files`: each edited file with today's plan of it, engine_check.rs
/// `checking`'s edit list) and today's outlines (`outlined`, sorted by name): each file's
/// outline row today (`Tasks::outline`, the `FileItems` kind's own code) against its stored row,
/// its parts against its stored outline record's, the moved cells' readers, and each woken
/// task's read list. The entering names, sorted, each once: every edited file's outline (its
/// source moved: it enters by its key, as the phase path enters it) and what the wake entered.
/// Reads the stored answer's pages in place (`page`), never a header shard, and one outline
/// record a file from the store.
pub(crate) fn wake<'a>(
    cx: &bstd::Cx,
    store: &mut dyn crate::store::Store<'_>,
    stored: &Stored<'_>,
    tools: &blaze_engine::Tools,
    outlined: &[crate::engine_tasks::Outlined<'_>],
    given: &[(Name, ContentId)],
    files: &[(Name, &[blaze_engine::Planned])],
    alloc: blaze_own::SharedAlloc<'a>,
) -> Result<(SVec<'a, Name>, Woke), CliError> {
    use crate::engine_check::EngineCheckError;
    let mut woke = Woke::default();
    let mut moved: SVec<'a, Moved> = SVec::new_in(alloc);
    let mut woken: SVec<'a, (Name, Woken)> = SVec::new_in(alloc);
    for &(file, group) in files.iter() {
        // A moved checkout read (`Kind::Given`): its `WHOLE` cell moves to today's answer digest,
        // waking its stored readers (bodies reading an `include_str!` data file stay on `ready`;
        // any woken `Script` task is refused by `admit` with `PhasesWhy::ScriptRead`).
        if file.kind == blaze_drive::Kind::Given {
            if group.is_empty() {
                if let Some(row) = page_for(stored, file, blaze_engine::Ask::Row)?.and_then(|page| page.row(file)) {
                    moved_cells(file, row.defines(), &[], &mut moved);
                }
                continue;
            }
            let answer = given
                .binary_search_by_key(&file, |(name, _)| *name)
                .ok()
                .and_then(|at| given.get(at))
                .map(|&(_, answer)| answer)
                .ok_or_else(|| EngineCheckError::ReadyHost {
                    task: file,
                    what: "a moved `Given` task with no planned checkout read answer (engine_ready_edit.rs `wake`)",
                })?;
            let now = [(crate::engine_tasks::WHOLE, answer)];
            if let Some(row) = page_for(stored, file, blaze_engine::Ask::Row)?.and_then(|page| page.row(file)) {
                moved_cells(file, row.defines(), &now, &mut moved);
            } else {
                moved_cells(file, core::iter::empty(), &now, &mut moved);
            }
            woken.push((file, Woken::Keyed));
            continue;
        }
        // An entry of the edit that is no source file's or checkout read's: its own task enters by
        // its key, and `admit` names it when its kind is not the ready path's.
        if file.kind != blaze_drive::Kind::FileItems {
            woken.push((file, Woken::Keyed));
            continue;
        }
        let planned = group.iter().find(|one| one.name == file).ok_or(EngineCheckError::ReadyHost { task: file, what: "an edited file with no outline task in today's plan (engine_ready_edit.rs `wake`)" })?;
        let at = outlined.binary_search_by(|one| one.name.cmp(&file)).map_err(|_| EngineCheckError::ReadyHost { task: file, what: "an edited file none of today's outlines holds (engine_ready_edit.rs `wake`)" })?;
        // 1. Today's row of the file, by the `FileItems` kind's own code.
        let task = blaze_engine::Task { planned: *planned, key: tools.key(file, &planned.source) };
        let today = crate::engine_tasks::Tasks { outlined }.outline(&task, alloc);
        let row_page = page_for(stored, file, blaze_engine::Ask::Row)?.ok_or(EngineCheckError::ReadyHost { task: file, what: "an edited file whose outline row's page the stored answer does not hold (engine_ready_edit.rs `wake`)" })?;
        let row = row_page.row(file).ok_or(EngineCheckError::ReadyHost { task: file, what: "an edited file with no outline row in the stored answer (engine_ready_edit.rs `wake`)" })?;
        // 2. Its cells and its parts against the stored row's and record's.
        moved_cells(file, row.defines(), &today.defines, &mut moved);
        let record_key = row_page.record(file).ok_or(EngineCheckError::ReadyHost { task: file, what: "an edited file whose stored outline row names no record (engine_ready_edit.rs `wake`)" })?;
        let held = store.get(cx, record_key)?.ok_or(EngineCheckError::OutlineBack(crate::engine_store::OutlineBack::Missing { name: file, key: record_key }))?;
        let old = crate::engine_store::outline_from(file, record_key, held.bytes, alloc).map_err(EngineCheckError::OutlineBack)?;
        let new_parts: &[Part] = match &outlined[at].made {
            Ok(record) => &record.parts,
            Err(_) => &[],
        };
        // The files this outline names, as the stored one named them: the module tree the
        // phase path's walk follows. Moved, the ready path cannot serve the edit (it walks no
        // tree: a file reached now or no longer would have no plan, or keep its rows).
        fn files_of(parts: &[Part]) -> impl Iterator<Item = (Name, Locate)> + '_ {
            parts.iter().filter(|part| matches!(part.locate, Locate::File { .. })).map(|part| (part.name, part.locate))
        }
        if !files_of(&old.parts).eq(files_of(new_parts)) {
            return Err(EngineCheckError::ReadyRefused(ReadyRefused::ModuleTree { file }).into());
        }
        keyed(&old.parts, new_parts, &mut woken);
        // The file's own outline: its source moved, so it enters by its key.
        woken.push((file, Woken::Keyed));
        woke.files += 1;
    }
    // One definer a file: sorted by cell id over all of them for `forward`'s search.
    moved.sort_unstable_by_key(|one| one.id);
    woke.moved = moved.len();
    // 3. The moved cells' readers (sorts `woken` by name, each once, its keyed entry kept).
    woke.readers = readers(stored, &moved, &mut woken)?;
    woke.woken = woken.len();
    // 4. Each woken task's read list, forward.
    let mut entering: SVec<'a, (Name, Woken)> = SVec::new_in(alloc);
    woke.forward = forward(stored, &moved, &woken, &mut entering, |_| {})?;
    let mut names: SVec<'a, Name> = SVec::with_capacity_in(entering.len(), alloc);
    names.extend(entering.iter().map(|(name, _)| *name));
    names.sort_unstable();
    names.dedup();
    Ok((names, woke))
}

/// Each entering name's task (`names`: [`wake`]'s, sorted, each once), with the key the engine
/// gives it, sorted by name: from the wake alone, never a scan of the checkout's plan (D187 step
/// 1 (a)). A name of an edited file is planned as today's plan of that file plans it (`files`,
/// [`file_tasks`]); any other name as its stored row was planned, read on the page [`forward`]
/// already read for it. That row's plan is today's: its file is in no entry of the edit, so the
/// file's plan digest in the stored root is today's (engine_check.rs `checking`'s edit list), and
/// the digest covers each task's name, phase and source (pages.rs `plan_digest`). A name with
/// neither is no task, as the phase path's plan holds none of it: an inline row (no file's plan
/// lists it), or a row of an edited file whose plan today no longer lists it (its row goes).
pub(crate) fn entering_tasks<'a>(
    stored: &Stored<'_>,
    tools: &blaze_engine::Tools,
    files: &[(Name, &[blaze_engine::Planned])],
    names: &[Name],
    alloc: blaze_own::SharedAlloc<'a>,
) -> Result<SVec<'a, blaze_engine::Task>, CliError> {
    let task_of = |planned: blaze_engine::Planned| blaze_engine::Task { planned, key: tools.key(planned.name, &planned.source) };
    let mut tasks: SVec<'a, blaze_engine::Task> = SVec::with_capacity_in(names.len(), alloc);
    for &(_, group) in files.iter() {
        tasks.extend(group.iter().filter(|one| names.binary_search(&one.name).is_ok()).map(|one| task_of(*one)));
    }
    tasks.sort_unstable_by_key(|task| task.planned.name);
    let edited = tasks.len();
    for &name in names.iter() {
        if tasks[..edited].binary_search_by_key(&name, |task| task.planned.name).is_ok() {
            continue;
        }
        let Some(row) = page_for(stored, name, blaze_engine::Ask::Row)?.and_then(|page| page.row(name)) else { continue };
        let facts = row.facts;
        if facts.inline || files.iter().any(|(file, _)| *file == facts.file) {
            continue;
        }
        tasks.push(task_of(blaze_engine::Planned { name, phase: facts.phase, source: facts.source, file: facts.file }));
    }
    tasks.sort_unstable_by_key(|task| task.planned.name);
    Ok(tasks)
}

/// What [`forward`] read.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct Forward {
    /// Read-list entries scanned, over every woken task's row.
    pub(crate) scanned: usize,
    /// Of them, reads of a moved cell (each compared with today's digest).
    pub(crate) on_moved: usize,
    /// Woken tasks with no stored row: a keyed part new today (entered), or a reader the readers
    /// section names with no row (a fault of the stored answer, counted, not entered).
    pub(crate) no_row: usize,
}

/// Step 4: of `woken` (sorted, each name once), the tasks that enter, appended to `entering`:
/// a keyed one always; a reader when one of its reads of a `moved` cell (sorted by id) saw
/// another digest than today's. `each_read` sees every read scanned (the dry run's grain count).
pub(crate) fn forward(stored: &Stored<'_>, moved: &[Moved], woken: &[(Name, Woken)], entering: &mut SVec<'_, (Name, Woken)>, mut each_read: impl FnMut(CellId)) -> Result<Forward, CliError> {
    let mut count = Forward::default();
    for &(name, why) in woken {
        let Some(row) = page_for(stored, name, blaze_engine::Ask::Row)?.and_then(|page| page.row(name)) else {
            count.no_row += 1;
            if why == Woken::Keyed {
                entering.push((name, why));
            }
            continue;
        };
        let mut enters = why == Woken::Keyed;
        for (read, digest) in row.reads() {
            count.scanned += 1;
            each_read(read);
            if let Ok(at) = moved.binary_search_by(|moved| moved.id.cmp(&read)) {
                count.on_moved += 1;
                enters |= moved[at].now != digest;
            }
        }
        if enters {
            entering.push((name, why));
        }
    }
    Ok(count)
}
