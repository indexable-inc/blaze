//! The `Bodies` stage of `blaze engine-check` (engine.md E5/E6, "The design" 4): every body of
//! the workspace's files type-checked and borrow-checked as ITS OWN engine task, entered once, on
//! the pool, by `check_body_reads` (front2 tables/task.rs) over the read-only view.
//!
//! THE PLAN, from text: one `BodyCheck` task per body part of each file's outline (work.rs
//! `outline_file`: one part per entry with a body of its own, and per entry a macro takes), its
//! name and source the outline's (`body_name`, `body_source`: the item's text), so the task's
//! key is known before any phase runs ([`plan`]).
//!
//! THE PHASE, on the coordinator first (a constant per member crate, never a step per body):
//! the crate list (the `UnitGraph` record), its trees, and for each member crate its check scope
//! (blaze-world check_scope.rs: the one rule of which crates a body sees), each scope crate's
//! names and items records read from the store as the Names stage put them, and the member's
//! files (its items record's file rows) matched to the planner's outlines. Each body of an owed
//! file is then matched to its item key through that file's listing (front2 project_records.rs
//! `list_files_with`, `index_by_chain`): the item key is what `check_body` finds a body by. What
//! the rest of a member's listing was for (the bodies a macro wrote, of blocks, and the note of
//! those no entry covers) is the member's listing row (engine_member_rows.rs), read by key; a
//! member none of whose files is owed is not opened.
//!
//! THE RUNS: the bodies of one member cut into runs of at most [`RUN`], each run a task of one
//! bstd scope; a run opens its own `Records` over the shared bytes (`Records` opens lazily
//! through `RefCell`s: never shared between threads, view.rs), and every body of the run is
//! checked over one `Tables` with its own recorder (task.rs). The stretch memoizes the IDs
//! of distinct questions over those tables. A panic fails its body alone (rule 20).
//!
//! WHAT A BODY READS, as the engine keys its readers: the view records names, lang, head and
//! impl-bucket cells. A crate's `ImplBucket` cell is read as read: the Heads stage defines it
//! (engine_heads.rs), or states its absence (no impl of the crate in that bucket). Until the
//! Names stage defines cells finer than its record's one whole cell (E4's wiring), the other
//! reads are WIDENED to a cell some phase defines ([`widen`]): a `CrateScope` cell to that
//! record's whole cell, the whole graph's bucket fact (made from the names records) to every
//! scope crate's. A `Signature` read of a member's head is the cell of the head task of the
//! entry that covers it (engine_heads.rs `Owners::place`; the body still makes the heads it
//! needs in its own store, task.rs), so a signature edit enters exactly the bodies whose head
//! read moved; a dependency's head, or one a macro wrote, is its crate's names record whole.
//! Settled names and impl tables share the graph's cell (engine_scope_reads.rs). Raw read
//! keys stay in the recorder until distinct, and only final row IDs need the engine's digests.

use core::convert::Infallible;
use core::num::NonZeroU32;

use blaze_drive::cells::Cell;
use blaze_drive::key::ABSENT;
use blaze_drive::{Input, Key, Kind, Locate, Name};
use blaze_core::SharedSet;
use blaze_engine::{Buffer, CellDigests, CellId, Done, Phase, Planned, SVec, Stage, Stands, Task, View};
use blaze_id::ItemKey;
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::front2::buckets::{ImplShelf, ImplShelfOpts, TableSet};
use blaze_ladder::front2::heads::NeedsMet;
use blaze_ladder::front2::project_records::{BodySource, Listing, index_outlined_by_chain, list_files_with};
use blaze_ladder::front2::source::NamesSource;
use blaze_ladder::front2::tables::cost::{Cost, Gauge};
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::tables::head::{CellFailure, ConstValues, FreezeError, NotTheirs, SeededHeads, SeededHeadsOpts, cell_id as head_cell_id};
use blaze_ladder::front2::tables::recorder::{PlacedCell, Recorder, RecorderOpts, TableRead};
use blaze_ladder::front2::tables::seeds::{HeadSeed, RequestSeedOpts, SeedError};
use blaze_ladder::front2::tables::task::{BodyFrame, ReadIds, ReadKeyer, Said as Line, SaidWhat, TaskError, WorkBytes, check_body_item};
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_own::{SharedAlloc, Vec};
use blaze_scope::crate_env::EnvLent;
use blaze_scope::items_record::ItemsRecord;
use blaze_world::crate_list::CrateGraph;
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::Frame;
use bstd::task::Class;
use std::alloc::Global;

use crate::CliError;
use crate::body_types::Kept;
use crate::engine_body_plan::{BodyClaim, BodyIs, FileClaim, FileIs, Reach, TaskRow};
use blaze_drive::view::{Production, ViewKey, ViewPlan, at_view};
use crate::engine_check::EngineCheckError;
use crate::engine_head_notes::{Cause, RootKind, Step, leak, root_cause};
use crate::engine_memory::InStretch;
use crate::engine_members_record::{EveryMember, SetupInputs, StandingMembers};
use crate::engine_names::NamesInputs;
use crate::engine_records::EngineRecords;
use crate::engine_scope_reads::ReadGrains;
use crate::engine_tasks::{Outlined, WHOLE};
use crate::fetch::FetchEnv;
use crate::run_names::Crate;
use crate::store::StoreSnapshot;
use crate::work;

/// Where the rule that refuses a body on this path is.
const RULE: &str = "engine_bodies.rs";

/// The most bodies one run checks: a run opens its own records once, and its frame holds every
/// body's lowering until the run ends (a body's frame is the records' lifetime in `check_body`,
/// so it cannot be reset per body yet: see the report's brief), so runs are short.
const RUN: usize = 32;

/// The most predicted cost one run checks, in bytes of its bodies' text (D167 (a): work at item
/// grain, a batch sized by predicted cost and capped, never grouped by origin). A run of short
/// entries ends at [`RUN`] entries or at this; an entry whose bodies are predicted past it (one
/// `storable_nested_arrays!()` call: 704 bodies) is cut into slices of at most this, each its own
/// run on its own worker, joined into the entry's one result after the join ([`join_slices`]);
/// a body predicted past it alone is a slice alone. The predictor is each body's text length,
/// known at plan time from the items record's rows with no text read ([`text_bytes`]); a cap by
/// body COUNT (64) left one slice of 64 bodies at 4,693 ms, 73 ms a body against a mean of 12
/// (profile score-c1725wave42). The cap in ms is this times the measured ms a byte, which the
/// profile's `bodies cost:` line prints each run from the slices it ran (the fit is what states
/// it; 48 KiB is chosen from no fit yet: UNSURE until that line is read).
const SLICE_BYTES: u64 = 48 << 10;

/// A body's least predicted cost: what checking any body costs beside its text (its stretch's
/// questions, its row), so a slice of many tiny bodies is still cut.
const MIN_BODY_BYTES: u32 = 256;

/// A body's predicted cost (D167 (a)): its text's length in bytes, from its crate's items record
/// alone (no text read): an item a macro wrote by its row's span, an item of `file` by its
/// outline entry's range, a member (an impl's or trait's that a macro wrote) by its member row's
/// span ([`member_lens`], made once a member crate when first asked). Never less than
/// [`MIN_BODY_BYTES`]; a body no row places costs that least.
fn text_bytes(
    items: &ItemsRecord<'_>,
    members: &mut Option<Vec<(ItemKey, u32), blaze_own::Alloc<'static>>>,
    outline: &blaze_file_items::Outline<'_>,
    file: u32,
    key: ItemKey,
    g: blaze_own::Alloc<'static>,
) -> u32 {
    use blaze_scope::resolve::At;
    let len = match items.identity(key) {
        Ok(Some(id)) => match id.at {
            Some(At::File(at)) if at == file => outline.items().get(id.outline.0 as usize).map_or(0, |entry| entry.range.len()),
            Some(At::Expansion(_)) => items.item(key).ok().flatten().and_then(|row| row.text).map_or(0, |span| span.len),
            _ => 0,
        },
        _ => {
            let lens = members.get_or_insert_with(|| member_lens(items, g));
            lens.binary_search_by_key(&key, |member| member.0).map_or(0, |at| lens[at].1)
        }
    };
    len.max(MIN_BODY_BYTES)
}

/// Bodies `from..to` of an entry predicted, summed ([`text_bytes`]); a body with no cost listed
/// (none should be: `groups` makes one a key) is predicted at the least.
fn predicted(costs: &[u32], from: usize, to: usize) -> u64 {
    (from..to).map(|at| u64::from(costs.get(at).copied().unwrap_or(MIN_BODY_BYTES))).sum()
}

/// Each member of an impl or trait a macro wrote, by key, with its text's length (its member
/// row's span), sorted by key: rows alone, no text read. A row that does not read is left out
/// (its body then costs [`MIN_BODY_BYTES`]; its check reads the row again and says why).
fn member_lens(items: &ItemsRecord<'_>, g: blaze_own::Alloc<'static>) -> Vec<(ItemKey, u32), blaze_own::Alloc<'static>> {
    use blaze_scope::resolve::At;
    let mut lens = Vec::new_in(g);
    for i in 0..items.items() {
        let Ok(Some(id)) = items.identity_at(i) else { continue };
        if !matches!(id.kind, blaze_surface::ItemKind::Impl | blaze_surface::ItemKind::Trait) || !matches!(id.at, Some(At::Expansion(_))) {
            continue;
        }
        let Ok(Some(members)) = items.members(id.key) else { continue };
        for member in members.iter().flatten() {
            lens.push((member.key, member.text.map_or(0, |span| span.len)));
        }
    }
    lens.sort_unstable_by_key(|member| member.0);
    lens
}

/// A run's frame: its records' decoded parts and its bodies' lowerings (reserved, not touched).
const BODIES_FRAME: usize = 1 << 30;

/// The domain of the out of a body no crate of the graph holds.
const OWNERLESS_DOMAIN: &str = "blaze-cli engine bodies ownerless 1";

/// The domain of the verdict of an entry whose expansion wrote several bodies (or none).
const ENTRY_DOMAIN: &str = "blaze-cli engine bodies expansion entry 1";

// The most bodies a macro wrote that no entry covers named by label in the request's note
// (the rest counted in it): a cap on the note, never on what is counted (rule 18). Its one
// home is the member row's form, which keeps a member's first ones (engine_member_rows.rs).
use crate::engine_member_rows::MOST_UNCOVERED_NAMED;

/// The lines block of the note of bodies a macro wrote that no entry covers: no task's (its
/// domain is no task name's), stable across requests.
const UNCOVERED_NOTE: &[u8] = b"{bodies a macro wrote that no entry covers}";

/// The label of the root's `fn main` that D104's launcher replaced (body_types/covered.rs
/// `Root::label`): its findings are at the entry's start, said so (`WRITTEN_UNPLACED`).
const REPLACED_MAIN: &str = "{the expansion that took `main`}";

/// The largest refusal classes shown in the profile. The rest are counted on one line so a
/// profile remains bounded even when every body has a distinct diagnostic.
const REFUSED_CAUSES_SHOWN: usize = 16;


/// THE PLAN OF A FILE'S BODIES (D191): one `BodyCheck` task a body part of each outlined file AT
/// EACH MEMBER UNIT WHOSE MODULE TREE REACHES THE FILE (engine_body_plan.rs [`Reach`]), sorted by
/// name, one a name. The first unit in rank keeps the part's plain name (the BASE row: a body
/// one unit reaches is named as it always was); each other unit's row is the name at its view
/// under `P` (blaze-drive view.rs `at_view`, the one spelling; run and test ask exactly that
/// name for the unit, run_ids.rs `views_of`). Every row has the body's text as its source, so an
/// edit of a body N units reach enters N rows and of a body one unit reaches, one. A file no
/// unit reaches, or `reach` none (no crate list), plans the base row alone.
///
/// CFG-AWARE HOLDERS (D191; engine_plan_cfg.rs): a unit whose `test` cuts a body's entry (its
/// `#[cfg]`s and its enclosing entries' and file's, `No` under every completion of the atoms
/// plan time does not know) has no row of it: a `#[cfg(test)]` body is the test build's alone,
/// at its plain name when the test build is its first holder; a body a unit reaching its file
/// cuts everywhere has no row at all. What plan time cannot decide keeps every reacher's row.
pub(crate) fn plan_views<'h, P: ViewPlan>(outlined: &[Outlined<'_>], skipped: &[Name], external: &[Name], reach: Option<&Reach>, alloc: SharedAlloc<'h>) -> SVec<'h, Planned> {
    let mut planned: SVec<'h, Planned> = SVec::new_in(alloc);
    // The holders' views of the body at hand, in rank: what `P` is asked over.
    let mut views: SVec<'h, ViewKey> = SVec::new_in(alloc);
    for one in outlined.iter() {
        // Outlines are retained for the module closure and its note, but a file exclusive to a
        // skipped target or an external checkout path package has no Bodies task.
        if skipped.binary_search(&one.name).is_ok() || external.binary_search(&one.name).is_ok() {
            continue;
        }
        let Ok(record) = &one.made else { continue };
        // Its bodies' cuts are read only where a unit reaches it (no crate list: none).
        let reached = reach.is_some_and(|reach| !reach.ranks(one.name).is_empty());
        let cuts = match reached {
            true => crate::engine_plan_cfg::file_cuts(one, alloc.local()),
            false => None,
        };
        for part in record.parts.iter() {
            if let (Kind::BodyCheck, Locate::Source(source)) = (part.name.kind, part.locate) {
                let cut = crate::engine_plan_cfg::part_cut(cuts.as_ref(), part);
                match reach {
                    Some(reach) => reach.views_into(one.name, cut, &mut views),
                    None => views.clear(),
                }
                let source = crate::engine_tasks::edition_source(source, one.edition);
                // Fold any enclosing/file/own `#[cfg]` / `#[cfg_attr]` attributes into the row's
                // key so editing an enclosing `#[cfg]` enters the row directly without needing
                // `Decision::Absent` to read `Section::Items` (rule 21).
                let source = match crate::engine_plan_cfg::part_cfg(cuts.as_ref(), part) {
                    Some(cfg) => crate::engine_body_plan::source_with_cfg(source, cfg),
                    None => source,
                };
                // A body two units or more hold: its units are in each row's key (rule 21).
                let source = match views.len() {
                    0 | 1 => source,
                    _ => crate::engine_body_plan::source_at_views(source, &views),
                };
                if !reached {
                    planned.push(Planned { name: part.name, phase: Phase::new(Stage::Bodies, 0), source, file: one.name });
                }
                for at in 0..views.len() {
                    planned.push(Planned { name: at_view(part.name, P::view_of(&views, at)), phase: Phase::new(Stage::Bodies, 0), source, file: one.name });
                }
            }
        }
    }
    planned.sort_unstable_by_key(|one| one.name);
    planned.dedup_by_key(|one| one.name);
    planned
}

/// Why a body gave no verdict: each named, printed as a note (rule 3: never a quiet pass).
#[derive(Debug)]
pub(crate) enum Said<E> {
    /// `check_body` refused the task (a fault of the plan or the view, task.rs `TaskError`).
    Task(E),
    /// The body's check panicked: this body alone failed (rule 20).
    Panicked,
    /// A frame refused an allocation for this body. Capture its use before unwinding drops
    /// the body's lists. The operation and read count identify the work that exhausted it.
    FrameUsedUp { used: u64, reserve: u64, what: &'static str, offers: u64, kept: u64, kinds: TopKept, stands: Stands },
    /// This stage could not give the body a check, and why.
    Refused(&'static str),
    /// The plan refused the body before any check (engine_body_plan.rs `Decision::Refused`):
    /// the words of its file's, its member's or its unit's records (its scope cut at a crate
    /// with no names record, its items record not open, ..), the same words for every body
    /// they refuse. A cause many bodies share, so the printer groups the bodies by it, one
    /// line a cause key, as a refused call's (blaze_report::query `groups`); `--each` prints
    /// each. Otherwise a [`Said::Refused`] in every respect.
    RefusedAtPlan(&'static str),
    /// The checker gave a verdict that refused the body or left a gap in it (task.rs `Done`
    /// with `failed`): the body was not checked whole. Its verdict's words why (task.rs
    /// `BodyOut::said`'s `NotChecked` and `BorrowsNotChecked` lines, said.rs's words), each
    /// line "<cause>: <words>", joined by "; " ("" when the verdict said none), are `why`.
    /// `calls` are the failures of the refused macro calls the body is not checked under
    /// (front2 path_miss_shown.rs `refused_call`), each once, in the order the body met them, in
    /// the request's heap; empty (no allocation) for a body under none. The calls are what many
    /// bodies share (one refused `make!` leaves every reader of its names unchecked), so the
    /// default view groups them by call (blaze_report::query `groups`, a `Cause::RefusedCall`
    /// each) and `why` is the body's own `Cause::NotWhole`. Both "" and empty: the verdict
    /// failed and said no cause (`Cause::Silent`).
    Unchecked { why: &'static str, calls: &'static [&'static str] },
    /// Types checked whole; the move and borrow check refused: its verdict's words why, as
    /// [`Said::Unchecked`]'s (a `Cause::BorrowsRefused` noted finding, never a failure).
    BorrowsUnchecked(&'static str),
}

/// Allocated in the request frame only when the body has a note. A task error has already
/// been rendered here, so none of its owned data needs a destructor in the frame.
struct BodyNote<'f> {
    said: Said<&'f str>,
    head_failed: Option<crate::engine_seeds::FirstFailure>,
}

impl<E> Said<E> {
    fn map_task<T>(self, map: impl FnOnce(E) -> T) -> Said<T> {
        match self {
            Said::Task(error) => Said::Task(map(error)),
            Said::Panicked => Said::Panicked,
            Said::FrameUsedUp { used, reserve, what, offers, kept, kinds, stands } => Said::FrameUsedUp { used, reserve, what, offers, kept, kinds, stands },
            Said::Refused(why) => Said::Refused(why),
            Said::RefusedAtPlan(why) => Said::RefusedAtPlan(why),
            Said::Unchecked { why, calls } => Said::Unchecked { why, calls },
            Said::BorrowsUnchecked(why) => Said::BorrowsUnchecked(why),
        }
    }
}

impl<'w> Said<&'w str> {
    /// The typed causes this says (D164: a body's note is a finding of each, its rule the
    /// cause's, its words the table's): one for every variant but [`Said::Unchecked`], which
    /// says its own causes `why` when there are any, then one a refused call it is under (a
    /// body whose name a refused call may have made is not checked under it, never an error of
    /// its own, never left out in silence), and [`Cause::Silent`] when it says neither.
    ///
    /// [`Cause::Silent`]: blaze_report::finding::Cause::Silent
    pub(crate) fn causes(&self, each: &mut dyn FnMut(blaze_report::finding::Cause<'w>)) {
        use blaze_report::finding::Cause;
        match *self {
            Said::Task(error) => each(Cause::Task { error }),
            Said::Panicked => each(Cause::Panicked),
            Said::FrameUsedUp { reserve, stands: Stands::OwedNextRequest, .. } => each(Cause::StretchFilled { reserve }),
            Said::FrameUsedUp { reserve, what, .. } => each(Cause::FrameUsedUp { what, reserve }),
            Said::Refused(why) => each(Cause::Refused { why }),
            Said::RefusedAtPlan(why) => each(Cause::PlanRefused { why }),
            Said::Unchecked { why: "", calls: [] } => each(Cause::Silent),
            Said::Unchecked { why, calls } => {
                if !why.is_empty() {
                    each(Cause::NotWhole { why });
                }
                for &call in calls {
                    each(Cause::RefusedCall { call });
                }
            }
            Said::BorrowsUnchecked(why) => each(Cause::BorrowsRefused { why }),
        }
    }
}

/// One body as the phase took it: what the printer and the profile read.
pub(crate) struct Ran<'f> {
    pub(crate) name: Name,
    /// Its item's name as written ("" when it was not found).
    pub(crate) label: &'f str,
    /// Its check's time on its worker, ns (0 when it was not checked).
    pub(crate) ns: u64,
    /// How many bytes its check grew its stretch's frame by (0 when it was not checked): what
    /// the profile's largest-frame line reads.
    pub(crate) bytes: u64,
    /// Its entry's chain (work.rs `item_chain`; 0 when it was not found).
    chain: u64,
    note: Option<&'f BodyNote<'f>>,
    /// Offset and length in the stage's findings, stable across row sorts and phases.
    found: FindingRange,
    /// Its file: an index into the planner's outlines.
    pub(crate) file: u32,
    /// Where its check stood in its stretch (engine_memory.rs `BodyFrames`).
    pub(crate) stretch: InStretch,
}

impl<'f> Ran<'f> {
    fn without_check(name: Name, label: &'f str, file: u32) -> Self {
        Self { name, label, ns: 0, bytes: 0, chain: 0, note: None, found: FindingRange::default(), file, stretch: InStretch::No }
    }

    fn said(&self) -> Option<&Said<&'f str>> {
        self.note.map(|note| &note.said)
    }
}

#[derive(Default)]
struct FindingRange {
    start: u32,
    len: u32,
}

/// A body found in its crate's records: its name and its item's name as written.
struct Took<'f> {
    name: Name,
    label: &'f str,
    /// Its member crate (an index of [`Group`]s) and the item keys it checks (its own body's,
    /// or each body an expansion at its entry wrote), or why it has none.
    item: Result<(u32, &'f [ItemKey]), &'static str>,
    /// Its entry's place: the entry's chain (work.rs `item_chain`) and, for an entry whose
    /// bodies a macro wrote, each one's label beside its key ([`Place`]).
    place: Place<'f>,
    /// The unit whose records found it (an index of `Found::holders`' units).
    unit: u32,
    /// Each body's predicted cost, one a key of `item` in its order ([`text_bytes`]).
    costs: &'f [u32],
    /// Each body's checker key as a program calls it, one a key of `item` in its order
    /// ([`asked_keys`]): what the task's run facts list as its covers. Empty when `item` is a
    /// refusal.
    asked: &'f [blaze_typeck::ItemKey],
}

/// The checker key each of `keys` is called by in a program (front2 keys.rs `asked_key`: a fn
/// of an impl or a trait is filed by its container's head under its MEMBER key, made from the
/// container's checker key, its kind and its name; any other item under its library key's first
/// 16 bytes), over `records`, the names records the keys were found in. A body's run facts list
/// these as its covers (engine_body_facts.rs), and run and test ask a called fn's row by the
/// key the program calls it by (run_ids.rs `RunIds::runnable`): before, the covers were every
/// key's first 16 bytes, so no method of the workspace was ever found (`<impl S>::name`: "the
/// body ids hold nothing for" it, base row and view row alike).
fn asked_keys<'n, A: core::alloc::Allocator>(records: &'n blaze_ladder::front2::source::Records<'n>, keys: &[ItemKey], into: &mut Vec<blaze_typeck::ItemKey, A>) {
    into.extend(keys.iter().map(|key| blaze_ladder::front2::keys::asked_key(records, *key)));
}

/// Where a body task's findings are printed: in the entry's item, found by its chain in its
/// file's outline as it is when printed (engine_lines.rs, a placed finding; rule 71).
#[derive(Clone, Copy)]
struct Place<'f> {
    chain: u64,
    /// The entry's file, as its crate's records number it (front2 `Records::file_now`), and the
    /// entry's range in that text: where a stretch reads the item's text to say its findings'
    /// ranges as token places ([`found_of`]).
    file: u32,
    item: blaze_surface::Range,
    /// One a body the task checks, in the order of its keys: `None` for the entry's own body
    /// (its findings' ranges are from the item's start); else a body whose text's place in the
    /// file is not known, by its label, so each finding is at the item's start, said so: a body
    /// an expansion at the entry wrote (body_types/covered.rs `written_label`,
    /// `WRITTEN_UNPLACED`), or a body of a block's module (covered.rs `in_blocks`,
    /// `BLOCK_UNPLACED`). Shorter than the keys: the rest are `None`.
    written: &'f [Option<Unplaced<'f>>],
}

/// A body of a task whose text's place in its file is not known: its label and the words that
/// say so (body_types/covered.rs `WRITTEN_UNPLACED` or `BLOCK_UNPLACED`).
#[derive(Clone, Copy)]
struct Unplaced<'f> {
    label: &'f str,
    words: &'static str,
}

/// A body of a block's module as a reader names it: the entry whose task checks it, then its
/// own name, `{a block of `_`}::serialize` (a member's: its own name in its impl).
struct BlockLabel<'r> {
    entry: &'r str,
    /// The body's name as its member row keeps it (engine_member_rows.rs `Block::name`).
    name: &'r str,
}

impl core::fmt::Display for BlockLabel<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{{a block of `{}`}}::{}", self.entry, self.name)
    }
}

/// One finding of a body task, as `check` files it (body_types.rs, where a body's findings are
/// filed): its rule, where it is in its item ([`FoundAt`]), and its message's pieces joined.
#[derive(Clone, Copy)]
struct BodyFinding<'f> {
    rule: u16,
    at: FoundAt,
    message: &'f [u8],
}

/// Still private to the body. Its findings borrow its scratch until the run appends them.
struct BodyResult<'a, 's> {
    done: Done<'a>,
    note: Option<(Said<blaze_own::Box<TaskError, SharedAlloc<'a>>>, Option<crate::engine_seeds::FirstFailure>)>,
    found: Vec<BodyFinding<'s>, &'s Frame>,
    /// The fns declared in the entry's bodies that its check reached (front2 tables/task.rs
    /// `BodyReads::locals`): in its scratch until the run appends them to [`RunOut::locals`].
    locals: Vec<ItemKey, &'s Frame>,
    /// Each clean body's typed record (blaze-typeck checked_record.rs; front2 tables/task.rs
    /// `BodyReads::typed`) as (its check's name, the bytes), copied out of the checker's work
    /// into the body's scratch: appended to [`RunOut::typed`] only when the body's result
    /// stands, so a panicked or refused body puts none.
    typed: Vec<(Name, &'s [u8]), &'s Frame>,
    /// Bodies whose verdict was clean (types and borrows) with no typed record: each one a body
    /// `blaze run` cannot build without a second check (floor 0).
    untyped_clean: u32,
    checked: bool,
    stands: Stands,
}

/// What a stretch's setup took before its first body began ([`run_stretch`]'s opening), in
/// wall ns, by part, summed over the stretches: the profile's `bodies: stretch setup` line.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct StretchSetup {
    stretches: u64,
    ns: u64,
    /// The impl shelf lent, the environment copied in, the scope's names records opened
    /// (run_names.rs `names_records_with_snapshot`).
    records_ns: u64,
    /// `Tables`, the read ids' memo and the request's head table placed for the stretch
    /// (engine_seeds.rs `StretchHeads::new`).
    heads_ns: u64,
    /// The lang index and the seed freezer: lang items, the constants' evaluator, receiver
    /// pointers (engine_head_consts.rs `lang_index`, `HeadConsts`, front2 receiver.rs).
    lang_ns: u64,
    /// Each scope crate's listing placed (engine_heads.rs `Owners::new`).
    owners_ns: u64,
}

impl StretchSetup {
    fn add(&mut self, other: StretchSetup) {
        self.stretches = self.stretches.saturating_add(other.stretches);
        self.ns = self.ns.saturating_add(other.ns);
        self.records_ns = self.records_ns.saturating_add(other.records_ns);
        self.heads_ns = self.heads_ns.saturating_add(other.heads_ns);
        self.lang_ns = self.lang_ns.saturating_add(other.lang_ns);
        self.owners_ns = self.owners_ns.saturating_add(other.owners_ns);
    }

    /// Its three largest parts, largest first, named.
    fn largest(&self) -> [(&'static str, u64); 3] {
        let mut parts = [("records opened", self.records_ns), ("heads placed", self.heads_ns), ("lang index and seed freezer", self.lang_ns), ("listings placed", self.owners_ns)];
        parts.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        [parts[0], parts[1], parts[2]]
    }
}

/// The wall ns from `from` to `to` (0 when the clock went back).
fn ns_between(from: bstd::clock::Instant, to: bstd::clock::Instant) -> u64 {
    to.since(from).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// One clean body's typed record in the phase's arena, put by [`BodiesStage::run_phase`] under
/// `engine_body_record::typed_key(bytes)`, its key named by `check`'s run facts.
struct TypedPut<'a> {
    check: Name,
    bytes: SVec<'a, u8>,
}

/// What a run hands on beside its `Done`s, filled as each body's task ends: each finding as
/// (the body's index in the run's `Ran`s, rule, place, its message's end in `messages`), in
/// the body's sorted order. The phase's arena, never the process heap; copied once into the
/// request's frame at the merge ([`BodiesStage::run_phase`]).
pub(crate) struct RunOut<'a> {
    found: SVec<'a, (usize, u16, FoundAt, usize)>,
    messages: SVec<'a, u8>,
    /// Indexed by row within this run. Only a body with a note occupies a slot.
    notes: SVec<'a, (usize, Said<blaze_own::Box<TaskError, SharedAlloc<'a>>>, Option<crate::engine_seeds::FirstFailure>)>,
    /// Each fn declared in a body a checked entry reached, as (the body's index in the run's
    /// `Ran`s, its library key): its entry's covers beside `task.items` (work.rs `covered`: "the
    /// entry's own fn and each fn declared in a body it reached"), so a call of it finds its row
    /// (run_ids.rs `workspace_of`). Phase arena only: dropped once `MadeFacts` is written, never
    /// on the stage's `Ran` row.
    locals: SVec<'a, (usize, ItemKey)>,
    clean: u32,
    items: u64,
    item_tasks: u64,
    /// A slice's bodies' verdict digests in item order ([`Part`]): what [`join_slices`] digests
    /// into its entry's out. Empty for a whole entry.
    outs: SVec<'a, ContentId>,
    /// The most a body's scratch frame held when its body ended ([`BODY_SCRATCH`]): one body's
    /// alone when the scratch is reset per body, all the run's when it is not (the plant).
    scratch_most: u64,
    /// What the run's scratch and stretch frames held committed ([`FramesHeld`]).
    frames: FramesHeld,
    /// The run's clean bodies' typed records ([`BodyResult::typed`]): a slice's are its own, and
    /// [`join_slices`] carries every slice's into its entry's run.
    typed: SVec<'a, TypedPut<'a>>,
    /// Clean bodies with no typed record ([`BodyResult::untyped_clean`]).
    untyped_clean: u32,
    /// The run's stretches' setup ([`StretchSetup`]).
    setup: StretchSetup,
    /// Each checkout file a body of the run read as data through its `Given` cell ([`data_read`],
    /// `DataCell::Followed`), its path and the answer the body saw: the leaf rows the request
    /// saves so the sweep follows them ([`BodiesStage::data_files`]).
    data_files: SVec<'a, (blaze_own::String<SharedAlloc<'a>>, ContentId)>,
}

impl<'a> RunOut<'a> {
    fn new(alloc: SharedAlloc<'a>) -> RunOut<'a> {
        RunOut { found: SVec::new_in(alloc), messages: SVec::new_in(alloc), notes: SVec::new_in(alloc), locals: SVec::new_in(alloc), clean: 0, items: 0, item_tasks: 0, outs: SVec::new_in(alloc), scratch_most: 0, frames: FramesHeld::default(), typed: SVec::new_in(alloc), untyped_clean: 0, setup: StretchSetup::default(), data_files: SVec::new_in(alloc) }
    }
}

// The stretch path's types, bounded where they are declared (rule 76).
const _: () = assert!(core::mem::size_of::<RunOut<'static>>() <= 4 << 10);

/// A body's scratch frame, reset when the body ends (rule 11): reserved once a run, nothing
/// committed until used. It holds every list of a body that is only a step toward its row
/// (lane BD3): the recorder's list and `seen` set, the cells before and after widening, the
/// verdict's words and bytes, its findings and their messages. A cap, not a size: the
/// recorder's own cap (front2 tables/recorder.rs `MOST_READS`, 2^20 reads) fills about 150 MB of
/// it with its cells, so 1 GiB leaves a body several times that; a body past it panics in its
/// own job (rule 20: the alloc-error hook) and is said as such.
const BODY_SCRATCH: usize = 1 << 30;

/// What a run's scratch keeps committed between two bodies: a reset keeps every block a body
/// grew it to (blaze-mem `Frame::reset`), so a run's scratch held its largest body's high-water
/// until the run's last body ended, once a worker. Past this the blocks ahead of the cursor go
/// back to the kernel as the next body begins (`Frame::trim`): a worker holds at most this
/// between bodies, and a large body pays its own page faults. 8 MiB holds the scratch of typical
/// bodies without pinning 576 MiB across 18 workers.
const SCRATCH_KEEP: usize = 8 << 20;

/// What the runs' frames held committed (rule 16): the scratch's most at a body's end, the
/// stretch frame's most at a stretch's end, and how many times a scratch was trimmed back to
/// [`SCRATCH_KEEP`]. The floor a worker: one body's scratch and one stretch's records, together.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FramesHeld {
    scratch_committed_most: u64,
    stretch_committed_most: u64,
    scratch_trims: u64,
}

impl FramesHeld {
    fn fold(&mut self, other: FramesHeld) {
        self.scratch_committed_most = self.scratch_committed_most.max(other.scratch_committed_most);
        self.stretch_committed_most = self.stretch_committed_most.max(other.stretch_committed_most);
        self.scratch_trims = self.scratch_trims.saturating_add(other.scratch_trims);
    }
}

/// One body's memory as its task ends (lane BD3; rule 16): what it leaves in the request's
/// arena (its row, to the request's end) and what its check took in its scratch (freed as it
/// ends), each counted from a list's length or capacity as it stood: adds only, no walk.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct BodyMem {
    /// The operation using the frame, retained across unwind for the refusal note.
    allocating: &'static str,
    #[cfg(test)]
    fault: Option<BodyFault>,
    /// The row's bytes in the request's arena: `Done` and its lists' capacities.
    pub(crate) row: u64,
    /// The row's floor: `Done` and its lists' lengths (a list of exactly what it keeps).
    pub(crate) row_floor: u64,
    /// Distinct recorder cells across all of this entry's items, before row widening.
    pub(crate) recorded: u64,
    kinds: KeptKinds,
    body_offers: u64,
    /// Distinct placed IDs, including the plan and data files, up to the last successful insert.
    distinct: u64,
    placed_all: bool,
    /// The body could not account for every individual read and filed its scope instead.
    coarse_refusal: bool,
    ids_made: u64,
    memo_hits: u64,
    /// Includes the stretch evaluator's offers for comparison with its shared ID memo.
    offered: u64,
    /// The body's recorders' memo, filter, coverage and by-question counts (front2
    /// tables/recorder.rs `ReadTally`; its `offered` and `kept` are the fields above's).
    asks: blaze_ladder::front2::tables::recorder::ReadTally,
    /// The cells handed to the widening (the evaluator's and the own crate's added).
    pub(crate) before: u64,
    /// The cells of its row, by kind: whole records, names cells, head cells, bucket cells, other.
    pub(crate) after: u64,
    pub(crate) whole: u64,
    pub(crate) names: u64,
    pub(crate) heads: u64,
    pub(crate) buckets: u64,
    pub(crate) other: u64,
    /// The crates of its check scope.
    pub(crate) scope: u64,
    /// The shared recorder and encoding at their peak, and the per-item lists summed.
    pub(crate) work: WorkBytes,
    /// Its scratch frame's use as it ended.
    pub(crate) scratch: u64,
}

/// The parts of [`BodyMem`] a line says, in its order.
const MEM_PARTS: usize = 23;

/// The first representative of each retained recorder cell, before row placement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct KeptKinds([u32; 24]);

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TopKept {
    kind: [u8; 5],
    count: [u32; 5],
}

impl KeptKinds {
    const NAMES: [&'static str; 24] = [
        "Binding", "Module", "Root", "Variants", "Item", "Members", "Expansion", "MemberOf",
        "VariantOf", "Beginning", "Lang", "Incoherent", "InternalSymbols", "Links", "ImplRows", "Files", "Extensions", "CfgAtoms", "Textual", "Whole", "Signature", "Text", "Head", "Impls",
    ];

    fn add(&mut self, key: TableRead) {
        use blaze_scope::names_cells::NameCell::*;
        let at = match key {
            TableRead::Names(name) => match name {
                Binding { .. } => 0, Module { .. } => 1, Root { .. } => 2, Variants { .. } => 3,
                Item { .. } => 4, Members { .. } => 5, Expansion { .. } => 6, MemberOf { .. } => 7,
                VariantOf { .. } => 8, Beginning { .. } => 9, Lang { .. } => 10, Incoherent { .. } => 11,
                InternalSymbols { .. } => 12, Links { .. } => 13, ImplRows { .. } => 14, Files { .. } => 15,
                Extensions { .. } => 16, CfgAtoms { .. } => 17, Textual { .. } => 18, Whole { .. } => 19,
            },
            TableRead::Signature { .. } => 20,
            TableRead::Text { .. } => 21,
            TableRead::Head { .. } => 22,
            TableRead::Impls { .. } => 23,
        };
        self.0[at] += 1;
    }

    fn top(&self) -> TopKept {
        let mut counts = self.0;
        let mut top = TopKept::default();
        for rank in 0..5 {
            let (at, count) = counts.iter().copied().enumerate()
                .max_by_key(|&(at, count)| (count, core::cmp::Reverse(at))).expect("read kinds");
            if count == 0 { break; }
            top.kind[rank] = at as u8;
            top.count[rank] = count;
            counts[at] = 0;
        }
        top
    }
}

impl core::fmt::Display for TopKept {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.count[0] == 0 { return f.write_str("none"); }
        for (rank, &count) in self.count.iter().take_while(|&&count| count != 0).enumerate() {
            if rank > 0 { f.write_str(", ")?; }
            write!(f, "{} {count}", KeptKinds::NAMES[self.kind[rank] as usize])?;
        }
        Ok(())
    }
}

impl BodyMem {
    fn place(&mut self, wide: &mut DistinctIds<'_>, id: CellId) {
        wide.insert(id);
        self.distinct = wide.len() as u64;
    }

    /// A checker that stopped before returning its BodyReads still owns its recorder.
    fn unfinished_reads(&mut self, recorder: &Recorder<'_>) {
        let read = recorder.tally();
        self.recorded += read.kept;
        self.offered += read.offered;
        self.body_offers += read.offered;
        self.asks.add(read);
        self.work.recorder = self.work.recorder.max(recorder.table_bytes());
        self.work.recorder_floor += read.kept * core::mem::size_of::<TableRead>() as u64;
        // Count the retained prefix even after ReadsFull. Filing a precise row separately
        // requires keys() to certify that the recorder kept every offer.
        for &key in recorder.recorded_prefix().iter() {
            self.kinds.add(key);
        }
    }

    fn parts(&self) -> [u64; MEM_PARTS] {
        let w = &self.work;
        [
            self.row,
            self.row_floor,
            self.recorded,
            self.before,
            self.after,
            self.whole,
            self.names,
            self.heads,
            self.buckets,
            self.other,
            self.scope,
            w.recorder,
            w.recorder_floor,
            w.cells,
            w.held,
            w.said,
            w.verdict,
            w.goal_hits,
            self.scratch,
            self.ids_made,
            self.memo_hits,
            self.offered,
            u64::from(self.coarse_refusal),
        ]
    }

    /// The row's read list counted by kind (`reads`: the row's, sorted, each once).
    fn count_row(&mut self, reads: &[(CellId, ContentId)]) {
        self.after = reads.len() as u64;
        for (id, _) in reads.iter() {
            let n = match (id.cell == WHOLE, id.of.kind) {
                (true, _) => &mut self.whole,
                (false, Kind::CrateScope) => &mut self.names,
                (false, Kind::Signature) => &mut self.heads,
                (false, Kind::ImplBucket) => &mut self.buckets,
                (false, _) => &mut self.other,
            };
            *n += 1;
        }
    }

    fn count_scope(&mut self, done: &Done<'_>) {
        self.coarse_refusal = true;
        self.distinct = done.reads.len() as u64;
        self.placed_all = true;
        self.count_row(&done.reads);
    }

    /// The row's bytes and its floor, from `done`'s capacities and lengths.
    fn count_done(&mut self, done: &Done<'_>) {
        let read = core::mem::size_of::<(CellId, ContentId)>() as u64;
        let define = core::mem::size_of::<(Cell, ContentId)>() as u64;
        let fixed = core::mem::size_of::<Done<'_>>() as u64;
        self.row = fixed + done.reads.capacity() as u64 * read + done.defines.capacity() as u64 * define;
        self.row_floor = fixed + done.reads.len() as u64 * read + done.defines.len() as u64 * define;
    }
}

/// One of the three largest bodies by recorded reads. The coordinator reserves the label
/// buffers before any worker runs. Publishing a body and reporting a refusal allocate nothing.
struct LargestBody {
    name: Option<Name>,
    path: SVec<'static, u8>,
    item: blaze_own::String<SharedAlloc<'static>>,
    mem: BodyMem,
}

/// Every body's [`BodyMem`] summed and at its most, written by the workers as each body ends
/// (relaxed adds; the counts are a gauge, read whole only after the join or by the status
/// line, where a body half-counted is one body's difference). Held on the status board
/// (engine_status.rs `Board::bodies_mem`), so the status, the stop's profile and the end's
/// profile each say it ([`BodyMemLive::line`]).
pub(crate) struct BodyMemLive {
    bodies: core::sync::atomic::AtomicU64,
    sums: [core::sync::atomic::AtomicU64; MEM_PARTS],
    most: [core::sync::atomic::AtomicU64; MEM_PARTS],
    largest: blaze_core::lock::Lock<Option<[LargestBody; 3]>>,
}

impl BodyMemLive {
    pub(crate) const fn new() -> BodyMemLive {
        BodyMemLive {
            bodies: core::sync::atomic::AtomicU64::new(0),
            sums: [const { core::sync::atomic::AtomicU64::new(0) }; MEM_PARTS],
            most: [const { core::sync::atomic::AtomicU64::new(0) }; MEM_PARTS],
            largest: blaze_core::lock::Lock::new("engine.bodies.largest", None),
        }
    }

    fn prepare(&self, ones: &[One<'_>], outlined: &[Outlined<'_>], alloc: SharedAlloc<'static>) -> Result<(), EngineCheckError> {
        if ones.is_empty() {
            return Ok(());
        }
        let path = ones.iter().map(|one| one.path(outlined).len()).max().unwrap_or(0);
        let item = ones.iter().map(|one| one.label.len()).max().unwrap_or(0);
        let mut largest = self.largest.lock();
        let slots = largest.get_or_insert_with(|| core::array::from_fn(|_| LargestBody {
            name: None,
            path: SVec::new_in(alloc),
            item: blaze_own::String::new_in(alloc),
            mem: BodyMem::default(),
        }));
        for slot in slots {
            slot.path.try_reserve_exact(path.saturating_sub(slot.path.len()))
                .map_err(|_| EngineCheckError::Grow { what: "the body memory profile's file path", had: slot.path.capacity() })?;
            slot.item.try_reserve(item.saturating_sub(slot.item.len()))
                .map_err(|_| EngineCheckError::Grow { what: "the body memory profile's item label", had: slot.item.capacity() })?;
        }
        Ok(())
    }

    /// One body ended with `mem`.
    fn add(&self, mem: &BodyMem, name: Name, path: &[u8], item: &str) {
        use core::sync::atomic::Ordering::Relaxed;
        self.bodies.fetch_add(1, Relaxed);
        for (at, part) in mem.parts().iter().enumerate() {
            self.sums[at].fetch_add(*part, Relaxed);
            self.most[at].fetch_max(*part, Relaxed);
        }
        // Equal counts are ordered by stable task name, independent of worker scheduling.
        // A second phase entering the same task can improve its rank but never lists it twice.
        let mut largest = self.largest.lock();
        let slots = largest.as_mut().expect("the Bodies phase prepares its status slots before running bodies");
        let end = slots.iter().position(|slot| slot.name == Some(name)).unwrap_or(slots.len() - 1);
        let rank = (core::cmp::Reverse(mem.recorded), name);
        if let Some(at) = slots[..=end].iter().position(|slot| {
            slot.name.is_none_or(|old| rank < (core::cmp::Reverse(slot.mem.recorded), old))
        }) {
            slots[at..=end].rotate_right(1);
            let slot = &mut slots[at];
            slot.name = Some(name);
            slot.mem = *mem;
            slot.path.clear();
            slot.path.extend_from_slice(path);
            slot.item.clear();
            slot.item.push_str(item);
        }
    }

    /// The bodies counted so far.
    pub(crate) fn bodies(&self) -> u64 {
        self.bodies.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// The line, a sibling of the profile's `bodies: held per body` line (nothing before the
    /// first body ends): `bodies memory: <n> bodies · row kept to the request's end <mean>
    /// bytes a body (most <m>; floor <f>, <r>x) · read cells a body: recorded <mean>/<most>,
    /// before widening .., after .. (whole records .., names cells .., head cells .., bucket
    /// cells .., other ..; scope crates ..) · freed as it ends, in its scratch: recorder <mean>
    /// bytes (most ..; floor .., <r>x), cells .., held .., said .., verdict .., goal hits ..;
    /// scratch at its end <mean> (most ..)`. Means rounded down; a ratio in tenths.
    pub(crate) fn line(&self, into: &mut impl core::fmt::Write) -> core::fmt::Result {
        use core::sync::atomic::Ordering::Relaxed;
        let n = self.bodies();
        if n == 0 {
            return Ok(());
        }
        let sum = |at: usize| self.sums[at].load(Relaxed);
        let mean = |at: usize| sum(at) / n;
        let most = |at: usize| self.most[at].load(Relaxed);
        let ratio = |of: usize, floor: usize| Tenths(sum(of).saturating_mul(10).checked_div(sum(floor)).unwrap_or(0));
        write!(
            into,
            "bodies memory: {n} bodies · row kept to the request's end {} bytes a body (most {}; floor {}, {}x) · read cells a body: recorded {}/{}, before widening {}/{}, after {}/{} (whole records {}, names cells {}, head cells {}, bucket cells {}, other {}; scope crates {}) · ids made {} against {} reads recorded, memo hits {} · freed as it ends, in its scratch: recorder {} bytes (most {}; floor {}, {}x), cells {} (most {}), held {}, said {}, verdict {}, goal hits {}; scratch at its end {} (most {})",
            mean(0),
            most(0),
            mean(1),
            ratio(0, 1),
            mean(2),
            most(2),
            mean(3),
            most(3),
            mean(4),
            most(4),
            mean(5),
            mean(6),
            mean(7),
            mean(8),
            mean(9),
            mean(10),
            sum(19),
            sum(21),
            sum(20),
            mean(11),
            most(11),
            mean(12),
            ratio(11, 12),
            mean(13),
            most(13),
            mean(14),
            mean(15),
            mean(16),
            mean(17),
            mean(18),
            most(18),
        )?;
        if sum(22) != 0 {
            write!(into, " · {} refusal(s) stand on the whole crate scope", sum(22))?;
        }
        let largest = self.largest.lock();
        if let Some(slots) = largest.as_ref() {
            for (at, slot) in slots.iter().take_while(|slot| slot.name.is_some()).enumerate() {
                let m = &slot.mem;
                write!(into, " · #{} `", at + 1)?;
                match core::str::from_utf8(&slot.path) {
                    Ok(path) => write!(into, "{path}")?,
                    Err(_) => write!(into, "{}", slot.path.escape_ascii())?,
                }
                write!(into, "` item `{}`: {} reads recorded, ", slot.item, m.recorded)?;
                write!(into, "{} distinct ids", m.distinct)?;
                if !m.placed_all {
                    write!(into, " so far (widening incomplete)")?;
                }
                write!(into, ", {} scratch bytes", m.scratch)?;
                write!(into, ", offers {}, kept {} distinct, top kept [{}]", m.body_offers, m.recorded, m.kinds.top())?;
                if at == 0 {
                    write!(into, " (recorded names {}, heads {}, impl buckets {}; row whole records {}, names cells {}, head cells {}, bucket cells {}, other {})",
                        m.kinds.0[..16].iter().sum::<u32>(), m.kinds.0[16], m.kinds.0[17], m.whole, m.names, m.heads, m.buckets, m.other)?;
                }
            }
        }
        writeln!(into)
    }
}

/// A ratio in tenths, said `12.3`.
struct Tenths(u64);

impl core::fmt::Display for Tenths {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}", self.0 / 10, self.0 % 10)
    }
}

/// The floor of what the stage holds a body after its task ends (rule 67): its task's name (the
/// key it is found by), its record's key (the verdict's digest) and a status byte.
pub(crate) const HELD_FLOOR: usize = core::mem::size_of::<Name>() + core::mem::size_of::<ContentId>() + 1;

/// What the stage holds past the bodies' tasks ([`BodiesStage::held`]): every body's row, its
/// label, findings and note words, summed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Held {
    pub(crate) bodies: u64,
    labels: u64,
    findings: u64,
    words: u64,
}

struct RefusedCause {
    count: usize,
    file: u32,
}

/// The stable class of one body's note. The first failed head carries its reason, which has no
/// item path, so bodies that read different failed heads remain in one class when their reason
/// is the same.
pub(crate) fn refused_cause<'f>(said: &Said<&str>, head_failed: Option<crate::engine_seeds::FirstFailure>, frame: &'f Frame) -> blaze_own::String<&'f Frame> {
    let mut cause = blaze_own::String::new_in(frame);
    let words = if let Some(first) = head_failed {
        cause.push_str("head failed: ");
        Some(first.why.split(": ").next().unwrap_or(first.why))
    } else {
        match said {
            Said::Task(words) => {
                cause.push_str("task: ");
                Some(*words)
            }
            Said::Panicked => {
                cause.push_str("Panicked");
                None
            }
            Said::FrameUsedUp { .. } => {
                cause.push_str("FrameUsedUp");
                None
            }
            Said::Refused(words) | Said::RefusedAtPlan(words) => {
                cause.push_str("Refused: ");
                Some(first_refusal_class(words))
            }
            Said::Unchecked { why, calls } => {
                cause.push_str("Unchecked: ");
                // The own causes' first class; a body under refused calls alone is classed by
                // the first call it is not checked under.
                Some(if why.is_empty() { calls.first().copied().unwrap_or("") } else { first_refusal_class(why) })
            }
            Said::BorrowsUnchecked(words) => {
                cause.push_str("BorrowsUnchecked: ");
                Some(first_refusal_class(words))
            }
        }
    };
    if let Some(words) = words {
        normalize_refused_words(words.trim(), &mut cause);
    }
    if let Some((at, _)) = cause.as_str().char_indices().nth(100) {
        cause.truncate(at);
    }
    cause
}

/// `words` (a verdict's not-checked words, front2 said.rs `refused_words`, which print the
/// refusal's `Debug`) onto `into`, each `ItemKey(<hex>)` said as `ItemKey(_)`. An item key is
/// its unit's (a library's and its test build's keys of one item differ: D191), so a note that
/// held it differed between the rows of one body and was printed once a holder (D191's fold
/// says a note once by its bytes, engine_lines.rs); and a key alone names nothing to a reader
/// (rule 19: the note already names the file and the body).
fn push_keyless(into: &mut blaze_own::String<&Frame>, words: &str) {
    const KEY: &str = "ItemKey(";
    let mut rest = words;
    while let Some(at) = rest.find(KEY) {
        let (before, from) = rest.split_at(at + KEY.len());
        into.push_str(before);
        let hex = from.bytes().take_while(u8::is_ascii_hexdigit).count();
        match from.as_bytes().get(hex) {
            Some(b')') if hex != 0 => {
                into.push('_');
                rest = &from[hex..];
            }
            _ => rest = from,
        }
    }
    into.push_str(rest);
}

fn first_refusal_class(words: &str) -> &str {
    // BorrowRefused prints its stable reason first. Its text availability may contain
    // "; ", and the detail after the bracket names a particular callee or source range.
    if words.starts_with("borrows not checked: [") && let Some(end) = words.find("] ") {
        return &words[..=end];
    }
    words.split("; ").next().unwrap_or(words)
}

fn normalize_refused_words(words: &str, into: &mut blaze_own::String<&Frame>) {
    let mut chars = words.chars().peekable();
    let mut previous = '\0';
    while let Some(ch) = chars.next() {
        if ch == '`' {
            into.push('_');
            while let Some(next) = chars.next() {
                if next == '`' {
                    break;
                }
            }
            previous = '_';
            continue;
        }
        if ch == '#' && chars.peek().is_some_and(|next| next.is_ascii_digit()) {
            while chars.peek().is_some_and(|next| next.is_ascii_digit()) {
                chars.next();
            }
            into.push('#');
            previous = '#';
            continue;
        }
        let boundary = !previous.is_ascii_alphanumeric() && previous != '_';
        if ch == '0' && chars.peek() == Some(&'x') && boundary {
            chars.next();
            while chars.peek().is_some_and(|next| next.is_ascii_hexdigit()) {
                chars.next();
            }
            into.push('#');
            previous = '#';
            continue;
        }
        let item_key = into.as_str().ends_with("ItemKey(");
        if item_key && ch.is_ascii_hexdigit() && boundary {
            while chars.peek().is_some_and(|next| next.is_ascii_hexdigit()) {
                chars.next();
            }
            into.push('#');
            previous = '#';
            continue;
        }
        if ch.is_ascii_hexdigit() && boundary {
            let mut look = chars.clone();
            let mut has_digit = ch.is_ascii_digit();
            let mut has_hex_letter = ch.is_ascii_hexdigit() && ch.is_ascii_alphabetic();
            while let Some(next) = look.next() {
                if !next.is_ascii_hexdigit() {
                    break;
                }
                has_digit |= next.is_ascii_digit();
                has_hex_letter |= next.is_ascii_alphabetic();
            }
            if has_digit && has_hex_letter {
                while chars.peek().is_some_and(|next| next.is_ascii_hexdigit()) {
                    chars.next();
                }
                into.push('#');
                previous = '#';
                continue;
            }
        }
        if ch.is_ascii_digit() {
            while chars.peek().is_some_and(|next| next.is_ascii_digit()) {
                chars.next();
            }
            into.push('#');
            previous = '#';
            continue;
        }
        into.push(ch);
        previous = ch;
    }
}

impl Held {
    const ROW: u64 = core::mem::size_of::<Ran<'_>>() as u64;

    fn mean(self, bytes: u64) -> u64 {
        bytes.checked_div(self.bodies).unwrap_or(0)
    }

    /// Bytes held a body, rounded down (0 with no body).
    pub(crate) fn per_body(self) -> u64 {
        let rows = self.bodies.saturating_mul(Self::ROW);
        self.mean(rows.saturating_add(self.labels).saturating_add(self.findings).saturating_add(self.words))
    }
}

/// PLANTED BUG, tests only (rule 23): a body's scratch is never reset between bodies, so a run's
/// scratch holds every body's findings at once; the answers must not change. Read on the
/// coordinator's thread when a phase begins ([`BodiesStage::run_phase`]).
#[cfg(test)]
thread_local! {
    pub(crate) static PLANT_KEEP_SCRATCH: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// PLANTED BUG, tests only (rule 23): the old arm of `groups`, every body entry the records
    /// list no body at taken as no unit's (`ENTRY_ABSENT`), silent: the control of the coverage
    /// tests. Read on the coordinator's thread ([`absent_entry`]).
    pub(crate) static PLANT_SILENT_ABSENT: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// PLANTED BUG, tests only (rule 23; lane BD3): the old row, its read list in the request's
    /// arena at the capacity of the list before widening (`widen` made it with the recorded
    /// count), so the memory line's row bytes are what the stage kept before. Read on the
    /// coordinator's thread when a phase begins.
    pub(crate) static PLANT_ROW_AT_RECORDED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// PLANTED BUG, tests only (rule 23; lane BD3): the coarse step off, a dependency's bucket
    /// reads kept one a bucket (as before) instead of its tables' `WHOLE`. Read on the
    /// coordinator's thread when a phase begins.
    pub(crate) static PLANT_FINE_BUCKETS: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    pub(crate) static PLANT_ALL_DEPENDENCIES_SETTLED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    pub(crate) static PLANT_BODY_FAULT: core::cell::Cell<Option<(&'static str, BodyFault)>> = const { core::cell::Cell::new(None) };
    pub(crate) static PLANT_CHECK_ONLY_GUARD: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    pub(crate) static PLANT_CHECKED_ON_PANIC: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// THE ORACLE ARM, tests only (D168 (2), rule 78): `groups` opens every member's world whole
    /// ([`Whole`]) instead of by need ([`ByNeed`]); every answer must be equal. Read on the
    /// coordinator's thread when a phase begins.
    pub(crate) static PLANT_OPEN_WHOLE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// PLANTED CONTROL, tests only: the read grains are made whole every command, never taken
    /// from the stored record (engine_scope_reads.rs `GrainsIn::rebuilt`): the edit's answer
    /// reads then grow with the crates.
    pub(crate) static PLANT_GRAINS_REBUILT: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyFault {
    RowFrame,
    WidenPanic,
    MissingFact,
    OweMissingFact,
    DropMissingFactRead,
    /// The control of a computed data include: its read's leaf row is not kept, so no task
    /// defines its `Given` cell.
    DropDataLeaf,
}

/// The memory plants of lane BD3 as the coordinator read them ([`PLANT_ROW_AT_RECORDED`],
/// [`PLANT_FINE_BUCKETS`]); both false outside tests.
#[derive(Clone, Copy, Default)]
struct RowPlants {
    row_at_recorded: bool,
    fine_buckets: bool,
    #[cfg(test)]
    all_dependencies_settled: bool,
    #[cfg(test)]
    fault: Option<(&'static str, BodyFault)>,
    #[cfg(test)]
    check_only_guard: bool,
    #[cfg(test)]
    checked_on_panic: bool,
    /// The oracle's arm of the open seam ([`OpenWorld`]): every member's world opened whole.
    #[cfg(test)]
    open_whole: bool,
    /// The planted control of the stored read grains ([`PLANT_GRAINS_REBUILT`]).
    #[cfg(test)]
    grains_rebuilt: bool,
}

/// D168 (2), "a world is opened by need": how [`BodiesStage::groups`] opens a member's world,
/// the one seam (rule 78), both arms through the one generic `groups`. A member's check scope
/// (each scope crate's names record read in place, engine_records.rs) and its listing row are
/// what the member's bodies are checked and its uncovered bodies counted from; [`ByNeed`] opens
/// the scope only for a member that holds an owed file (its bodies run), whose listing row is
/// not stored or does not open (it is made from the whole listing, which needs the scope), or
/// whose scope is cut at an input (its refusal names the cut, which `scope_of` words); every
/// other member is read from its own names record alone (its items record: its files, their
/// head subjects, its row's key) and its stored row. [`Whole`] opens every member's scope at the
/// phase's start, as before D168: the oracle the engine tests run against. Equal answers: a
/// member that is not opened has no owed file, so no body of it runs and no decision reads a
/// claim of it beyond the ones both arms make from its items record (`FileIs::NotHeld`); its
/// row is the stored one both arms read; its environment is made in both (a refused one drops
/// its row in both). The cold path costs nothing more: every member's row is missing, so every
/// member is opened, once, as before.
pub(crate) trait OpenWorld {
    const BY_NEED: bool;
}

/// Every member's world opened at the phase's start (the oracle arm).
pub(crate) struct Whole;

/// A member's world opened at its first need (the command's arm).
pub(crate) struct ByNeed;

impl OpenWorld for Whole {
    const BY_NEED: bool = false;
}

impl OpenWorld for ByNeed {
    const BY_NEED: bool = true;
}

/// Whether every crate of member `at`'s check scope has a names input this command, read from
/// the names stage alone (no record is opened): `false` when the scope would be cut at an input,
/// or cannot be made, so the caller opens it whole and `scope_of` words the refusal.
fn scope_inputs_whole(names: NamesInputs<'_, '_>, list: &blaze_world::crate_list::CrateList<'static>, at: usize, own: &crate::front_load::CrateRecords<'_>, g: blaze_own::Alloc<'static>) -> bool {
    let no_core = blaze_scope::names_record::NamesRecord::open(own.names_proof).crate_root().head().no_core;
    let Ok(checked) = blaze_world::check_scope::check_scope_in(list, at, no_core, g) else { return false };
    checked.crates().all(|one| names.input_of(blaze_drive::key::crate_scope_name(&list.key(one))).is_some())
}

/// THE STANDING DECISION, made one way for the setup that takes it ([`BodiesStage::groups`]
/// under `S::STANDING`) and the ready setup that reads back what the members it does not take
/// need ([`BodiesStage::ready_reads`]): member `place` stands from the members record when the
/// record was made over `today`, no outline it holds or reaches is in `moved` (sorted), and its
/// unit and its names input (name and out) are today's. `None`: it is visited.
///
/// The member's names input today (`unit.name`'s): entered this command, its entry (failed: it
/// visits); not entered, on the phase path none (it visits); not entered on the ready driver,
/// the row the stored answer it stands on holds, which IS the record's own, never read (piece
/// C: no page per member): the record is taken only over that answer (`View::standing` checks
/// `Header::made_at`), and every member in it was written with that answer's names input,
/// entered there or standing there (a member whose names failed is never written).
fn stands<'v, 'r>(view: &'v crate::engine_members_record::View<'r>, place: u32, today: &crate::engine_members_record::Header, moved: &[Name], unit: &crate::engine_body_plan::Unit, names: NamesInputs<'_, '_>) -> Option<&'v crate::engine_members_record::Member<'r>> {
    view.standing(place, today, moved).filter(|kept| stands_as(kept, unit, names))
}

/// Whether the member `kept` of a members record is today's `unit` with today's names input
/// (or refusal state for a standing refused member; the half of [`stands`] past the record's own
/// read list; [`stands_below`] shares it).
fn stands_as(kept: &crate::engine_members_record::Member<'_>, unit: &crate::engine_body_plan::Unit, names: NamesInputs<'_, '_>) -> bool {
    if kept.unit != *unit {
        return false;
    }
    match &kept.state {
        crate::engine_members_record::MemberState::Held(held) => {
            let today = match names.entry(unit.name) {
                Some(entered) => entered.map(|own| (own.name, own.out)),
                None => names.stood_on().map(|_| (unit.name, held.names_out)),
            };
            let Some((own_name, own_out)) = today else { return false };
            held.names == own_name && held.names_out == own_out && own_out != work::failed_out()
        }
        crate::engine_members_record::MemberState::CutScope(cut) => {
            let own_today = match names.entry(unit.name) {
                Some(entered) => entered.map(|own| (own.name, own.out)),
                None => names.stood_on().map(|_| (unit.name, cut.names_out)),
            };
            let Some((own_name, own_out)) = own_today else { return false };
            if cut.names != own_name || cut.names_out != own_out || own_out == work::failed_out() {
                return false;
            }
            match names.entry(cut.missing) {
                Some(Some(_)) => false,
                Some(None) => cut.missing_failed,
                None => names.stood_on().is_some(),
            }
        }
        crate::engine_members_record::MemberState::Refused(crate::engine_members_record::RefusedWhy::ScopeFailed) => match names.entry(unit.name) {
            Some(None) => true,
            Some(Some(_)) => false,
            None => names.stood_on().is_some(),
        },
        crate::engine_members_record::MemberState::Refused(crate::engine_members_record::RefusedWhy::NoNamesTask) => names.entry(unit.name).is_none(),
        crate::engine_members_record::MemberState::Refused(crate::engine_members_record::RefusedWhy::Unstood) => false,
    }
}

/// PIECE A, THE STAND BELOW THE HEADS: member `place` stands from the members record though a
/// file it holds moved, when each such file's listing facts today (`entries_now`) are the ones
/// the record kept (engine_members_record.rs `View::standing_below_heads`) and its unit and names
/// input are today's ([`stands_as`]). Everything the record keeps of it is then what a visit
/// would make; what a visit adds is its claims on the owed files, which `groups` asks of it only
/// when an entered task is its own row of such a file ([`own_row_entered`]).
fn stands_below<'v, 'r>(
    view: &'v crate::engine_members_record::View<'r>,
    place: u32,
    today: &crate::engine_members_record::Header,
    moved: &[Name],
    unit: &crate::engine_body_plan::Unit,
    names: NamesInputs<'_, '_>,
    entries_now: impl Fn(Name) -> Option<ContentId>,
) -> Option<&'v crate::engine_members_record::Member<'r>> {
    view.standing_below_heads(place, today, moved, entries_now).filter(|kept| stands_as(kept, unit, names))
}

/// How the ready setup takes a member a moved file is in whose listing facts did not move
/// (piece A, `groups`): stood below its heads and visited only at need ([`Deferral::Below`],
/// the command's), or the tests' controls (engine_ready_edit.rs `CommitPlant`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Deferral {
    Below,
    /// `CommitPlant::HoldersKeyedByText`: a moved held file compared by its text, so it never
    /// stands below its heads.
    ByText,
    /// `CommitPlant::DeferredNeverVisited`: stood below its heads, never visited at need.
    NeverVisited,
}

/// What a Bodies phase's setup is asked for ([`BodiesStage::groups`]): the entered tasks, each as
/// a body's row at one unit (`rows`, sorted by name: engine_body_plan.rs `task_rows`), and the
/// plan's reach the rows were named from.
#[derive(Clone, Copy)]
struct Entered<'t> {
    tasks: &'t [Task],
    rows: &'t [TaskRow],
    reach: Option<&'t Reach>,
}

/// Where the plan's reach is read from ([`BodiesStage::plan_reach`]), chosen by the driver's
/// setup: the phase path has every member's outlines, the ready path the edited files alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ReachFrom {
    /// Each member's module tree walked over the request's outlines (engine_body_plan.rs
    /// `Reach::walk`).
    Walk,
    /// Each member's held and reached files as the last members record keeps them
    /// (engine_body_plan.rs `Reach::of_members`).
    MembersRecord,
}

/// PIECE A under D191: whether a member stood below its heads (`kept`, its members record
/// entry; its unit at `rank` in the plan's reach) must be visited: an entered task is ITS row
/// (`rows`: each entered task as a body's row at one unit, engine_body_plan.rs `task_rows`) in a
/// file it holds. Its claims are read by its own rows' decisions alone (engine_body_plan.rs
/// `Holders::decide` decides a row from its unit's claims), so a member none of whose rows is
/// entered keeps what the record holds, unvisited; one whose row is entered is visited, so a
/// body two units hold has both units' claims when both rows are entered (an edit of its text).
/// A unit the reach does not rank (`None`) is visited: its walk is then checked against the
/// reach and the difference named (`groups`).
fn own_row_entered(kept: &crate::engine_members_record::Member<'_>, rows: &[TaskRow], rank: Option<u32>) -> bool {
    let Some(rank) = rank else { return true };
    match &kept.state {
        crate::engine_members_record::MemberState::Held(held) => rows.iter().any(|row| row.rank == rank && held.held.iter().any(|h| h.slot == row.file)),
        crate::engine_members_record::MemberState::CutScope(_) | crate::engine_members_record::MemberState::Refused(_) => true,
    }
}

/// The two names a held file's path in a member's items record may be outlined under: its own,
/// and, for `x/mod.rs`, the slot `x.rs` it is found at (sweep.rs `real_path`).
fn slot_names(path: &str, g: blaze_own::Alloc<'static>) -> [Option<Name>; 2] {
    let at_mod_rs = path.strip_suffix("/mod.rs").map(|stem| {
        let mut slot: Vec<u8, blaze_own::Alloc<'static>> = Vec::with_capacity_in(stem.len() + 3, g);
        slot.extend_from_slice(stem.as_bytes());
        slot.extend_from_slice(b".rs");
        Name::file(&slot)
    });
    [Some(Name::file(path.as_bytes())), at_mod_rs]
}

#[cfg(test)]
fn planted_rows() -> RowPlants {
    RowPlants {
        row_at_recorded: PLANT_ROW_AT_RECORDED.with(core::cell::Cell::get),
        fine_buckets: PLANT_FINE_BUCKETS.with(core::cell::Cell::get),
        all_dependencies_settled: PLANT_ALL_DEPENDENCIES_SETTLED.with(core::cell::Cell::get),
        fault: PLANT_BODY_FAULT.with(core::cell::Cell::get),
        check_only_guard: PLANT_CHECK_ONLY_GUARD.with(core::cell::Cell::get),
        checked_on_panic: PLANT_CHECKED_ON_PANIC.with(core::cell::Cell::get),
        open_whole: PLANT_OPEN_WHOLE.with(core::cell::Cell::get),
        grains_rebuilt: PLANT_GRAINS_REBUILT.with(core::cell::Cell::get),
    }
}

#[cfg(not(test))]
fn planted_rows() -> RowPlants {
    RowPlants::default()
}

#[cfg(test)]
fn planted_keep_scratch() -> bool {
    PLANT_KEEP_SCRATCH.with(core::cell::Cell::get)
}

#[cfg(not(test))]
fn planted_keep_scratch() -> bool {
    false
}

#[cfg(test)]
fn planted_silent_absent() -> bool {
    PLANT_SILENT_ABSENT.with(core::cell::Cell::get)
}

#[cfg(not(test))]
fn planted_silent_absent() -> bool {
    false
}

/// Where a body's finding is in its item. By tokens, never bytes (rule 71): the task's key is
/// its item's tokens (blaze-file-items, "Identity"), so it stands across a comment or a reformat
/// and its stored lines are printed again unentered; a byte range would then point into the old
/// layout, a token place points at the same tokens in the new (`blaze_file_items::place_bytes`,
/// at print time).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FoundAt {
    /// At the item's start: a body whose text's place in its file is not known ([`Unplaced`]).
    Start,
    /// These tokens of the item's text, counted from its first token.
    Tokens(parser::token_key::TokenPlace),
}

/// One member crate's check scope as a run reads it: all read-only, shared by its runs. Its
/// crates' records are borrowed in place from the store's mapping for `'r` (engine_records.rs).
struct Group<'f, 'r> {
    list: blaze_world::crate_list::CrateList<'static>,
    /// The member's own `CrateScope` name.
    own: Name,
    /// Its scope's crates in reader order, the member at 0: opened at the setup, or past the
    /// member each at its first question ([`GroupCrates`]).
    crates: GroupCrates<'r>,
    /// The member's environment, what its bodies' `env!` read: its crate list facts with its
    /// build script's outputs joined (body_types.rs `owner_env_by`), lent to each run, and the
    /// script run read for it (a `Script` task's WHOLE cell), in every body's read list.
    env: Option<EnvLent<'f, 'f>>,
    script: Option<Name>,
    /// The places of its lang crates among `crates`.
    lang: Vec<usize, Global>,
    /// Each scope crate's `CrateScope` name, sorted: the whole graph's bucket fact widens to these.
    scope: Vec<Name, Global>,
}

/// A group's scope crates in reader order, the member at 0: the two arms of piece C (a) (rule
/// 78, the arms of [`OpenWorld`]). [`GroupCrates::Opened`]: every crate's records opened at the
/// setup ([`Whole`], [`scope_of`]); [`GroupCrates::ByNeed`]: the member's alone, every other
/// crate's at the first question a body job asks of it (the request's
/// [`crate::front_load::ScopeRecords`], [`scope_by_need`]). Both answer every question alike: the
/// engine tests' oracle runs [`Whole`] against [`ByNeed`].
enum GroupCrates<'r> {
    Opened(Vec<Crate<'r>, Global>),
    ByNeed { own: Crate<'r>, rest: Vec<crate::front_load::Unopened, Global> },
}

impl<'r> GroupCrates<'r> {
    fn len(&self) -> usize {
        match self {
            GroupCrates::Opened(crates) => crates.len(),
            GroupCrates::ByNeed { rest, .. } => rest.len() + 1,
        }
    }

    /// The member's own crate (reader crate 0).
    fn own(&self) -> Option<&Crate<'r>> {
        match self {
            GroupCrates::Opened(crates) => crates.first(),
            GroupCrates::ByNeed { own, .. } => Some(own),
        }
    }

    /// Reader crate `k`'s place in the crate list.
    fn world_at(&self, k: usize) -> Option<usize> {
        match self {
            GroupCrates::Opened(crates) => crates.get(k).map(|one| one.world_at),
            GroupCrates::ByNeed { own, rest } => match k {
                0 => Some(own.world_at),
                k => rest.get(k - 1).map(|one| one.world_at),
            },
        }
    }

    /// Each crate's key in reader order, none opened for it.
    fn keys(&self) -> impl Iterator<Item = blaze_id::CrateItemKey> + '_ {
        let (opened, own, rest) = match self {
            GroupCrates::Opened(crates) => (&crates[..], None, &[][..]),
            GroupCrates::ByNeed { own, rest } => (&[][..], Some(own.records.key), &rest[..]),
        };
        opened.iter().map(|one| one.records.key).chain(own).chain(rest.iter().map(|one| one.key))
    }

    /// The crates as a reader's records are opened over them, by-need ones from `records`.
    fn scope<'a>(&'a self, records: &'a dyn crate::front_load::OpenByNeed) -> crate::run_names::ScopeCrates<'a, 'a> {
        match self {
            GroupCrates::Opened(crates) => crate::run_names::ScopeCrates::Opened(&crates[..]),
            GroupCrates::ByNeed { own, rest } => crate::run_names::ScopeCrates::ByNeed(crate::run_names::ByNeedScope { own: *own, rest: &rest[..], records }),
        }
    }

    /// The crates with their records in reader order, for a refusal's words alone
    /// (engine_head_consts.rs `UnplacedIn`, which names a crate by its reader index): a by-need
    /// crate opened now, the list ending before the first whose record does not open (so every
    /// index kept is its crate's; that crate's job is refused by the load's words when a body
    /// asked it: front_load.rs `StoreLoad::refused`).
    fn opened<'a>(&'a self, records: &'a dyn crate::front_load::OpenByNeed, g: blaze_own::Alloc<'static>) -> Vec<Crate<'a>, blaze_own::Alloc<'static>> {
        let mut all = Vec::with_capacity_in(self.len(), g);
        match self {
            GroupCrates::Opened(crates) => all.extend(crates.iter().copied()),
            GroupCrates::ByNeed { own, rest } => {
                all.push(*own);
                for one in rest.iter() {
                    let Ok(opened) = records.open(one) else { break };
                    all.push(Crate { records: opened, name: one.name, package: one.package, world_at: one.world_at });
                }
            }
        }
        all
    }
}

/// One crate's check scope with its records opened: what the Heads stage files a crate's impls
/// over and the Bodies stage checks a member's bodies over ([`scope_of`], the one rule of both).
pub(crate) struct Scope<'r> {
    /// The crate's own records.
    pub(crate) own_records: crate::front_load::CrateRecords<'r>,
    /// Its scope's crates in reader order, the crate at 0 (cut at the first without records).
    pub(crate) crates: Vec<Crate<'r>, Global>,
    /// The places of its lang crates among `crates`.
    pub(crate) lang: Vec<usize, Global>,
    /// Each scope crate's `CrateScope` name, sorted.
    pub(crate) scope: Vec<Name, Global>,
    /// The first scope crate without records, by its world index, when `crates` was cut there,
    /// and why (one value, so a cut never lacks its cause): what a refusal names (rule 19, plan
    /// row A8, [`Scope::cut_cause`]), and, by its `CrateScope` name ([`Scope::cut_name`]), what
    /// a refusal of the cut scope reads beside `scope`.
    pub(crate) cut: Option<(usize, CutWhy)>,
}

impl Scope<'_> {
    /// The `CrateScope` name of the crate the scope was cut at ([`Scope::cut`]), the cell a
    /// refusal of the cut scope reads (as ABSENT where that crate has no `Names` task).
    pub(crate) fn cut_name(&self, list: &blaze_world::crate_list::CrateList<'static>) -> Option<Name> {
        self.cut.as_ref().map(|(one, _)| blaze_drive::key::crate_scope_name(&list.key(*one)))
    }

    /// The crate the scope was cut at and why, by its name: what a refusal of the cut scope
    /// prints (`check scope holds `p`, which ...`).
    pub(crate) fn cut_cause(&self, list: &blaze_world::crate_list::CrateList<'static>) -> Option<CutCause<'_>> {
        self.cut.as_ref().map(|(one, why)| CutCause { krate: CrateGraph::name(list, *one), why })
    }
}

/// Why a check scope was cut at one of its crates ([`Scope::cut`]).
#[derive(Debug)]
pub(crate) enum CutWhy {
    /// The crate's `CrateScope` task defines no names record this command: it failed
    /// (`failed`), or no `Names` task was planned for it.
    NoNamesRecord { failed: bool },
    /// Its names record is defined but could not be read in place (engine_records.rs).
    Unreadable(crate::engine_records::ReadError),
}

/// A cut scope's crate and its [`CutWhy`], printed after "its crate's" or "its member's".
pub(crate) struct CutCause<'s> {
    pub(crate) krate: &'static str,
    pub(crate) why: &'s CutWhy,
}

impl core::fmt::Display for CutCause<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let krate = self.krate;
        match self.why {
            CutWhy::NoNamesRecord { failed: true } => {
                write!(f, "check scope holds `{krate}`, which has no names record this command (its `CrateScope` task failed: that failure is reported at it)")
            }
            CutWhy::NoNamesRecord { failed: false } => {
                write!(f, "check scope holds `{krate}`, which has no names record this command (it has no `Names` task: engine_names.rs `plan`)")
            }
            CutWhy::Unreadable(error) => write!(f, "check scope holds `{krate}`, whose names record could not be read: {error} (engine_bodies.rs `scope_of`)"),
        }
    }
}

/// Why crate `at` has no check scope at all ([`scope_of`]): each drop the read could meet,
/// typed, with the crate named (rule 19).
#[derive(Debug)]
pub(crate) enum ScopeRefusal {
    /// The crate's own `CrateScope` task defines no names record this command: it failed
    /// (`failed`, reported at it), or no `Names` task was planned for it.
    NoNamesRecord { krate: &'static str, at: usize, failed: bool },
    /// Its own names record is defined but could not be read in place.
    Records { krate: &'static str, at: usize, error: crate::engine_records::ReadError },
    /// Its check scope could not be made from the crate graph (blaze-world check_scope.rs).
    CheckScope { error: blaze_world::check_scope::ScopeError<'static> },
}

impl core::fmt::Display for ScopeRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ScopeRefusal::NoNamesRecord { krate, failed: true, .. } => {
                write!(f, "crate `{krate}` has no names record this command (its `CrateScope` task failed: that failure is reported at it)")
            }
            ScopeRefusal::NoNamesRecord { krate, failed: false, .. } => {
                write!(f, "crate `{krate}` has no names record this command (it has no `Names` task: engine_names.rs `plan`)")
            }
            ScopeRefusal::Records { krate, error, .. } => write!(f, "the names record of crate `{krate}` could not be read: {error} (engine_bodies.rs `scope_of`)"),
            ScopeRefusal::CheckScope { error } => write!(f, "its check scope could not be made: {error} (engine_bodies.rs `scope_of`)"),
        }
    }
}

/// Per-phase cache of each crate's opened `(Input, CrateRecords)` indexed by world crate index
/// (`0..list.len()`), so building check scopes for many crates in one phase (`HeadsStage`) asks
/// `EngineRecords` at most once per unique crate instead of repeating lookups across overlapping
/// dependency closures.
pub(crate) struct ScopeCache<'r> {
    slots: blaze_own::Vec<Option<(blaze_drive::Input, crate::front_load::CrateRecords<'r>)>, blaze_own::Alloc<'static>>,
}

impl<'r> ScopeCache<'r> {
    pub(crate) fn new(len: usize, alloc: blaze_own::Alloc<'static>) -> ScopeCache<'r> {
        let mut slots = blaze_own::Vec::with_capacity_in(len, alloc);
        slots.resize(len, None);
        ScopeCache { slots }
    }

    pub(crate) fn records_of(
        &mut self,
        read: &mut EngineRecords<'r>,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        names: NamesInputs<'_, '_>,
        list: &blaze_world::crate_list::CrateList<'static>,
        at: usize,
        g: blaze_own::Alloc<'static>,
    ) -> Result<(blaze_drive::Input, crate::front_load::CrateRecords<'r>), ScopeRefusal> {
        if let Some(cached) = self.slots.get(at).copied().flatten() {
            return Ok(cached);
        }
        let name = blaze_drive::key::crate_scope_name(&list.key(at));
        let krate = CrateGraph::name(list, at);
        let input = names.input_of(name).ok_or_else(|| ScopeRefusal::NoNamesRecord { krate, at, failed: names.failed(name) })?;
        let records = read.scope_records(cx, store, &input, list.key(at), krate, g).map_err(|error| ScopeRefusal::Records { krate, at, error })?;
        if let Some(slot) = self.slots.get_mut(at) {
            *slot = Some((input, records));
        }
        Ok((input, records))
    }
}

/// Crate `at`'s check scope (blaze-world check_scope.rs `check_scope_in`), reusing `cache` so
/// each crate's records are asked of `read` at most once per phase across all scopes built with
/// `cache`.
#[expect(clippy::too_many_arguments, reason = "a stage's handles, each named")]
pub(crate) fn scope_of_cached<'r>(
    cache: &mut ScopeCache<'r>,
    read: &mut EngineRecords<'r>,
    cx: &Cx,
    store: &mut dyn crate::store::Store<'r>,
    names: NamesInputs<'_, '_>,
    list: &blaze_world::crate_list::CrateList<'static>,
    at: usize,
    heap: &'static blaze_own::Heap,
) -> Result<(Scope<'r>, bool), ScopeRefusal> {
    let g = heap.alloc();
    let (_, own_records) = cache.records_of(read, cx, store, names, list, at, g)?;
    let no_core = blaze_scope::names_record::NamesRecord::open(own_records.names_proof).crate_root().head().no_core;
    let checked = blaze_world::check_scope::check_scope_in(list, at, no_core, g).map_err(|error| ScopeRefusal::CheckScope { error })?;
    let mut scope = Scope { own_records, crates: Vec::new_in(Global), lang: Vec::new_in(Global), scope: Vec::new_in(Global), cut: None };
    scope.lang.extend(checked.lang_places());
    let mut whole = true;
    for one in checked.crates() {
        let (input, records) = match cache.records_of(read, cx, store, names, list, one, g) {
            Ok(found) => found,
            Err(ScopeRefusal::NoNamesRecord { failed, .. }) => {
                whole = false;
                scope.cut = Some((one, CutWhy::NoNamesRecord { failed }));
                break;
            }
            Err(ScopeRefusal::Records { error, .. }) => {
                whole = false;
                scope.cut = Some((one, CutWhy::Unreadable(error)));
                break;
            }
            Err(ScopeRefusal::CheckScope { error }) => return Err(ScopeRefusal::CheckScope { error }),
        };
        scope.crates.push(Crate { records, name: CrateGraph::name(list, one), package: list.shown_from(one), world_at: one });
        scope.scope.push(input.name);
    }
    scope.scope.sort_unstable();
    Ok((scope, whole))
}

/// Crate `at`'s check scope (blaze-world check_scope.rs `check_scope_in`), each crate's
/// records read in place through `read` (engine_records.rs), and whether every crate of it had
/// records (`false`: the scope was cut, [`Scope::cut`] says where and why, and the caller refuses by
/// name). Refused: the crate's own names record or its scope could not be read, the cause typed
/// ([`ScopeRefusal`]); nothing is dropped (rule 3).
#[expect(clippy::too_many_arguments, reason = "a stage's handles, each named")]
pub(crate) fn scope_of<'r>(read: &mut EngineRecords<'r>, cx: &Cx, store: &mut dyn crate::store::Store<'r>, names: NamesInputs<'_, '_>, list: &blaze_world::crate_list::CrateList<'static>, at: usize, heap: &'static blaze_own::Heap) -> Result<(Scope<'r>, bool), ScopeRefusal> {
    let mut cache = ScopeCache::new(CrateGraph::len(list), heap.alloc());
    scope_of_cached(&mut cache, read, cx, store, names, list, at, heap)
}

/// Crate `at`'s check scope MADE BY NEED (piece C (a)) over its own records `own_records`, which
/// the caller read: the crates, their order, the lang places and the names as [`scope_of`] makes
/// them (blaze-world check_scope.rs `check_scope_in`), the member alone opened ([`Scope::crates`]
/// holds it alone); each crate past it found by its names input and checked present in the
/// committed `snapshot`, never opened ([`crate::front_load::present`]: one lookup, no byte of the
/// record read), returned unopened in reader order for the request's
/// [`LazyScopeRecords`]. When `defer_inputs` is true (a standing member visited below its heads
/// on the ready driver with no `CrateScope` entered this request and no planted unreadable crate),
/// its scope already stood whole in the members record, so each dependency crate's `names.input_of`
/// and snapshot lookup are deferred to its first actual question in [`LazyScopeRecords`], avoiding
/// reading ~900 unused `CrateScope` task pages upfront.
/// Cut where [`scope_of`] cuts, by the same causes: a crate
/// with no names input, a record not in the store or not in place, or the crate the tests plant
/// unreadable (`planted`, engine_records.rs `plant_unreadable`), read as [`scope_of`] reads it.
/// A record that is present and does not open is found at its first question and refuses the
/// bodies of the job that asked it (front_load.rs `StoreLoad::refused`). `Err`: the store's
/// snapshot refused a lookup, named with the member (rule 3).
#[expect(clippy::too_many_arguments, reason = "the scope builder's handles, each named")]
fn scope_by_need<'r>(
    names: NamesInputs<'_, '_>,
    list: &blaze_world::crate_list::CrateList<'static>,
    at: usize,
    own_records: crate::front_load::CrateRecords<'r>,
    snapshot: &StoreSnapshot,
    planted: Option<&'static str>,
    (defer_inputs, defer_present): (bool, bool),
    heap: &'static blaze_own::Heap,
) -> Result<Result<MadeScope<'r>, ScopeRefusal>, CliError> {
    let g = heap.alloc();
    let name_at = |at: usize| blaze_drive::key::crate_scope_name(&list.key(at));
    let krate = CrateGraph::name(list, at);
    if !defer_inputs && names.input_of(name_at(at)).is_none() {
        return Ok(Err(ScopeRefusal::NoNamesRecord { krate, at, failed: names.failed(name_at(at)) }));
    }
    let no_core = blaze_scope::names_record::NamesRecord::open(own_records.names_proof).crate_root().head().no_core;
    let checked = match blaze_world::check_scope::check_scope_in(list, at, no_core, g) {
        Ok(checked) => checked,
        Err(error) => return Ok(Err(ScopeRefusal::CheckScope { error })),
    };
    let mut scope = Scope { own_records, crates: Vec::new_in(Global), lang: Vec::new_in(Global), scope: Vec::new_in(Global), cut: None };
    scope.lang.extend(checked.lang_places());
    let mut rest: Vec<crate::front_load::Unopened, Global> = Vec::with_capacity_in(checked.readers().len().saturating_sub(1), Global);
    let mut whole = true;
    for one in checked.crates() {
        let scope_name = name_at(one);
        let name = CrateGraph::name(list, one);
        if one == at {
            // The owner, first of `check_scope_in`'s crates: reader crate 0, opened.
            scope.crates.push(Crate { records: own_records, name, package: list.shown_from(one), world_at: one });
            scope.scope.push(scope_name);
            continue;
        }
        if defer_inputs {
            // Standing member on the ready driver: its scope stood whole in the members record
            // and no `CrateScope` ran this request. Defer `names.input_of` and `snapshot.get`
            // to the first actual question of `one` (`LazyScopeRecords`).
            rest.push(crate::front_load::Unopened {
                input: Input { name: scope_name, at: Key::default(), out: ABSENT },
                key: list.key(one),
                world_at: one,
                name,
                package: list.shown_from(one),
            });
            scope.scope.push(scope_name);
            continue;
        }
        let Some(input) = names.input_of(scope_name) else {
            whole = false;
            scope.cut = Some((one, CutWhy::NoNamesRecord { failed: names.failed(scope_name) }));
            break;
        };
        let unopened = crate::front_load::Unopened { input, key: list.key(one), world_at: one, name, package: list.shown_from(one) };
        if !defer_present {
            let present: Result<(), crate::engine_records::ReadError> = if planted == Some(name) {
                Err(crate::front_load::LoadError::NotInStore { name: input.name, at: input.at }.into())
            } else {
                crate::front_load::present(snapshot, &unopened).map_err(|error| CliError::Text {
                    what: crate::engine_head_notes::leak(g, format_args!("`{}`: its check scope's crate `{name}` does not read from the store's snapshot: {error:?} (front_load.rs `present`)", list.root(at))),
                })?
            };
            if let Err(error) = present {
                whole = false;
                scope.cut = Some((one, CutWhy::Unreadable(error)));
                break;
            }
        }
        rest.push(unopened);
        scope.scope.push(input.name);
    }
    scope.scope.sort_unstable();
    Ok(Ok((scope, Some(rest), whole)))
}

/// A member's check scope as `groups` takes it: the scope, its crates past the member left
/// unopened (`Some`: made by need, [`scope_by_need`]; `None`: every crate opened, [`scope_of`]),
/// and whether it is whole (`false`: cut, [`Scope::cut`]).
type MadeScope<'r> = (Scope<'r>, Option<Vec<crate::front_load::Unopened, Global>>, bool);

/// Each outlined file's bodies by name with their labels (or the words of why its text could
/// not be read again), as [`planned_label`] makes them: in the request's frame.
type Labels<'f> = Vec<(u32, Result<Vec<(Name, &'f str), &'f Frame>, &'f str>), &'f Frame>;

/// The label body `name` of outline `file` prints when it was refused before any records named
/// it (a member's scope not read, its file claimed by no unit): the label a checked body prints
/// (`groups`: [`crate::item_name::shown_name`] of its outline item, or the macro call it is),
/// made from the file's text outlined again under its part's edition, once a file (`labels`).
/// Only printed, never keyed (rule 71). A file whose text no longer outlines to the body
/// prints "" ([`crate::engine_body_plan::Named`] then names its place); one that cannot be read
/// prints why, in braces as a label that is not a name.
fn planned_label<'f>(labels: &mut Labels<'f>, cx: &Cx, root: &Dir<ReadWrite>, outlined: &[crate::engine_tasks::Outlined<'_>], file: u32, name: Name, frame: &'f Frame) -> &'f str {
    let at = match labels.iter().position(|(one, _)| *one == file) {
        Some(at) => at,
        None => {
            let made = file_labels(cx, root, outlined, file, frame).map_err(|lost| {
                let words = bstd::mem::format_in!(frame, "{{a body of a file whose text could not be read again to name it: {lost}}}");
                &*words.into_bump_str()
            });
            labels.push((file, made));
            labels.len() - 1
        }
    };
    match &labels[at].1 {
        Ok(bodies) => bodies.iter().find(|(one, _)| *one == name).map_or("", |(_, label)| *label),
        Err(words) => words,
    }
}

/// Why a file's text could not be read again for its bodies' labels ([`planned_label`]).
#[derive(Debug)]
enum LabelLost {
    /// No outline of that index ([`crate::engine_tasks::Outlined`]).
    NoOutline { file: u32 },
    Path(bstd::path::PathError),
    Read(bstd::fs::FsError),
    NotText,
    NoEdition { byte: u8 },
}

impl core::fmt::Display for LabelLost {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LabelLost::NoOutline { file } => write!(f, "outline {file} is not among the planner's"),
            LabelLost::Path(error) => write!(f, "its path: {error}"),
            LabelLost::Read(error) => write!(f, "{error}"),
            LabelLost::NotText => f.write_str("its bytes are not UTF-8"),
            LabelLost::NoEdition { byte } => write!(f, "its edition byte {byte} is no edition"),
        }
    }
}

/// Every body of outline `file` by its name ([`work::body_name`]), each labelled as `groups`
/// labels a checked one.
fn file_labels<'f>(cx: &Cx, root: &Dir<ReadWrite>, outlined: &[crate::engine_tasks::Outlined<'_>], file: u32, frame: &'f Frame) -> Result<Vec<(Name, &'f str), &'f Frame>, LabelLost> {
    let one = outlined.get(file as usize).ok_or(LabelLost::NoOutline { file })?;
    let path = core::str::from_utf8(&one.path).map_err(|_| LabelLost::NotText)?;
    let mut bytes = bstd::mem::Bytes::new_in(frame, "a source file whose refused bodies are named");
    root.read_all(cx, &bstd::path::Path::new(path).map_err(LabelLost::Path)?, &mut bytes).map_err(LabelLost::Read)?;
    let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| LabelLost::NotText)?;
    let edition = blaze_surface::Edition::from_byte(one.edition).ok_or(LabelLost::NoEdition { byte: one.edition })?;
    let erased = blaze_own::Erased::new(frame);
    let outline = blaze_file_items::outline(text, edition, erased.handle());
    let mut found = Vec::new_in(frame);
    for item in outline.items().iter() {
        if work::checked_body(&outline, item).is_none() && !work::expansion_entry(&outline, item, text) {
            continue;
        }
        let shown = crate::item_name::shown_name(item, text);
        let label = match shown.is_empty() {
            false => bstd::mem::format_in!(frame, "{shown}"),
            true => bstd::mem::format_in!(frame, "{{a macro call}}"),
        };
        let label: &'f str = label.into_bump_str();
        found.push((work::body_name(one.name, &outline, item, text), label));
    }
    Ok(found)
}

/// Body `one`'s subject (D164, rule 19): its file's checkout path and its item as written, or
/// its place in its file's outline, both in `frame`. A file that is none of the planner's
/// outlines or a path that is not UTF-8 is a typed stop naming the body, never a "?".
/// `base`: the body's base name (its outline part's, D191), which its outline is asked by: a
/// row at a unit's view (`name`) is no part of the outline, and every row of one body names the
/// same item, so the printer's groups count the body once (engine_lines.rs `grouped_lines`).
fn body_subject<'p>(one: &Ran<'_>, name: Name, base: Name, outlined: &[Outlined<'_>], frame: &'p Frame) -> Result<blaze_report::finding::Subject<'p>, EngineCheckError> {
    let file = outlined.get(one.file as usize).ok_or(EngineCheckError::Unplaced { body: name, what: "its file is none of the planner's outlines" })?;
    let path = core::str::from_utf8(&file.path).map_err(|_| EngineCheckError::Unplaced { body: name, what: "its file's checkout path is not UTF-8" })?;
    let item = bstd::mem::format_in!(frame, "{}", crate::engine_body_plan::Named::of(one.label, outlined, one.file, base));
    let item: &'p str = item.into_bump_str();
    Ok(blaze_report::finding::Subject::Item { file: work::held_str(path, frame), item })
}

/// The rule a body's note says it was not finished by (`None`: no note, so `checked` decides): an
/// `Unchecked` note is `TYPES_NOT_CHECKED`, a `BorrowsUnchecked` one `BORROWS_NOT_CHECKED`, and
/// every refusal, panic, frame refusal and task fault `work::NOT_RUN` (those notes carry no rule
/// number of their own). A body with no note that did not run is `NOT_RUN` too.
fn note_rule(said: Option<&Said<&str>>, checked: bool) -> Option<u16> {
    match said {
        Some(Said::Unchecked { .. }) => Some(crate::body_types::TYPES_NOT_CHECKED),
        Some(Said::BorrowsUnchecked(_)) => Some(crate::body_types::BORROWS_NOT_CHECKED),
        Some(Said::Task(_) | Said::Panicked | Said::FrameUsedUp { .. } | Said::Refused(_) | Said::RefusedAtPlan(_)) => Some(work::NOT_RUN),
        None if !checked => Some(work::NOT_RUN),
        None => None,
    }
}

/// The run facts records' payloads of the bodies that ran in a run (engine_body_facts.rs), made at
/// the merge from the run's `One`s. They live in the PHASE's arena, handed back by
/// [`BodiesStage::run_phase`] to the caller, which drops them after that phase's put: not a field
/// of a body's row (`Ran`) nor of the stage, and nothing of them is in the request's frame.
pub(crate) struct MadeFacts<'p> {
    bytes: SVec<'p, u8>,
    /// (name, from, to) in `bytes`, sorted by name.
    index: SVec<'p, (Name, u32, u32)>,
}

/// One body a run checks.
#[derive(Clone, Copy)]
struct One<'f> {
    task: Task,
    /// The bodies it checks: one, or each an expansion at its entry wrote (none: a clean pass).
    items: &'f [ItemKey],
    group: u32,
    file: u32,
    label: &'f str,
    place: Place<'f>,
    /// The cells that say the units ranked before its checker do not hold it (engine_body_plan.rs
    /// `Holders::cells`): in its reads, so a unit that comes to hold it enters it again.
    held: &'f [CellId],
    /// `Some`: one slice of an entry predicted past [`SLICE_BYTES`], its `items`, `costs` and
    /// `place.written` that slice's ([`Part`]); `None`: the whole entry.
    part: Option<Part>,
    /// Each body's predicted cost, one an item ([`text_bytes`]): what a run and a slice are cut
    /// by ([`SLICE_BYTES`]).
    costs: &'f [u32],
    /// Each body's checker key as a program calls it, one an item in its order ([`asked_keys`]):
    /// its run facts' covers.
    asked: &'f [blaze_typeck::ItemKey],
}

/// Where a slice's bodies are in its entry's: from item `from`, of `whole` items. A slice's
/// result is joined into its entry's after the join ([`join_slices`]); while it runs, its
/// verdict is never the entry's (an entry of more than one body has the merged digest, even
/// in a slice of one).
#[derive(Clone, Copy)]
struct Part {
    from: u32,
    whole: u32,
}

impl One<'_> {
    fn path<'o>(&self, outlined: &'o [Outlined<'_>]) -> &'o [u8] {
        // Checked when the coordinator creates this run entry.
        &outlined[self.file as usize].path
    }
}

/// The phase-independent handles the Bodies phase reads.
pub(crate) struct Handles<'h, 'e, 'n, 'o> {
    pub(crate) root: &'h Dir<ReadWrite>,
    pub(crate) env: &'h FetchEnv<'e>,
    /// The `UnitGraph`'s record as a reader takes it; `None`: the command has no graph.
    pub(crate) graph: Option<Input>,
    /// Each crate's names input, by the one seam (engine_names.rs `NamesInputs`): the stage's
    /// entries on the phase path, and the standing rows beside them on the ready driver's.
    pub(crate) names: Option<NamesInputs<'h, 'n>>,
    pub(crate) outlined: &'h [Outlined<'o>],
    pub(crate) workers: NonZeroU32,
    pub(crate) clock: &'h Clock,
    /// Where each body says it runs (engine_status.rs); a stop refuses the bodies not begun.
    pub(crate) board: &'h crate::engine_status::Board<'h>,
    /// The `BodyCheck` kind's tool (engine_store.rs `tools`): in each member listing row's key
    /// (engine_member_rows.rs `row_key`), as it is in every body task's.
    pub(crate) tool: blaze_drive::Tool,
}

/// Why the Bodies stage checks no body this command, and the cells that stop is a function of
/// (plan row A8; none: owed), from [`BodiesStage::groups`].
type Stop<'f> = (&'static str, &'f [CellId]);

/// The on-demand packs of the crate list's trees and its corrected files (body_types.rs `Kept::lazy_trees`).
type Trees<'f> = (blaze_own::Box<crate::body_types::SyncLazyTrees<'f>, blaze_own::Alloc<'static>>, &'static [(u32, &'static str, &'static str)]);

/// What [`BodiesStage::groups`] found: each member's group, each body's place, the files of
/// member crates, and the members' names.
struct Found<'f, 'r> {
    grains: ReadGrains<'static>,
    groups: Vec<Group<'f, 'r>, Global>,
    /// One a body a unit's records found: in the request's heap, never the command's fixed
    /// frame; in the units' order, indexed by `holders`' decisions.
    took: Vec<Took<'f>, blaze_own::Alloc<'static>>,
    /// Every member unit's word on the files its module tree reaches and the bodies its records
    /// hold: who checks each body (engine_body_plan.rs).
    holders: crate::engine_body_plan::Holders<'f>,
    members: Vec<Name, Global>,
    /// Every member crate's names cell (`WHOLE`), with a `Names` task or without, sorted.
    member_cells: Vec<CellId, Global>,
    /// Bodies a macro wrote that no entry covers (covered.rs `written_by` gave no file entry, and
    /// `in_blocks` no block), summed over the members' listing rows: checked by no task, said in
    /// the profile.
    uncovered: u32,
    /// Each member crate's outlined files by its key's prefix and its items record's file
    /// index, sorted: where a head read finds its head task (engine_heads.rs `Owners`, [`widen`]).
    heads: SVec<'f, crate::engine_heads::Subject>,
    trees: Trees<'f>,
}

/// The stage: the crates' records read once a command, and every body's result.
pub(crate) struct BodiesStage<'f> {
    /// The records read so far, shared with the Heads stage (engine_heads.rs).
    pub(crate) kept: Kept,
    /// Every crate's impl tables, put by the Heads stage (engine_heads.rs) and read only from
    /// the Bodies phase on: each run lends it (`TableSet::lent`).
    pub(crate) shelf: ImplShelf,
    /// Every body entered, sorted by name after its phase.
    pub(crate) ran: Vec<Ran<'f>, Global>,
    /// All runs append in order. The frame reserves space for a phase's findings in one go.
    findings: Option<Vec<BodyFinding<'f>, &'f Frame>>,
    /// Each checkout file a body read as data through its `Given` cell, with the answer it saw
    /// (`RunOut::data_files`), over every run: [`BodiesStage::data_files`]. `None`: none.
    data_files: Option<Vec<(&'f str, ContentId), &'f Frame>>,
    /// Rows with no note or findings (including rows the plan left outside).
    clean: u32,
    /// Item bodies checked and tasks with at least one item body.
    items: u64,
    item_tasks: u64,
    /// Bodies of files no member crate of the graph holds: checked by no crate, counted.
    pub(crate) ownerless: u32,
    /// Their files, by outline index, sorted and once each.
    ownerless_files: Vec<u32, Global>,
    /// The phase's wall time, ns, and the runs it was cut into.
    pub(crate) wall_ns: u64,
    pub(crate) runs: u32,
    /// Bodies a macro wrote that no entry covers (`Found::uncovered`), said in the profile.
    uncovered: u32,
    /// Their note's words, the first named by label ([`MOST_UNCOVERED_NAMED`], [`Uncovered`]):
    /// the request's lines say them ([`BodiesStage::plan_lines`]), so none is left out in
    /// silence (rule 3). `None`: every body a macro wrote is covered.
    uncovered_note: Option<&'static str>,
    /// Whether this request's phase found the members' records (`groups` ran whole): else the
    /// uncovered note stands from the last request's lines.
    groups_found: bool,
    /// The most a body's scratch frame held when its body ended, over every run ([`RunOut`]).
    scratch_most: u64,
    /// What every run's scratch and stretch frames held committed ([`FramesHeld`]).
    frames: FramesHeld,
    /// The bodies checked from the request's frozen heads, refused at a seed, and their rounds
    /// (engine_seeds.rs `SeedTally`), summed over every run.
    pub(crate) seeds: crate::engine_seeds::SeedTally,
    /// Every head not frozen, this request's and the standing head tasks' (engine_head_notes.rs),
    /// set before the Bodies phase: a body that reads one names it and quotes why.
    pub(crate) heads_failed: crate::engine_head_notes::Failed,
    /// Bodies no unit holds (engine_body_plan.rs): no unit's, never refused; a file's note once.
    outside: crate::engine_body_plan::Outside,
    /// How a body's names reads are kept (engine_scope_reads.rs), the command's choice.
    scope_reads: crate::engine_scope_reads::ScopeReadsAs,
    /// Expansion-text record demands through this phase's request snapshot.
    pub(crate) expansion_demands: u64,
    /// Distinct expansion-text record keys demanded through this phase's snapshot.
    pub(crate) expansion_keys: u64,
    /// What `groups` opened, listed and keyed against what the phase owed ([`OwedListing`]),
    /// summed over the request's phases: the profile's floor line (rule 67).
    owed_listing: OwedListing,
    /// The key of each member listing row this request read or put (engine_member_rows.rs),
    /// in the request's heap: what the answer's root names so the next command finds them
    /// ([`BodiesStage::member_row_keys`]). `None`: no row yet.
    member_rows: Option<Vec<Key, blaze_own::Alloc<'static>>>,
    /// The typed records this request put (blaze-typeck checked_record.rs), each under
    /// `engine_body_record::typed_key(bytes)`: the keys, in the request's heap, for the
    /// answer's root to name ([`BodiesStage::typed_record_keys`]), and their count and bytes and
    /// the clean bodies that made none, for the profile's `bodies: typed records` line.
    typed: Option<Vec<Key, blaze_own::Alloc<'static>>>,
    typed_bytes: u64,
    untyped_clean: u32,
    /// Every stretch's setup before its first body ([`StretchSetup`]), summed over the phases.
    setup: StretchSetup,
    /// Each slice's task as the profile names it ([`Part`], [`BodiesStage::task_label`]): its
    /// time's name (its first body's), its entry's label, and its bodies' place in the entry.
    /// `None`: no entry was sliced.
    slices: Option<Vec<SliceLabel<'f>, &'f Frame>>,
    /// Each run's predicted cost against its thread time ([`SLICE_BYTES`]), for the profile's
    /// `bodies cost:` line.
    cost_fit: CostFit,
    /// What the read grains are taken from (engine_scope_reads.rs `GrainsIn`), set by the
    /// driver before the first Bodies phase ([`BodiesStage::grains_from`]): the last request's
    /// grains record and the serial of the answer this request starts from. `None`: made whole.
    grains_last: Option<(Option<Key>, u64)>,
    /// Every `CrateScope` and `ImplBucket` task this request entered so far, sorted, in the
    /// request's heap ([`BodiesStage::note_entered`]): the crates whose grain facts are read
    /// again. `None`: none entered yet.
    grains_entered: Option<Vec<Name, blaze_own::Alloc<'static>>>,
    /// The grains record this request put, for the driver to name under the checkout's grains
    /// root ([`BodiesStage::grains_record`]).
    grains_key: Option<Key>,
    /// The profile's `bodies: read grains` line, summed over the phases.
    grains_line: Option<crate::engine_scope_reads::GrainsLine>,
    /// The last request's members record (engine_members_record.rs) and the serial of the
    /// answer this request starts from, set by the driver before the first Bodies phase
    /// ([`BodiesStage::members_from`]). `None`: none named.
    members_last: Option<(Option<Key>, u64)>,
    /// The outlines this request moved (`FileItems` names, sorted, each once), set before its
    /// setup ([`BodiesStage::stand_members`]): its setup then takes a member none of whose files
    /// moved from the record ([`StandingMembers`]). `None`: every member visited ([`EveryMember`]).
    outlines_moved: Option<Vec<Name, blaze_own::Alloc<'static>>>,
    /// The `FileItems` tasks entered in earlier phases of this request (`note_entered`), sorted.
    outlines_entered: Option<Vec<Name, blaze_own::Alloc<'static>>>,
    /// The files the request has an outline of (a place in the stored answer's root) and did not
    /// read back (the ready setup's, sorted: [`BodiesStage::unread_outlines`]). A visited
    /// member's held file among them is the setup's typed refusal (`groups`), never a
    /// `NotOutlined` head. `None`: every outline was read (the phase path).
    unread: Option<Vec<Name, blaze_own::Alloc<'static>>>,
    /// Full-file `FILE_DOMAIN` digests from the request's initial sweep for unread `/0/` checkout
    /// files, sorted by path: verifies lazy disk reads stay on the request snapshot.
    unread_digests: Option<blaze_own::Arc<blaze_own::Vec<(blaze_own::String<SharedAlloc<'static>>, ContentId), SharedAlloc<'static>>, SharedAlloc<'static>>>,
    /// The members record this request put, for the driver to name under the checkout's
    /// members root ([`BodiesStage::members_record`]).
    members_key: Option<Key>,
    /// The planted control of the standing setup (engine_ready_edit.rs
    /// `CommitPlant::StandingDropsHeads`): a standing member's head subjects are not taken.
    standing_drops_heads: bool,
    /// The planted control of the members' row key (engine_ready_edit.rs
    /// `CommitPlant::RowKeyUnitOnly`): each row keyed by its unit and the tool alone.
    row_key_unit_only: bool,
    /// How the standing setup takes a member whose moved files kept their listing facts (piece
    /// A, [`Deferral`]): set with the moved outlines ([`BodiesStage::stand_members`]).
    deferral: Deferral,
    /// THE PLAN'S REACH (D191, engine_body_plan.rs [`Reach`]): which member units reach each
    /// file, made once a request by [`BodiesStage::plan_reach`] for the plan, and read by every
    /// Bodies phase's setup, which names each task's row from it as the plan did.
    reach: Option<Reach>,
    /// The entered rows of bodies two units or more reach (sorted by name, each once), kept
    /// across the request's Bodies phases for its lines ([`BodiesStage::lines`]). On the
    /// process heap, made by the first phase that has a reach.
    shared_rows: Option<blaze_own::AVec<'static, TaskRow>>,
    /// The crate (by name) the tests plant unreadable (engine_check.rs `Opts::unreadable`, as
    /// engine_records.rs `plant_unreadable` reads it for [`scope_of`]): a scope made by need
    /// ([`scope_by_need`]) cuts at it as [`scope_of`] does. The command's is `None`.
    unreadable: Option<&'static str>,
    /// What the request's scope records opened by need did, summed over the runs that read
    /// them ([`crate::front_load::ScopeRecords`]): the profile's by-need line.
    by_need: crate::front_load::ByNeedCounts,
}

/// Runs' predicted cost (bytes of their bodies' text) against their thread time: the ms a
/// byte that states [`SLICE_BYTES`] in ms, and the run that strays most from it.
#[derive(Clone, Copy, Default)]
struct CostFit {
    runs: u64,
    bytes: u64,
    ns: u64,
    /// The longest run: its thread time and its predicted bytes.
    longest: (u64, u64),
    /// The run with the most ns a byte: its thread time and bytes.
    densest: (u64, u64),
}

impl CostFit {
    fn add(&mut self, bytes: u64, ns: u64) {
        self.runs += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        self.ns = self.ns.saturating_add(ns);
        if ns > self.longest.0 {
            self.longest = (ns, bytes);
        }
        // ns / bytes compared without dividing: a / b > c / d is a * d > c * b.
        if u128::from(ns) * u128::from(self.densest.1.max(1)) > u128::from(self.densest.0) * u128::from(bytes.max(1)) {
            self.densest = (ns, bytes);
        }
    }
}

/// A slice's task in the profile: `<entry label> (bodies <from>..<to> of <whole>)`.
struct SliceLabel<'f> {
    name: Name,
    entry: &'f str,
    from: u32,
    to: u32,
    whole: u32,
}

/// What [`BodiesStage::groups`] did against what its phase owed (rule 67: the floor and the
/// ratio, in the profile). A file is OWED when an entered `BodyCheck` task's body is in it
/// (`Task::planned.file`, run_phase's `owed`); a member unit is owed when it holds an owed file.
/// `Holders::decide` (engine_body_plan.rs) asks only the claims of an entered task's file, so
/// an owed file's entries are all a phase's decisions read.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct OwedListing {
    /// Member units whose records were opened (run_names.rs `names_records_with_snapshot`):
    /// those holding an owed file, and those whose listing row was not in the store.
    members_opened: u32,
    /// Member units of the crate list.
    members: u32,
    /// Member units holding an owed file: the floor of `members_opened`.
    members_owed: u32,
    /// (unit, file) pairs whose entries were outlined and keyed by chain (`index_by_chain`).
    files_listed: u32,
    /// (unit, file) pairs the members' items records hold.
    files: u32,
    /// (unit, file) pairs of owed files a unit holds: the floor of `files_listed`.
    files_owed: u32,
    /// Listed bodies keyed by (file, chain) in the listed files.
    entries_keyed: u64,
    /// Members' listing rows read by key, made from a whole listing, or found damaged
    /// (engine_member_rows.rs): after an edit, made is the members holding an edited file (their
    /// files' texts are in the key) and read is every other member.
    rows: crate::engine_member_rows::RowCounts,
    /// D168 (2): members whose check scope was opened (every scope crate's names record asked
    /// of the request's reader, `scope_of`), and the members that needed it ([`OpenWorld`]: an
    /// owed file, a row to make, a scope cut at an input): its floor. Under [`Whole`] every
    /// member is opened.
    scopes_opened: u32,
    scopes_needed: u32,
    /// What `groups` asked of the request's reader (engine_records.rs `ReadCounts`): names
    /// records opened (their bytes) and asks answered by a record opened before (repeat asks,
    /// floor 0: a lookup made once per member and crate, then memoized).
    records_opened: u64,
    record_bytes: u64,
    repeat_asks: u64,
    /// The wall of `groups` (ns): the setup before the first body runs.
    wall_ns: u64,
    /// Members taken from the last request's members record, unvisited ([`StandingMembers`]),
    /// and the members that record held.
    members_stood: u32,
    members_recorded: u32,
    /// Of `members_stood`, those a moved file is in that stood below their heads (piece A,
    /// [`stands_below`]); and the members stood below their heads that an entered row of their
    /// own needed visited ([`own_row_entered`]: counted visited, not stood).
    members_below: u32,
    members_needed: u32,
    /// Stored members records that did not open (another version, damage): made again.
    members_record_refused: u32,
    /// Where `wall_ns` went, by part ([`SetupParts`]).
    parts: SetupParts,
}

/// WHERE THE MEMBERS' SETUP SPENT ITS WALL ([`BodiesStage::groups`]), by part, in ns: the
/// profile's `members' setup by part` line, so the setup's cost is named by the profile before
/// any claim of why it is slow (rule 17). Each part is a lap of the setup's clock
/// ([`lap`]); what is left of `wall_ns` past their sum is printed as `rest`.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct SetupParts {
    /// From the setup's start: the crate list read and every tree's pack opened (body_types.rs
    /// `Kept::list`, `Kept::trees`).
    list_and_trees_ns: u64,
    /// The read grains made and put (engine_scope_reads.rs `ReadGrains`).
    grains_ns: u64,
    /// The last request's members record read and opened (engine_members_record.rs `View`).
    members_record_ns: u64,
    /// The members taken from it, unvisited ([`stands`]).
    stood_ns: u64,
    /// The visited members' own names records read and checked (engine_records.rs
    /// `scope_records`, front_load.rs `checked_records`), or their scopes opened whole.
    own_records_ns: u64,
    /// The visited members' environments (body_types.rs `owner_env_by`).
    env_ns: u64,
    /// The visited members' items records opened again, their file rows matched to the
    /// outlines and their listing rows probed.
    files_ns: u64,
    /// The visited members' check scopes made by need (`scope_inputs_whole`, `scope_by_need`).
    scope_ns: u64,
    /// The visited members' records opened for their listing (run_names.rs
    /// `names_records_over`), their rows read or made.
    opened_ns: u64,
    /// The owed files' bodies listed and keyed by chain.
    listing_ns: u64,
    /// Of `listing_ns` (not in the sum): the members' bodies listed from their records (front2
    /// project_records.rs `list_files_with`, which walks every item of the crate), and the owed
    /// files' runs keyed by chain (`index_outlined_by_chain`, in the walk's one outline of the
    /// file); the rest is the walk of the file's entries (`with_outline`: the file's one outline,
    /// and each entered body's keys, label and cost).
    listing_list_ns: u64,
    listing_chains_ns: u64,
    /// This request's members record written and put.
    put_ns: u64,
}

impl SetupParts {
    fn add(&mut self, other: SetupParts) {
        self.list_and_trees_ns = self.list_and_trees_ns.saturating_add(other.list_and_trees_ns);
        self.grains_ns = self.grains_ns.saturating_add(other.grains_ns);
        self.members_record_ns = self.members_record_ns.saturating_add(other.members_record_ns);
        self.stood_ns = self.stood_ns.saturating_add(other.stood_ns);
        self.own_records_ns = self.own_records_ns.saturating_add(other.own_records_ns);
        self.env_ns = self.env_ns.saturating_add(other.env_ns);
        self.files_ns = self.files_ns.saturating_add(other.files_ns);
        self.scope_ns = self.scope_ns.saturating_add(other.scope_ns);
        self.opened_ns = self.opened_ns.saturating_add(other.opened_ns);
        self.listing_ns = self.listing_ns.saturating_add(other.listing_ns);
        self.listing_list_ns = self.listing_list_ns.saturating_add(other.listing_list_ns);
        self.listing_chains_ns = self.listing_chains_ns.saturating_add(other.listing_chains_ns);
        self.put_ns = self.put_ns.saturating_add(other.put_ns);
    }

    fn sum(&self) -> u64 {
        [self.list_and_trees_ns, self.grains_ns, self.members_record_ns, self.stood_ns, self.own_records_ns, self.env_ns, self.files_ns, self.scope_ns, self.opened_ns, self.listing_ns, self.put_ns].iter().fold(0u64, |sum, one| sum.saturating_add(*one))
    }
}

/// The ns from `*mark` to now, `*mark` moved to now: one lap of [`SetupParts`].
fn lap(clock: &Clock, mark: &mut bstd::clock::Instant) -> u64 {
    let now = clock.now();
    let ns = ns_between(*mark, now);
    *mark = now;
    ns
}

impl OwedListing {
    fn add(&mut self, other: OwedListing) {
        self.parts.add(other.parts);
        self.members_stood = self.members_stood.saturating_add(other.members_stood);
        self.members_recorded = self.members_recorded.saturating_add(other.members_recorded);
        self.members_below = self.members_below.saturating_add(other.members_below);
        self.members_needed = self.members_needed.saturating_add(other.members_needed);
        self.members_record_refused = self.members_record_refused.saturating_add(other.members_record_refused);
        self.scopes_opened = self.scopes_opened.saturating_add(other.scopes_opened);
        self.scopes_needed = self.scopes_needed.saturating_add(other.scopes_needed);
        self.records_opened = self.records_opened.saturating_add(other.records_opened);
        self.record_bytes = self.record_bytes.saturating_add(other.record_bytes);
        self.repeat_asks = self.repeat_asks.saturating_add(other.repeat_asks);
        self.wall_ns = self.wall_ns.saturating_add(other.wall_ns);
        self.rows.add(other.rows);
        self.members_opened = self.members_opened.saturating_add(other.members_opened);
        self.members = self.members.saturating_add(other.members);
        self.members_owed = self.members_owed.saturating_add(other.members_owed);
        self.files_listed = self.files_listed.saturating_add(other.files_listed);
        self.files = self.files.saturating_add(other.files);
        self.files_owed = self.files_owed.saturating_add(other.files_owed);
        self.entries_keyed = self.entries_keyed.saturating_add(other.entries_keyed);
    }
}

/// One packed or unread dependency file's outlined `FileItemsView` bytes, cached once per phase
/// so stretches do not re-lex, re-outline, or re-digest immutable dependency files and never leak
/// temporary `Outline` ASTs into a stretch's bump frame.
struct CachedDepFile {
    world_at: usize,
    index: u32,
    edition: u8,
    text_len: usize,
    text_digest: ContentId,
    surface: ContentId,
    bytes: Vec<u8, SharedAlloc<'static>>,
}

/// Power-of-two slot count for [`DependencyFiles`]'s lock-free direct-mapped cache.
const DEP_FILE_SLOTS: usize = 512;

/// Lock-free request-wide cache of outlined dependency files (owned by [`Prepared`], dropped with
/// the phase): uses `PublishOnce` slots allocated on first miss so zero locks are constructed.
struct DependencyFiles {
    slots: bstd::sync::PublishOnce<Vec<bstd::sync::PublishOnce<CachedDepFile>, SharedAlloc<'static>>>,
    heap: &'static blaze_own::Heap,
}

impl DependencyFiles {
    fn new(heap: &'static blaze_own::Heap) -> DependencyFiles {
        DependencyFiles { slots: bstd::sync::PublishOnce::new(), heap }
    }

    fn slot_index(world_at: usize, index: u32, edition: u8) -> usize {
        let idx = usize::try_from(index).unwrap_or(0);
        let h = world_at.wrapping_mul(0x9E37_79B9) ^ idx.wrapping_mul(0x85EB_CA6B) ^ usize::from(edition);
        h & (DEP_FILE_SLOTS - 1)
    }

    fn get_or_outline<'n>(
        &self,
        world_at: usize,
        index: u32,
        text: &'n str,
        digest: ContentId,
        edition: blaze_surface::Edition,
        bump: blaze_own::Bump<'n>,
    ) -> Result<blaze_scope::items_record::CurrentFile<'n>, blaze_scope::items_record::ItemsRecordError> {
        let ed = edition.byte();
        let at = Self::slot_index(world_at, index, ed);
        if let Some(slots) = self.slots.get()
            && let Some(cached) = slots.get(at).and_then(|slot| slot.get())
            && cached.world_at == world_at
            && cached.index == index
            && cached.edition == ed
            && cached.text_len == text.len()
            && cached.text_digest == digest
        {
            let view = blaze_surface::FileItemsView::read(blaze_scope::resolve::aligned(&cached.bytes, bump))
                .map_err(|why| blaze_scope::items_record::ItemsRecordError::FileItemsRead { file: index, why })?;
            return Ok(blaze_scope::items_record::CurrentFile { view, text, surface: cached.surface });
        }
        let alloc = self.heap.alloc();
        let outline = blaze_file_items::outline(text, edition, alloc);
        let raw = outline.to_file_items(text, digest, alloc)
            .map_err(|why| blaze_scope::items_record::ItemsRecordError::FileItemsBuild { file: index, why })?;
        let surface = outline.surface();
        let mut bytes: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(raw.len(), self.heap.shared());
        bytes.extend_from_slice(&raw);
        let view = blaze_surface::FileItemsView::read(blaze_scope::resolve::aligned(&bytes, bump))
            .map_err(|why| blaze_scope::items_record::ItemsRecordError::FileItemsRead { file: index, why })?;
        let slots = match self.slots.get() {
            Some(slots) => Some(slots),
            None => {
                let table = Vec::from_iter_in((0..DEP_FILE_SLOTS).map(|_| bstd::sync::PublishOnce::new()), self.heap.shared());
                let _ = self.slots.set(table);
                self.slots.get()
            }
        };
        if let Some(slot) = slots.and_then(|slots| slots.get(at)) {
            let _ = slot.set(CachedDepFile { world_at, index, edition: ed, text_len: text.len(), text_digest: digest, surface, bytes });
        }
        Ok(blaze_scope::items_record::CurrentFile { view, text, surface })
    }
}

/// Whether `outlines` (sorted by slot `Name`) holds the planned outline for checkout path `real`
/// (either at its literal path or at its `x.rs` module slot for `x/mod.rs`).
fn outline_exists(outlines: &[Outlined<'_>], real: &str, alloc: blaze_own::Alloc<'_>) -> bool {
    let locate = |slot: Name| {
        outlines
            .binary_search_by_key(&slot, |one| one.name)
            .ok()
            .and_then(|i| outlines.get(i))
            .is_some_and(|one| one.path.as_slice() == real.as_bytes())
    };
    if locate(Name::file(real.as_bytes())) {
        return true;
    }
    let Some(stem) = real.strip_suffix("/mod.rs") else { return false };
    let mut slot = blaze_own::AVec::from_slice_in(stem.as_bytes(), alloc);
    slot.extend_from_slice(b".rs");
    locate(Name::file(&slot))
}

/// Stretch-local [`blaze_ladder::front2::source::Load`] over [`crate::front_load::StoreLoad`]:
/// reuses each crate's already-verified `CrateRecords::names_proof` (avoiding repeat
/// `Verified::check` per stretch), answers `proc_macro` directly from the crate graph (avoiding
/// opening unused dependency `CrateScope` records during impl lookup), uses planned outlines
/// whenever present, and caches outlined packed/unread dependency files across stretches.
struct BodiesLoad<'n> {
    inner: &'n crate::front_load::StoreLoad<'n>,
    crates: crate::front_load::CratesFrom<'n>,
    texts: &'n crate::job_texts::JobTexts<'n>,
    world_ats: &'n [usize],
    packages: &'n [&'n str],
    list: &'n blaze_world::crate_list::CrateList<'static>,
    outlines: &'n [Outlined<'n>],
    has_unread_outlines: bool,
    unread_digests: Option<&'n [(blaze_own::String<SharedAlloc<'static>>, ContentId)]>,
    dep_files: &'n DependencyFiles,
    alloc: blaze_own::Alloc<'n>,
}

impl<'n> blaze_ladder::front2::source::Load<'n> for BodiesLoad<'n> {
    fn names(&self, krate: usize) -> Option<&'n [u8]> {
        self.inner.names(krate)
    }

    fn names_verified(&self, krate: usize) -> Option<blaze_scope::names_record::Verified<'n>> {
        // On `Err`, return `None` so `VerifiedNames` falls back to `self.names(krate)`, recording
        // `StoreLoad::refused` and logging `SourceError::NoRecord`.
        self.crates.records(krate)?.ok().map(|records| records.names_proof)
    }

    fn items(&self, krate: usize) -> Option<&'n [u8]> {
        self.inner.items(krate)
    }

    fn proc_macro(&self, krate: usize) -> Option<bool> {
        self.world_ats.get(krate).map(|&at| CrateGraph::proc_macro(self.list, at))
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'n str> {
        self.inner.file(krate, path)
    }

    fn file_items(
        &self,
        krate: usize,
        index: u32,
        path: blaze_scope::package_path::PackagePath<'_>,
        surface: ContentId,
        edition: blaze_surface::Edition,
        bump: blaze_own::Bump<'n>,
    ) -> Result<Option<blaze_scope::items_record::CurrentFile<'n>>, blaze_scope::items_record::ItemsRecordError> {
        let Some(package) = self.packages.get(krate) else { return Ok(None) };
        let Some(full) = path.in_package(package, self.alloc) else { return Ok(None) };
        if let Some(real) = full.strip_prefix("/0/")
            && (!self.has_unread_outlines || outline_exists(self.outlines, real, self.alloc))
        {
            return self.inner.file_items(krate, index, path, surface, edition, bump);
        }
        let Some(text) = blaze_scope::resolve::SourceTexts::read(self.texts, krate, &full) else {
            return Ok(None);
        };
        let digest = ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes());
        // If this is an unread checkout file (`/0/...`), verify its full-text digest against the
        // request's initial sweep so concurrent post-sweep edits (e.g. body/const edits that keep
        // `Outline::surface` identical) are refused as stale rather than mixing snapshots.
        if let Some(real) = full.strip_prefix("/0/")
            && let Some(digests) = self.unread_digests
        {
            let find = |path: &str| digests.binary_search_by(|(p, _)| p.as_str().cmp(path)).ok().and_then(|i| digests.get(i)).map(|row| row.1);
            let expected = find(real).or_else(|| {
                let stem = real.strip_suffix("/mod.rs")?;
                let mut slot = blaze_own::AString::from_str_in(stem, self.alloc);
                slot.push_str(".rs");
                find(slot.as_str())
            });
            if let Some(saw) = expected
                && saw != digest
                && let Some(key) = self.crates.key(krate)
            {
                return Err(blaze_scope::items_record::ItemsRecordError::Stale {
                    krate: key,
                    file: Some(index),
                    call: None,
                    saw,
                    found: digest,
                });
            }
        }
        let Some(&world_at) = self.world_ats.get(krate) else {
            return blaze_scope::items_record::CurrentFile::from_text(index, text, edition, bump).map(Some);
        };
        self.dep_files.get_or_outline(world_at, index, text, digest, edition, bump).map(Some)
    }

    fn expansion(&self, krate: usize, call: ItemKey) -> Option<blaze_ladder::front2::source::LoadedExpansion<'n>> {
        self.inner.expansion(krate, call)
    }
}

/// Opens one stretch's `Records` directly over [`BodiesLoad`], matching
/// `run_names::names_records_over` while reusing verified names proofs, crate-graph `proc_macro`
/// flags, planned outlines, and cached dependency file outlines.
#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "a stretch's records edge: each argument one input of them")]
fn open_stretch_records<'n>(
    cx: &'n Cx,
    root: &'n Dir<ReadWrite>,
    crates: crate::run_names::ScopeCrates<'_, 'n>,
    list: &'n blaze_world::crate_list::CrateList<'static>,
    packs: crate::job_texts::Packs<'n>,
    outlines: &'n [Outlined<'n>],
    has_unread_outlines: bool,
    unread_digests: Option<&'n [(blaze_own::String<SharedAlloc<'static>>, ContentId)]>,
    dep_files: &'n DependencyFiles,
    corrected: &'n [(u32, &'n str, &'n str)],
    procs: Option<&'n crate::proc_runner::ProcRunner<'static>>,
    env: Option<&'n blaze_scope::crate_env::CrateEnv<'n>>,
    snapshot: &'n StoreSnapshot,
    expansion_keys: &'n SharedSet<Key, SharedAlloc<'static>>,
    frame: &'n Frame,
    g: blaze_own::Alloc<'static>,
) -> crate::run_names::Opened<'n> {
    use core::cell::RefCell;
    let count = crates.len();
    let mut crate_names: Vec<&'n str, &'n Frame> = Vec::with_capacity_in(count, frame);
    let mut packages: Vec<&'n str, &'n Frame> = Vec::with_capacity_in(count, frame);
    let mut world_ats: Vec<usize, &'n Frame> = Vec::with_capacity_in(count, frame);
    let mut callers: Vec<(usize, usize, blaze_expand::Edition), &'n Frame> = Vec::with_capacity_in(count, frame);
    let mut by_need: Option<(&'n [crate::front_load::Unopened], &'n dyn crate::front_load::OpenByNeed)> = None;
    let from = match crates {
        crate::run_names::ScopeCrates::Opened(crates) => {
            let mut records: Vec<crate::front_load::CrateRecords<'n>, &'n Frame> = Vec::with_capacity_in(count, frame);
            for (at, one) in crates.iter().enumerate() {
                records.push(one.records);
                crate_names.push(one.name);
                packages.push(one.package);
                world_ats.push(one.world_at);
                callers.push((at, one.world_at, one.records.edition));
            }
            crate::front_load::CratesFrom::Opened(records.into_bump_slice())
        }
        crate::run_names::ScopeCrates::ByNeed(scope) => {
            crate_names.push(scope.own.name);
            packages.push(scope.own.package);
            world_ats.push(scope.own.world_at);
            callers.push((0, scope.own.world_at, scope.own.records.edition));
            by_need = Some((scope.rest, scope.records));
            for one in scope.rest {
                crate_names.push(one.name);
                packages.push(one.package);
                world_ats.push(one.world_at);
            }
            crate::front_load::CratesFrom::ByNeed { own: scope.own.records, rest: scope.rest, records: scope.records }
        }
    };
    let crate_names: &'n [&'n str] = crate_names.into_bump_slice();
    let packages: &'n [&'n str] = packages.into_bump_slice();
    let world_ats: &'n [usize] = world_ats.into_bump_slice();
    let callers: &'n [(usize, usize, blaze_expand::Edition)] = callers.into_bump_slice();
    let texts: &'n crate::job_texts::JobTexts<'n> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(
        crate::job_texts::JobTexts {
            cx,
            root,
            frame,
            packs,
            corrected,
            env: env.map(|env| (0, env)),
            envs: None,
            read: RefCell::new(std::vec::Vec::new_in(Global)),
            unread: RefCell::new(Vec::new_in(frame)),
            data: RefCell::new(std::vec::Vec::new_in(Global)),
            left: RefCell::new(std::vec::Vec::new_in(Global)),
            procs: crate::job_texts::Procs::of(procs),
            own: None,
            callers,
            by_need,
            run_stopped: core::cell::Cell::new(None),
            activity: None,
        },
        frame,
    ));
    let storage = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(blaze_own::Erased::new(frame), frame));
    let alloc = storage.handle();
    let load: &'n crate::front_load::StoreLoad<'n> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(
        crate::front_load::StoreLoad::from_crates(from, packages, texts, Some(snapshot), Some(expansion_keys), Some(outlines), alloc),
        frame,
    ));
    let bodies_load: &'n BodiesLoad<'n> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(
        BodiesLoad { inner: load, crates: from, texts, world_ats, packages, list, outlines, has_unread_outlines, unread_digests, dep_files, alloc },
        frame,
    ));
    let log: &'n blaze_ladder::front2::source::Log<'n> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(
        blaze_ladder::front2::source::Log::new(crate_names, g),
        frame,
    ));
    let keys = load.keys(frame);
    let records: &'n blaze_ladder::front2::source::Records<'n> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(
        blaze_ladder::front2::source::Records::new(keys, bodies_load, log, frame, g),
        frame,
    ));
    let sources: &'n blaze_ladder::front2::body_host::Sourced<'n, crate::job_texts::JobTexts<'n>> =
        blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(blaze_ladder::front2::body_host::Sourced(texts), frame));
    crate::run_names::Opened { records, load, sources }
}

/// A Bodies phase's setup ([`BodiesStage::prepare`]): what every body check of the phase reads,
/// made once on the coordinator and read by its runs through [`Prepared::runner`] from any
/// worker. Owned here,
/// never in a frame: the snapshot's owning handle and the expansion key set have destructors a
/// frame never runs; kept until every body worker has joined.
pub(crate) struct Prepared<'s, 'f, 'r> {
    /// The store's committed snapshot, for lazy expansion texts.
    snapshot: crate::store::SharedSnapshot,
    expansion_keys: SharedSet<Key, SharedAlloc<'static>>,
    /// Outlined dependency files cached once per phase across all stretches.
    dep_files: DependencyFiles,
    /// Each member's check scope, files and item keys (`groups`); `None` when the stage stopped
    /// (every task's row is then in the phase's buffer, filed by `prepare`).
    found: Option<Found<'f, 'r>>,
    /// The bodies to check, by (member, file, name).
    ones: &'f [One<'f>],
    outlined: &'s [Outlined<'s>],
    root: &'s Dir<ReadWrite>,
    fs: &'s bstd::fs::Fs,
    board: &'s crate::engine_status::Board<'s>,
    clock: &'s Clock,
    listings: &'s crate::engine_heads::Listings,
    seeds: Option<&'s crate::engine_seeds::RequestTable<'s>>,
    names_keys: Option<crate::engine_names::NamesKeys<'s>>,
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    heap: &'static blaze_own::Heap,
    keep_scratch: bool,
    has_unread_outlines: bool,
    unread_digests: Option<blaze_own::Arc<blaze_own::Vec<(blaze_own::String<SharedAlloc<'static>>, ContentId), SharedAlloc<'static>>, SharedAlloc<'static>>>,
    plants: RowPlants,
}

/// Scope records opened by need, resolving each crate's `CrateScope` input lazily on its first
/// actual question when [`scope_by_need`] deferred upfront lookups for a standing member.
pub(crate) struct LazyScopeRecords<'r> {
    inner: crate::front_load::ScopeRecords<'r>,
    names: Option<crate::engine_names::NamesKeys<'r>>,
    resolved: blaze_own::Vec<bstd::sync::PublishOnce<Result<Input, &'static str>>, SharedAlloc<'static>>,
    heap: &'static blaze_own::Heap,
}

impl<'r> LazyScopeRecords<'r> {
    fn new(snapshot: &'r StoreSnapshot, crates: usize, names: Option<crate::engine_names::NamesKeys<'r>>, heap: &'static blaze_own::Heap) -> LazyScopeRecords<'r> {
        let resolved = blaze_own::Vec::from_iter_in((0..crates).map(|_| bstd::sync::PublishOnce::new()), heap.shared());
        LazyScopeRecords { inner: crate::front_load::ScopeRecords::new(snapshot, crates, heap), names, resolved, heap }
    }

    pub(crate) fn counts(&self) -> crate::front_load::ByNeedCounts {
        self.inner.counts()
    }
}

impl crate::front_load::OpenByNeed for LazyScopeRecords<'_> {
    fn open<'s>(&'s self, one: &crate::front_load::Unopened) -> Result<crate::front_load::CrateRecords<'s>, &'static str> {
        if one.input.at != Key::default() {
            return self.inner.open(one);
        }
        let Some(slot) = self.resolved.get(one.world_at) else {
            return Err("its place is past the request's crate list (engine_bodies.rs `LazyScopeRecords`)");
        };
        let got = match slot.get() {
            Some(got) => *got,
            None => {
                let resolved_input = match self.names {
                    None => Err("its crate's names inputs were not given to resolve a lazy scope crate (engine_bodies.rs `LazyScopeRecords`)"),
                    Some(keys) => keys.input_of(one.input.name).ok_or_else(|| {
                        crate::engine_head_notes::leak(self.heap.alloc(), format_args!("crate `{}` has no names record this command (engine_bodies.rs `LazyScopeRecords`)", one.name))
                    }),
                };
                let _ = slot.set(resolved_input);
                slot.get().copied().unwrap_or(resolved_input)
            }
        };
        let input = got?;
        let resolved = crate::front_load::Unopened { input, ..*one };
        self.inner.open(&resolved)
    }
}

/// One entry's check by [`BodyRunner::run_body`]: its row in `out` (made or failed, by
/// [`Outcome`]), its body record and its findings, and its frozen-head counts.
pub(crate) struct BodyRun<'a, 'f> {
    pub(crate) out: Buffer<'a>,
    pub(crate) ran: SVec<'a, Ran<'f>>,
    pub(crate) findings: RunOut<'a>,
    pub(crate) tally: crate::engine_seeds::SeedTally,
    /// The run's thread stretch `(start, end)` in ns since request start, for `Times::job`.
    pub(crate) span: (u64, u64),
}

// Bounded where declared (rule 76; [`Census`]): 62 KiB each with the census by value.
const _: () = assert!(core::mem::size_of::<BodyRun<'static, 'static>>() <= 4 << 10);
const _: () = assert!(core::mem::size_of::<BodiesStage<'static>>() <= 4 << 10);
const _: () = assert!(core::mem::size_of::<Result<(Buffer<'static>, SVec<'static, Ran<'static>>, RunOut<'static>), EngineCheckError>>() <= 4 << 10);

/// What a body run reads of its [`BodiesStage`], and nothing more: the request-wide impl shelf,
/// the heads not frozen and how names reads are kept, all read only and `Sync`. A run never
/// borrows the stage itself, whose frame-held parts (its findings, its notes) are the
/// coordinator's alone: the phase path's workers and the `Ready` driver's `BodyCheck` kind
/// (engine_ready_check.rs) hold this view across threads (rule 30: one view, both drivers).
#[derive(Clone, Copy)]
pub(crate) struct StageReads<'v> {
    shelf: &'v ImplShelf,
    /// Where a crate the shelf lacks is read from on a body's first probe (the `Ready` driver's
    /// stored rows, engine_shelf_pages.rs); `None` on the phase path, whose shelf holds every
    /// crate's tables before the bodies run (engine_heads.rs `complete`).
    source: Option<&'v dyn blaze_ladder::front2::buckets::ShelfSource>,
    heads_failed: &'v crate::engine_head_notes::Failed,
    scope_reads: crate::engine_scope_reads::ScopeReadsAs,
}

/// What every body run of one setup reads but today's digests: the parts of [`Shared`] that are
/// the setup's ([`Prepared`]) and the stage's ([`StageReads`]), with the bodies to check and the
/// command's clock. Made once on the coordinator ([`Prepared::runner`]) and borrowed by runs on
/// any worker: every part is one a phase's workers already share ([`Shared`]), so it is `Sync`
/// where `Prepared` is not (its setup's own lists, `Found::took`, are on a one-thread
/// allocator). A run is [`Shared`] over this and its own digests ([`BodyRunner::over`]): one
/// shape for both drivers (rule 30).
#[derive(Clone, Copy)]
pub(crate) struct BodyRunner<'v, 'f, 'r> {
    grains: &'v ReadGrains<'static>,
    outlined: &'v [Outlined<'v>],
    listings: &'v crate::engine_heads::Listings,
    root: &'v Dir<ReadWrite>,
    fs: &'v bstd::fs::Fs,
    packs: &'v crate::body_types::SyncLazyTrees<'f>,
    corrected: &'v [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    groups: &'v [Group<'f, 'r>],
    snapshot: &'v StoreSnapshot,
    expansion_keys: &'v SharedSet<Key, SharedAlloc<'static>>,
    heads: &'v crate::engine_heads::Subjects,
    heap: &'static blaze_own::Heap,
    stage: StageReads<'v>,
    seeds: Option<&'v crate::engine_seeds::RequestTable<'v>>,
    board: &'v crate::engine_status::Board<'v>,
    keep_scratch: bool,
    has_unread_outlines: bool,
    unread_digests: Option<&'v [(blaze_own::String<SharedAlloc<'static>>, ContentId)]>,
    plants: RowPlants,
    /// The bodies this setup left to check, by (member, file, name).
    ones: &'v [One<'f>],
    clock: &'v Clock,
    /// The request's scope records opened by need ([`Prepared::scope_records`]): every run's
    /// crates past its member ([`GroupCrates::ByNeed`]) are opened here at their first question.
    by_need: &'v dyn crate::front_load::OpenByNeed,
    /// Outlined dependency files cached once per phase across stretches.
    dep_files: &'v DependencyFiles,
}

/// A body a run was asked for that its setup did not leave to check (rule 19), in the owned,
/// `Copy` form a worker's run hands the coordinator; the command's error is
/// `EngineCheckError::Unplaced`, by `From`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct NotInSetup {
    pub(crate) body: Name,
    pub(crate) what: &'static str,
}

impl From<NotInSetup> for EngineCheckError {
    fn from(NotInSetup { body, what }: NotInSetup) -> EngineCheckError {
        EngineCheckError::Unplaced { body, what }
    }
}

impl<'v, 'f, 'r> BodyRunner<'v, 'f, 'r> {
    /// What a run reads: this setup's parts over today's digests `reads`.
    fn over<'x, 'a, R: ?Sized + CellDigests>(&self, reads: &'x R) -> Shared<'x, 'a, 'f, 'r, R>
    where
        'v: 'x,
    {
        Shared {
            grains: self.grains,
            outlined: self.outlined,
            listings: self.listings,
            root: self.root,
            fs: self.fs,
            packs: self.packs,
            corrected: self.corrected,
            procs: self.procs,
            groups: self.groups,
            snapshot: self.snapshot,
            expansion_keys: self.expansion_keys,
            heads: self.heads,
            reads,
            rows_in: core::marker::PhantomData,
            heap: self.heap,
            shelf: self.stage.shelf,
            source: self.stage.source,
            seeds: self.seeds,
            heads_failed: self.stage.heads_failed,
            scope_reads: self.stage.scope_reads,
            board: self.board,
            keep_scratch: self.keep_scratch,
            has_unread_outlines: self.has_unread_outlines,
            unread_digests: self.unread_digests,
            plants: self.plants,
            by_need: self.by_need,
            dep_files: self.dep_files,
        }
    }

    /// The member's own `CrateScope` cell (`Stage::Names`) for `body`: what `BodyKind::run`
    /// peeks before opening scope crates or typechecking so a body scheduled in the same tick as
    /// `FileItems` (`Stage::Outlines`) parks in O(1) instead of running twice.
    pub(crate) fn own_scope_cell(&self, body: Name) -> Option<CellId> {
        let one = self.ones.iter().find(|one| one.task.planned.name == body)?;
        let group = self.groups.get(usize::try_from(one.group).ok()?)?;
        Some(CellId { of: group.own, cell: WHOLE })
    }

    /// One entered body's check (its whole entry, never sliced) over today's digests `reads`,
    /// on the calling thread, from any worker (the `Ready` driver, D187). A task this setup
    /// did not leave to check is refused, naming it.
    pub(crate) fn run_body<'a, R: CellDigests>(&self, cx: &Cx, reads: &R, task: &Task, alloc: SharedAlloc<'a>) -> Result<BodyRun<'a, 'f>, NotInSetup> {
        let name = task.planned.name;
        let one = self.ones.iter().find(|one| one.task.planned.name == name).ok_or(NotInSetup { body: name, what: "not a body its phase's setup left to check (engine_bodies.rs `prepare`)" })?;
        let shared = self.over(reads);
        let mut run = BodyRun { out: Buffer::new_in(alloc), ran: SVec::new_in(alloc), findings: RunOut::new(alloc), tally: crate::engine_seeds::SeedTally::default(), span: (0, 0) };
        let began = crate::engine_times::ns(self.clock.now().since_start());
        run_bodies(cx, &shared, core::slice::from_ref(one), self.clock, &mut run.out, &mut run.ran, &mut run.findings, &mut run.tally, alloc);
        run.span = (began, crate::engine_times::ns(self.clock.now().since_start()));
        Ok(run)
    }
}

impl<'s, 'f, 'r> Prepared<'s, 'f, 'r> {
    /// This setup's runner over `found` (this setup's, `Prepared::found`) and the stage's view
    /// `stage` ([`BodiesStage::reads_view`]).
    fn runner_of<'v>(&'v self, found: &'v Found<'f, 'r>, stage: StageReads<'v>, by_need: &'v dyn crate::front_load::OpenByNeed) -> BodyRunner<'v, 'f, 'r> {
        let (packs, corrected) = (&*found.trees.0, found.trees.1);
        BodyRunner {
            grains: &found.grains,
            outlined: self.outlined,
            listings: self.listings,
            root: self.root,
            fs: self.fs,
            packs,
            corrected,
            procs: self.procs,
            groups: &found.groups,
            snapshot: self.snapshot.as_ref(),
            expansion_keys: &self.expansion_keys,
            heads: &found.heads,
            heap: self.heap,
            stage,
            seeds: self.seeds,
            board: self.board,
            keep_scratch: self.keep_scratch,
            has_unread_outlines: self.has_unread_outlines,
            unread_digests: self.unread_digests.as_deref().map(blaze_own::Vec::as_slice),
            plants: self.plants,
            ones: self.ones,
            clock: self.clock,
            by_need,
            dep_files: &self.dep_files,
        }
    }

    /// What the runs read, over today's digests `reads`, the stage's view `stage` and the
    /// request's scope records `by_need`.
    fn shared<'v, 'a, R: ?Sized + CellDigests>(&'v self, found: &'v Found<'f, 'r>, stage: StageReads<'v>, reads: &'v R, by_need: &'v dyn crate::front_load::OpenByNeed) -> Shared<'v, 'a, 'f, 'r, R> {
        self.runner_of(found, stage, by_need).over(reads)
    }

    /// The runner every body run of this setup borrows (the `Ready` driver's `BodyCheck` kind,
    /// D187), its scope crates opened by need through `by_need` (this setup's
    /// [`Prepared::scope_records`]): `None` when the setup stopped before any crate was read,
    /// every task's row then filed by `prepare` (decided, never run).
    pub(crate) fn runner<'v>(&'v self, stage: StageReads<'v>, by_need: &'v dyn crate::front_load::OpenByNeed) -> Option<BodyRunner<'v, 'f, 'r>> {
        self.found.as_ref().map(|found| self.runner_of(found, stage, by_need))
    }

    /// The request's scope records opened by need (piece C (a)), over this setup's committed
    /// snapshot, one slot a crate of its list: made by the caller beside the runs that borrow
    /// it (its records are borrowed from the snapshot this setup holds), its counts read after
    /// they end ([`BodiesStage::add_by_need`]). One a request: a crate is opened once whichever
    /// job asks it first (the floor: the crates the checks ask, each once).
    pub(crate) fn scope_records(&self) -> LazyScopeRecords<'_> {
        let crates = self.found.as_ref().and_then(|found| found.groups.first()).map_or(0, |group| CrateGraph::len(&group.list));
        LazyScopeRecords::new(self.snapshot.as_ref(), crates, self.names_keys, self.heap)
    }

    /// Pre-opens the distinct lang crates (`core`, `alloc`, `std`, `bstd`) of active groups (those
    /// referenced by `self.ones`) ONCE in parallel across `workers` before body stretches begin,
    /// avoiding redundant nested spawns and `OnceLock` contention across concurrent stretches.
    pub(crate) fn pre_open_lang(&self, cx: &Cx, by_need: &LazyScopeRecords<'_>, workers: NonZeroU32) -> Result<(), CliError> {
        self.pre_open_lang_with(cx, by_need, workers, |_| ())
    }

    /// Runs `work` on the current thread while pre-opening the distinct lang crates (`core`,
    /// `alloc`, `std`, `bstd`) of active groups in parallel on `workers`.
    pub(crate) fn pre_open_lang_with<T>(&self, cx: &Cx, by_need: &LazyScopeRecords<'_>, workers: NonZeroU32, work: impl FnOnce(&Cx) -> T) -> Result<T, CliError> {
        let Some(found) = self.found.as_ref() else { return Ok(work(cx)) };
        if self.ones.is_empty() || workers.get() <= 1 {
            return Ok(work(cx));
        }
        let mut to_open: [Option<&crate::front_load::Unopened>; 8] = [None; 8];
        let mut count = 0usize;
        let mut seen_groups: [u32; 8] = [u32::MAX; 8];
        let mut seen_count = 0usize;
        for one in self.ones {
            if seen_groups.iter().take(seen_count).any(|&g| g == one.group) {
                continue;
            }
            if let Some(slot) = seen_groups.get_mut(seen_count) {
                *slot = one.group;
                seen_count += 1;
            }
            let Some(group) = usize::try_from(one.group).ok().and_then(|at| found.groups.get(at)) else { continue };
            let GroupCrates::ByNeed { rest, .. } = &group.crates else { continue };
            for &place in group.lang.iter() {
                if place > 0
                    && let Some(unopened) = rest.get(place - 1)
                    && !to_open.iter().take(count).flatten().any(|prev| prev.world_at == unopened.world_at)
                    && let Some(slot) = to_open.get_mut(count)
                {
                    *slot = Some(unopened);
                    count += 1;
                }
            }
            if count == to_open.len() {
                break;
            }
        }
        if count < 2 {
            return Ok(work(cx));
        }
        let pre_cx = cx.span("engine: pre-open lang crates");
        let scope = pre_cx.scope();
        scope.narrow_workers(workers, Class::Foreground);
        let mut handles = blaze_own::Vec::with_capacity_in(count, pre_cx.frame());
        for unopened in to_open.iter().take(count).flatten().copied() {
            handles.push(scope.may_overlap("engine: open lang crate", 1 << 20, move |_cx: &Cx| {
                let _ = crate::front_load::OpenByNeed::open(by_need, unopened);
                Ok::<_, Infallible>(())
            }));
        }
        let out = work(cx);
        let mut first_err: Option<CliError> = None;
        for handle in handles.into_iter() {
            if let Err(err) = handle.join(&pre_cx) {
                first_err.get_or_insert_with(|| CliError::Pool(err));
            }
        }
        if let Err(err) = scope.join(&pre_cx) {
            first_err.get_or_insert_with(|| CliError::Pool(err));
        }
        if let Some(err) = first_err {
            return Err(err);
        }
        Ok(out)
    }

    /// The `Ready` driver's bodies joined into `stage` (D187): the runs its `BodyCheck` kind
    /// sank, each one entry's [`BodyRunner::run_body`] whose row the engine took, merged by THE
    /// ONE JOIN ([`BodiesStage::join_runs`]) as the phase path merges its runs; the setup's
    /// counts the phase path keeps after its runs kept the same way. Each run's bodies are found
    /// among this setup's `ones` by name (a run is one entry, never sliced). The rows the runs
    /// still hold (none but their owed names: the engine took each run's own row) go to a
    /// scratch buffer and are dropped. Its run facts, in `at_phase`, for the caller's puts.
    #[expect(clippy::too_many_arguments, reason = "the join's edge: each argument one handle of it")]
    pub(crate) fn join_ready<'a, 'st, 'p>(
        &self,
        stage: &mut BodiesStage<'f>,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'st>,
        runs: SVec<'a, BodyRun<'a, 'f>>,
        times: &mut crate::engine_times::Times,
        frame: &'f Frame,
        alloc: SharedAlloc<'a>,
        at_phase: SharedAlloc<'p>,
    ) -> Result<MadeFacts<'p>, CliError> {
        stage.runs = u32::try_from(runs.len()).unwrap_or(u32::MAX);
        if let Some(found) = self.found.as_ref() {
            stage.uncovered = found.uncovered;
        }
        let mut entries: SVec<'a, &[One<'f>]> = SVec::with_capacity_in(runs.len(), alloc);
        let mut buffers: SVec<'a, (Buffer<'a>, SVec<'a, Ran<'f>>, RunOut<'a>)> = SVec::with_capacity_in(runs.len(), alloc);
        for run in runs.into_iter() {
            stage.seeds.add(&run.tally);
            times.job(crate::engine_times::Step::BodiesRun, run.span.0, run.span.1);
            // The run's one entry: its body's `One` (`run_body` runs one, never a slice).
            let one = run.ran.first().and_then(|ran| self.ones.iter().find(|one| one.task.planned.name == ran.name));
            entries.push(one.map_or(&[][..], core::slice::from_ref));
            buffers.push((run.out, run.ran, run.findings));
        }
        stage.expansion_demands = self.snapshot.expansion_demands();
        stage.expansion_keys = self.expansion_keys.len() as u64;
        let mut scratch = Buffer::new_in(alloc);
        stage.join_runs(cx, store, &mut scratch, times, &entries, buffers, 0, self.heap, frame, at_phase)
    }
}

impl<'f> BodiesStage<'f> {
    /// What a body run reads of this stage ([`StageReads`]): borrowed by the runs, on any
    /// worker, while the stage is read only.
    pub(crate) fn reads_view(&self) -> StageReads<'_> {
        StageReads { shelf: &self.shelf, source: None, heads_failed: &self.heads_failed, scope_reads: self.scope_reads }
    }

    /// [`Self::reads_view`] whose runs read a crate the shelf lacks from `source` on their
    /// first probe of it (the `Ready` driver, D187: only the crates its bodies probe are read).
    pub(crate) fn reads_paged<'v>(&'v self, source: &'v dyn blaze_ladder::front2::buckets::ShelfSource) -> StageReads<'v> {
        StageReads { source: Some(source), ..self.reads_view() }
    }

    pub(crate) fn new(scope_reads: crate::engine_scope_reads::ScopeReadsAs) -> BodiesStage<'f> {
        BodiesStage {
            kept: Kept::default(),
            shelf: ImplShelf::new(ImplShelfOpts {}),
            ran: Vec::new_in(Global),
            findings: None,
            data_files: None,
            clean: 0,
            items: 0,
            item_tasks: 0,
            ownerless: 0,
            ownerless_files: Vec::new_in(Global),
            wall_ns: 0,
            runs: 0,
            uncovered: 0,
            uncovered_note: None,
            groups_found: false,
            scratch_most: 0,
            frames: FramesHeld::default(),
            seeds: crate::engine_seeds::SeedTally::default(),
            heads_failed: crate::engine_head_notes::Failed::new(),
            outside: crate::engine_body_plan::Outside::new(),
            scope_reads,
            expansion_demands: 0,
            expansion_keys: 0,
            owed_listing: OwedListing::default(),
            member_rows: None,
            typed: None,
            typed_bytes: 0,
            untyped_clean: 0,
            setup: StretchSetup::default(),
            slices: None,
            cost_fit: CostFit::default(),
            grains_last: None,
            grains_entered: None,
            grains_key: None,
            grains_line: None,
            members_last: None,
            outlines_moved: None,
            outlines_entered: None,
            unread: None,
            unread_digests: None,
            members_key: None,
            standing_drops_heads: false,
            row_key_unit_only: false,
            deferral: Deferral::Below,
            reach: None,
            shared_rows: None,
            unreadable: None,
            by_need: crate::front_load::ByNeedCounts::default(),
        }
    }

    /// Plants crate `krate`'s record unreadable for a scope made by need, as `read`'s
    /// `plant_unreadable` does for [`scope_of`]: a test's cause (rule 23), never the command's.
    pub(crate) fn plant_unreadable(&mut self, krate: Option<&'static str>) {
        self.unreadable = krate;
    }

    /// Plants the members' row key's control when `plant` is
    /// `CommitPlant::RowKeyUnitOnly`: a test's cause (rule 23), never the command's.
    pub(crate) fn plant_rows(&mut self, plant: crate::engine_ready_edit::CommitPlant) {
        // A pattern, not a value: only the tests construct the plant (its `expect(dead_code)`).
        self.row_key_unit_only = matches!(plant, crate::engine_ready_edit::CommitPlant::RowKeyUnitOnly);
    }

    /// What the request's scope records opened by need did (`counts`, read after every run
    /// that borrowed them ended), added to the profile's line.
    pub(crate) fn add_by_need(&mut self, counts: crate::front_load::ByNeedCounts) {
        self.by_need.asks = self.by_need.asks.saturating_add(counts.asks);
        self.by_need.opened = self.by_need.opened.saturating_add(counts.opened);
        self.by_need.bytes = self.by_need.bytes.saturating_add(counts.bytes);
    }

    /// The last request's members record (`record`: the key the checkout's members root names,
    /// `None` for none) and the serial of the answer this request starts from: a member stands
    /// from the record only where it was made over that answer (engine_members_record.rs
    /// `Header::made_at`). Both drivers set it; only [`StandingMembers`] reads it.
    pub(crate) fn members_from(&mut self, record: Option<Key>, last_serial: u64) {
        self.members_last = Some((record, last_serial));
    }

    /// THE PLAN'S REACH (D191): which member units reach each file, made once a request from
    /// `list`, THE CRATE LIST THE PLAN ALREADY HOLDS (the names stage's world, engine_names.rs
    /// `NamesStage`: on the phase path made or read back by engine_units.rs `discover` before
    /// the plan, the very bytes the `UnitGraph` record is sealed with; on the ready path read
    /// back by its setup), never read from the store: on the phase path the graph's record is
    /// filed only when the engine merges the Units stage, after the plan, so a store read here
    /// misses on every request whose Units stage runs (a new blaze, a moved manifest, a fresh
    /// home). Kept for every Bodies phase's setup and returned for the plan ([`plan_views`],
    /// called by engine_ready_edit.rs `file_tasks`): one object for both (rule 30), so a task's
    /// row is named by the setup exactly as the plan named it. `from`: the phase path walks every
    /// member's tree over `outlined`, the outlines this request made before the plan
    /// ([`ReachFrom::Walk`]); the ready path reads the last members record
    /// ([`ReachFrom::MembersRecord`]; [`BodiesStage::members_from`] first), as its outlines are
    /// the edited files alone. `None`: no crate list (its world was not made: the graph's record
    /// is sealed with none, so the setup finds none either, and the plan names no view). A
    /// members record that does not open over today's graph (`graph`), or holds no entry of a
    /// member, refuses the ready request ([`crate::engine_ready_edit::ReadyRefused::Members`],
    /// rule 3), never a reach guessed.
    #[expect(clippy::too_many_arguments, reason = "the plan's edge: each argument one handle of it")]
    pub(crate) fn reach(&self) -> Option<&Reach> {
        self.reach.as_ref()
    }

    pub(crate) fn plan_reach<'r>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        graph: Option<Input>,
        list: Option<&blaze_world::crate_list::CrateList<'_>>,
        outlined: &[Outlined<'_>],
        // Every member unit's found files (engine_body_plan.rs `FoundEdge`): its walk's beside
        // its root, and the setup's walk of it (`groups`).
        found: &[crate::engine_body_plan::FoundEdge],
        from: ReachFrom,
        g: blaze_own::Alloc<'static>,
    ) -> Result<Option<&Reach>, CliError> {
        use crate::engine_ready_edit::ReadyRefused;
        self.reach = None;
        let Some(list) = list else { return Ok(None) };
        let reach = match from {
            ReachFrom::Walk => Reach::walk(list, outlined, found, g).map_err(EngineCheckError::Unoutlined)?,
            ReachFrom::MembersRecord => {
                let refused = |unit: Option<Name>| EngineCheckError::ReadyRefused(ReadyRefused::Members { unit });
                let Some(graph) = graph else { return Err(refused(None).into()) };
                let stored = match self.members_last.and_then(|(record, _)| record) {
                    Some(key) => store.get(cx, key)?.map(|held| held.bytes),
                    None => None,
                };
                let view = match stored.map(|bytes| crate::engine_members_record::View::open(bytes, g)) {
                    Some(Ok(view)) if view.header.graph_out == graph.out => view,
                    _ => return Err(refused(None).into()),
                };
                Reach::of_members(list, &view, found, g).map_err(|unknown| refused(Some(unknown.unit)))?
            }
        };
        self.reach = Some(reach);
        Ok(self.reach.as_ref())
    }

    /// The ready driver's setup: `entering` (the wake's names, any kinds) holds every outline
    /// this request moves; the next setups take a member none of whose files moved, and none of
    /// whose files a body entered is in, from the members record ([`StandingMembers`]).
    pub(crate) fn stand_members(&mut self, entering: &[Name], plant: crate::engine_ready_edit::CommitPlant, heap: blaze_own::Alloc<'static>) {
        let mut moved: Vec<Name, blaze_own::Alloc<'static>> = Vec::new_in(heap);
        moved.extend(entering.iter().copied().filter(|name| name.kind == Kind::FileItems));
        moved.sort_unstable();
        moved.dedup();
        self.outlines_moved = Some(moved);
        // A pattern, not a value: only the tests construct the plant (its `expect(dead_code)`).
        self.standing_drops_heads = matches!(plant, crate::engine_ready_edit::CommitPlant::StandingDropsHeads);
        self.deferral = if matches!(plant, crate::engine_ready_edit::CommitPlant::HoldersKeyedByText) {
            Deferral::ByText
        } else if matches!(plant, crate::engine_ready_edit::CommitPlant::DeferredNeverVisited) {
            Deferral::NeverVisited
        } else {
            Deferral::Below
        };
    }

    /// The ready setup's: the files it has an outline of and did not read back (sorted, each
    /// once). Under [`StandingMembers`] a visited member's held file among them is refused by
    /// name in `groups` (it would key the member's row apart from the phase path's).
    pub(crate) fn unread_outlines(
        &mut self,
        unread: Vec<Name, blaze_own::Alloc<'static>>,
        digests: Option<blaze_own::Arc<blaze_own::Vec<(blaze_own::String<SharedAlloc<'static>>, ContentId), SharedAlloc<'static>>, SharedAlloc<'static>>>,
    ) {
        self.unread = Some(unread);
        self.unread_digests = digests;
    }

    /// THE READY SETUP'S READS (piece B2): which outlines the bodies' setup will need, before any
    /// is read back, so the ready setup reads only those (engine_check.rs `checking`) and never
    /// every file's. A member taken from the members record ([`stands`], the one decision
    /// `groups` makes under [`StandingMembers`] over the same `moved`) needs none of its outlines:
    /// its claims are made by name. A member it will visit needs its module tree (`reached` from
    /// its root) and each file it holds: its root, and the files the record last held and
    /// reached for it (a hint the caller closes under each outline's file parts, so a reach the
    /// edit grew is read whole; a held file outside it is refused by name in `groups`, never
    /// taken as not outlined). `moved`: the wake's outlines and the owed bodies' files, sorted,
    /// as `groups` makes it. Returned sorted, each once; empty when no crate list is read
    /// (`groups` then stops before any member).
    #[expect(clippy::too_many_arguments, reason = "the setup's edge: each argument one handle of it")]
    pub(crate) fn ready_reads<'r>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        graph: blaze_drive::Input,
        names: NamesInputs<'_, '_>,
        tool: blaze_drive::Tool,
        moved: &[Name],
        frame: &'f Frame,
        g: blaze_own::Alloc<'static>,
    ) -> Result<Vec<Name, blaze_own::Alloc<'static>>, CliError> {
        use crate::engine_members_record as rec;
        let mut seeds: Vec<Name, blaze_own::Alloc<'static>> = Vec::new_in(g);
        let Some(list) = self.kept.list(cx, store, &graph, frame)? else { return Ok(seeds) };
        let today = rec::Header { graph_out: graph.out, tool, made_at: self.members_last.map_or(0, |(_, serial)| serial) };
        let stored = match self.members_last.and_then(|(record, _)| record) {
            Some(key) => store.get(cx, key)?.map(|held| held.bytes),
            None => None,
        };
        // A record that does not open stands no member (`groups` counts it and visits each):
        // every member's tree is then read, never one taken from a record it will not take.
        let view = stored.and_then(|bytes| rec::View::open(bytes, g).ok());
        let world = names.stage().names.as_ref();
        let is_test = |name: Name| world.is_some_and(|names| names.crate_at(name.subject).is_some_and(|at| names.list().test(at)));
        let root_of = |at: usize| Name::file(list.root(at).strip_prefix("/0/").unwrap_or(list.root(at)).as_bytes());
        // Only the visited members' own module trees need planned outlines upfront (for `groups`'s
        // `reached_under`, `file_heads`, and `list_files_with`). Any transitive member dependency's
        // file text actually asked during checking (`StoreLoad::file_items`, e.g. a `const` body)
        // is outlined on demand from disk (`CurrentFile::from_text`), so standing transitive
        // dependency outlines are never eagerly read back from the store.
        for member in (0..CrateGraph::len(&list)).filter(|at| list.member(*at)) {
            let own_name = blaze_drive::key::crate_scope_name(&list.key(member));
            let unit = crate::engine_body_plan::Unit { name: own_name, prefix: list.key(member).prefix(), test: is_test(own_name) };
            let place = u32::try_from(member).unwrap_or(u32::MAX);
            if view.as_ref().and_then(|view| stands(view, place, &today, moved, &unit, names)).is_some() {
                continue;
            }
            seeds.push(root_of(member));
            if let Some(kept) = view.as_ref().and_then(|view| view.member(place)) {
                match &kept.state {
                    rec::MemberState::Held(held) => seeds.extend(held.held.iter().map(|held| held.slot)),
                    rec::MemberState::CutScope(cut) => seeds.extend(cut.held.iter().map(|held| held.slot)),
                    rec::MemberState::Refused(_) => {}
                }
                seeds.extend(kept.not_held.iter().copied());
            }
        }
        seeds.sort_unstable();
        seeds.dedup();
        Ok(seeds)
    }

    /// The members record this request put: the key the checkout's members root names from the
    /// next commit on (engine_check.rs pins it). `None`: no Bodies phase reached the crate list.
    pub(crate) fn members_record(&self) -> Option<Key> {
        self.members_key
    }

    /// The members the setups took from the members record, unvisited, and the members they
    /// visited, over the request's Bodies phases.
    pub(crate) fn members_stood(&self) -> (u32, u32) {
        let o = &self.owed_listing;
        (o.members_stood, o.members.saturating_sub(o.members_stood))
    }

    /// The visited members' listing rows over the request's Bodies phases: read by key, made,
    /// damaged (engine_member_rows.rs `RowCounts`).
    pub(crate) fn member_row_counts(&self) -> crate::engine_member_rows::RowCounts {
        self.owed_listing.rows
    }

    /// The last request's grains record (`record`: the key the checkout's grains root names,
    /// `None` for none) and the serial of the answer this request starts from: read grains are
    /// then taken from the record (engine_scope_reads.rs `GrainsIn`). THE OBLIGATION that comes
    /// with it: [`BodiesStage::note_entered`] is told every phase's tasks before the Bodies
    /// phase, or a fact a task of this request moved is taken stale.
    pub(crate) fn grains_from(&mut self, record: Option<Key>, last_serial: u64) {
        self.grains_last = Some((record, last_serial));
    }

    /// One phase's entered tasks: its `CrateScope` and `ImplBucket` ones are kept for read
    /// grains, and its `FileItems` ones for standing-member invalidation, sorted, in `heap`.
    pub(crate) fn note_entered(&mut self, names: impl Iterator<Item = Name>, heap: blaze_own::Alloc<'static>) {
        let grains = self.grains_entered.get_or_insert_with(|| Vec::new_in(heap));
        let outlines = self.outlines_entered.get_or_insert_with(|| Vec::new_in(heap));
        for name in names {
            match name.kind {
                Kind::CrateScope | Kind::ImplBucket => grains.push(name),
                Kind::FileItems => outlines.push(name),
                _ => {}
            }
        }
        grains.sort_unstable();
        grains.dedup();
        outlines.sort_unstable();
        outlines.dedup();
    }

    /// Whether the last request's members record was loaded and is available to stand members from.
    pub(crate) fn has_members_record(&self) -> bool {
        self.members_last.and_then(|(record, _)| record).is_some()
    }

    /// The `FileItems` tasks entered in earlier phases of this request, sorted.
    pub(crate) fn outlines_entered(&self) -> &[Name] {
        self.outlines_entered.as_deref().unwrap_or(&[])
    }

    /// Whether any `ImplBucket` task entered in earlier phases of this request.
    pub(crate) fn entered_impl_bucket(&self) -> bool {
        self.grains_entered.as_deref().unwrap_or(&[]).iter().any(|name| name.kind == Kind::ImplBucket)
    }

    /// The grains record this request put: the key the checkout's grains root names from the
    /// next commit on (engine_check.rs pins it), so the next request takes its read grains
    /// from it. `None`: no Bodies phase reached the crate list.
    pub(crate) fn grains_record(&self) -> Option<Key> {
        self.grains_key
    }

    /// The keys of the typed records this request put, sorted, each once: records the answer's
    /// root must name beside the member rows (engine_check.rs `kept_beside`), or `blaze run`
    /// finds none after the store's sweep and checks each body again.
    /// Each checkout file a body read as data through its `Given` cell this request (a computed
    /// include, [`data_read`]), as a script's `file:` read: the leaf rows engine_check.rs hook H2
    /// keeps beside the scripts' (`ScriptsStage::script_rows`), so the next request follows the
    /// file and plans its `Given` task.
    pub(crate) fn data_files(&self) -> impl Iterator<Item = (crate::engine_script_reads::ReadKind, &str, ContentId)> + '_ {
        self.data_files.iter().flatten().map(|&(path, answer)| (crate::engine_script_reads::ReadKind::File, path, answer))
    }

    pub(crate) fn typed_record_keys(&mut self) -> &[Key] {
        match self.typed.as_mut() {
            Some(keys) => {
                keys.sort_unstable();
                keys.dedup();
                keys
            }
            None => &[],
        }
    }

    /// The keys of the member listing rows this request read or put, sorted, each once: the
    /// records the answer's root must name (store/roots.rs: a record lives while a root reaches
    /// it), or the next command makes each row again.
    pub(crate) fn member_row_keys(&mut self) -> &[Key] {
        match self.member_rows.as_mut() {
            Some(keys) => {
                keys.sort_unstable();
                keys.dedup();
                keys
            }
            None => &[],
        }
    }

    /// The paths of the first files (8 at most) whose bodies no member crate holds.
    pub(crate) fn ownerless_paths(&self, outlined: &[Outlined<'_>]) -> std::vec::Vec<std::string::String, Global> {
        let mut paths = std::vec::Vec::new_in(Global);
        for at in self.ownerless_files.iter().take(8) {
            paths.push(outlined.get(*at as usize).map_or_else(|| std::string::String::from("?"), |one| std::string::String::from_utf8_lossy(&one.path).into_owned()));
        }
        paths
    }

    /// The plan's notes into the request's lines (engine_body_plan.rs `Outside::lines`): a file
    /// no unit holds, said once, with no engine row; kept from `last` where no body of the file
    /// was entered. Its scratch is `frame`'s.
    pub(crate) fn plan_lines<'a>(&self, outlined: &[Outlined<'_>], unread: &[Name], last: Option<&crate::engine_lines::Last<'a, '_>>, lines: &mut crate::engine_lines::Lines<'a>, frame: &Frame) {
        self.outside.lines(self.ran.iter().map(|one| one.file), outlined, unread, last, lines, frame);
        // The bodies a macro wrote that no task checks: named once a request, never silent.
        // Decided by this request's phase when it read the members' records; else the last
        // request's note stands (nothing it is a function of was read again).
        let name = crate::engine_body_plan::note_name(UNCOVERED_NOTE);
        match (self.groups_found, self.uncovered_note) {
            (true, Some(words)) => lines.note(name, crate::engine_lines::Class::Unchecked, words.as_bytes()),
            (true, None) => {}
            (false, _) => {
                if let Some(block) = last.and_then(|last| last.block(name)) {
                    lines.keep(name, block);
                }
            }
        }
    }

    /// Whether every body entered was checked: none refused, none in a file no crate holds.
    pub(crate) fn whole(&self) -> bool {
        self.ownerless == 0 && self.ran.iter().all(|one| matches!(one.said(), None | Some(Said::BorrowsUnchecked(_))))
    }

    /// A Bodies phase's setup, on the coordinator, once a phase: each member crate's check scope
    /// and files the entered bodies touch, their listings and item keys (`groups`), and each
    /// task decided: a body to check (in [`Prepared`]), or its row now, in `buffer` (a refusal at
    /// the plan, a body no unit holds, a file no member holds). The bodies are then checked by
    /// [`BodyRunner::run_body`], one an entry, from any worker over [`Prepared::runner`], or by the phase's
    /// runs ([`BodiesStage::run_phase`]).
    /// `view` is today's digests through the engine's read seam (`CellDigests`): the phase
    /// engine's `View`, or the `Ready` driver's host (D187); a refusal's read list is read from it.
    #[expect(clippy::too_many_arguments, reason = "a phase's edge: each argument one handle of it")]
    pub(crate) fn prepare<'s, 'a, 'r, V: ?Sized + CellDigests>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        read: &mut EngineRecords<'r>,
        handles: &Handles<'s, '_, '_, '_>,
        tasks: &[Task],
        view: &V,
        buffer: &mut Buffer<'a>,
        request_heads: Option<&'s crate::engine_seeds::RequestTable<'s>>,
        listings: &'s crate::engine_heads::Listings,
        alloc: SharedAlloc<'a>,
        frame: &'f Frame,
    ) -> Result<Prepared<'s, 'f, 'r>, CliError> {
        let cx = cx.span("engine: the bodies phase's setup");
        let heap = handles.env.heap;
        let snapshot_owner = store.snapshot()?.ok_or(CliError::Text { what: "the Bodies stage needs a committed store snapshot for lazy expansion texts (engine_bodies.rs)" })?;
        let snapshot: &StoreSnapshot = snapshot_owner.as_ref();
        // In the request's heap through its handle that crosses threads: the body workers insert
        // (a frame is one thread's, and the set's `insert` takes a `'static` allocator).
        let expansion_keys: SharedSet<Key, SharedAlloc<'static>> = SharedSet::new_in("engine bodies expansion text record keys", heap.shared());
        let stopped_out = |stage: &mut BodiesStage<'f>, reach, snapshot_owner, expansion_keys| {
            stage.reach = reach;
            let procs = handles.names.and_then(|inputs| inputs.stage().names.as_ref()).and_then(|names| names.procs());
            let names_keys = handles.names.map(|inputs| inputs.keys());
            Ok(Prepared {
                snapshot: snapshot_owner,
                expansion_keys,
                dep_files: DependencyFiles::new(heap),
                found: None,
                ones: &[],
                outlined: handles.outlined,
                root: handles.root,
                fs: &handles.env.fs,
                board: handles.board,
                clock: handles.clock,
                listings,
                seeds: request_heads,
                names_keys,
                procs,
                heap,
                keep_scratch: planted_keep_scratch(),
                has_unread_outlines: stage.unread.as_ref().is_some_and(|unread| !unread.is_empty()),
                unread_digests: stage.unread_digests.as_ref().map(blaze_own::Share::share),
                plants: planted_rows(),
            })
        };
        if handles.board.stopping().is_some() {
            let reach = self.reach.take();
            return stopped_out(self, reach, snapshot_owner, expansion_keys);
        }
        // 0. What this phase OWES: the files of the entered tasks' bodies (each task's
        // `planned.file`, the outline its `BodyCheck` part is in: `plan` above), by outline
        // index, sorted, each once. Only these files' entries are outlined and keyed by chain
        // (`groups`): `Holders::decide` asks no claim of any other file.
        let mut owed: Vec<u32, &'f Frame> = Vec::with_capacity_in(tasks.len(), frame);
        // A task whose file the request has no outline of is the setup's typed refusal (rule 3:
        // the ready setup reads back every owed file, engine_check.rs `checking`), never a body
        // dropped from what is owed.
        for (idx, task) in tasks.iter().enumerate() {
            if (idx & 255) == 0 && handles.board.stopping().is_some() {
                let reach = self.reach.take();
                return stopped_out(self, reach, snapshot_owner, expansion_keys);
            }
            let at = handles.outlined.binary_search_by(|one| one.name.cmp(&task.planned.file)).map_err(|_| EngineCheckError::Unplaced { body: task.planned.name, what: "its file is none of the request's outlines (the ready setup reads back every owed file: engine_check.rs `checking`)" })?;
            owed.push(u32::try_from(at).unwrap_or(u32::MAX));
        }
        owed.sort_unstable();
        owed.dedup();
        // 0b. WHICH ROW EACH TASK IS (D191): a body's row at one unit, named from the plan's
        // reach exactly as the plan named it (engine_body_plan.rs `task_rows`); taken out for the
        // phase and put back below. No reach: no crate list, so `groups` stops before any member
        // (a task decided over a found plan with no reach is the stop below, rule 3).
        let reach = self.reach.take();
        let rows = match reach.as_ref() {
            Some(reach) => crate::engine_body_plan::task_rows::<Production>(tasks, handles.outlined, reach, heap.alloc()).map_err(|body| EngineCheckError::Unplaced {
                body,
                what: "a body task that is no unit's row of a body of its file under the plan's reach (engine_body_plan.rs `task_rows`): the plan and the Bodies setup disagree",
            })?,
            None => Vec::new_in(heap.alloc()),
        };
        // The rows of bodies two units or more hold, kept for the request's lines: each such
        // row's block names its body and units (engine_lines.rs `VIEW`, [`BodiesStage::lines`]).
        // A row's count is its body's holders (engine_body_plan.rs `TaskRow::rows`: its file's
        // reachers less those its cfg cuts).
        if let Some(reach) = reach.as_ref() {
            // The entered rows plan time kept only because it could not decide their cfg
            // (engine_body_plan.rs `Reach::undecided`): the profile's count, never a quiet guess.
            let undecided = rows.iter().filter(|row| reach.undecided(row.rank, row.cut)).count();
            self.outside.undecided = self.outside.undecided.saturating_add(u32::try_from(undecided).unwrap_or(u32::MAX));
            let shared = self.shared_rows.get_or_insert_with(|| Vec::new_in(heap.alloc()));
            shared.extend(rows.iter().filter(|row| row.rows >= 2).copied());
            shared.sort_unstable_by_key(|row| row.name);
            shared.dedup_by_key(|row| row.name);
        }
        if handles.board.stopping().is_some() {
            return stopped_out(self, reach, snapshot_owner, expansion_keys);
        }
        // 1. Each member's check scope and files, and each owed body's item key, on this thread.
        let plants = planted_rows();
        // The open seam (D168 (2), rule 78): by need, or whole under the oracle's plant. The
        // setup's seam (engine_members_record.rs, rule 78): every member visited (the phase
        // path), or a member none of whose files moved taken from the members record (the ready
        // driver's, [`BodiesStage::stand_members`]).
        let standing = self.outlines_moved.is_some();
        let entered = Entered { tasks, rows: &rows, reach: reach.as_ref() };
        #[cfg(test)]
        let found = match (plants.open_whole, standing) {
            (true, _) => self.groups::<Whole, EveryMember>(&cx, store, read, handles, view, plants, frame, snapshot, &expansion_keys, &owed, entered)?,
            (false, false) => self.groups::<ByNeed, EveryMember>(&cx, store, read, handles, view, plants, frame, snapshot, &expansion_keys, &owed, entered)?,
            (false, true) => self.groups::<ByNeed, StandingMembers>(&cx, store, read, handles, view, plants, frame, snapshot, &expansion_keys, &owed, entered)?,
        };
        #[cfg(not(test))]
        let found = match standing {
            false => self.groups::<ByNeed, EveryMember>(&cx, store, read, handles, view, plants, frame, snapshot, &expansion_keys, &owed, entered)?,
            true => self.groups::<ByNeed, StandingMembers>(&cx, store, read, handles, view, plants, frame, snapshot, &expansion_keys, &owed, entered)?,
        };
        let (found, stopped) = match found {
            Ok(found) => (Some(found), None),
            Err(why) => (None, Some(why)),
        };
        // 2. Each task: a body to check, or its result now.
        let mut ones: Vec<One<'f>, &'f Frame> = Vec::with_capacity_in(tasks.len(), frame);
        // Who checks each body (engine_body_plan.rs `Holders::decide`), and what that is a
        // function of; reused a task.
        // In `Global` as `Holders::decide` takes them (engine_body_plan.rs, moved here unchanged).
        let mut passed: Vec<u32, Global> = Vec::new_in(Global);
        let mut met: Vec<CellId, Global> = Vec::new_in(Global);
        // Each file's bodies' labels, made once a file the first time a body of it is refused
        // with none ([`planned_label`]); at most one entry an outlined file.
        let mut labels: Labels<'f> = Vec::new_in(frame);
        let unit_cells = found.as_ref().map(|f| f.holders.unit_cells(&f.grains, heap.alloc()));
        // Each reach rank's unit among the found units (by its `CrateScope` name), once a phase.
        let mut by_rank: Vec<Option<u32>, blaze_own::Alloc<'static>> = Vec::new_in(heap.alloc());
        if let (Some(found), Some(reach)) = (found.as_ref(), reach.as_ref()) {
            by_rank.extend((0..reach.len()).map(|rank| reach.unit(rank).and_then(|one| found.holders.unit_named(one.unit))));
        }
        if handles.board.stopping().is_some() {
            return stopped_out(self, reach, snapshot_owner, expansion_keys);
        }
        for (idx, task) in tasks.iter().enumerate() {
            if (idx & 255) == 0 && handles.board.stopping().is_some() {
                return stopped_out(self, reach, snapshot_owner, expansion_keys);
            }
            let name = task.planned.name;
            let file = handles.outlined.binary_search_by(|one| one.name.cmp(&task.planned.file)).map_or(u32::MAX, |at| u32::try_from(at).unwrap_or(u32::MAX));
            // Each refusal with the cells it is a function of (plan row A8): its failed row
            // stands on them until one moves. A stop of the whole stage reads what `groups` says
            // it is a function of, including the absent graph. A pack that could not be opened
            // is a machine failure and remains owed.
            let none: &[CellId] = &[];
            let (refused, cells, label): (&'static str, &[CellId], &'f str) = match (stopped, found.as_ref()) {
                (Some((why, reads)), _) => (why, reads, ""),
                (None, None) => ("the stage's crates were not read (engine_bodies.rs `groups`)", none, ""),
                (None, Some(found)) => {
                    use crate::engine_body_plan::{Decision, NO_CLAIM, NO_UNIT};
                    // THE ROW (D191): which body and which unit this task is; decided from that
                    // unit's claims alone, checked in that unit's world (its find's group).
                    let Some(reach) = reach.as_ref() else {
                        return Err(EngineCheckError::Unplaced { body: name, what: "the Bodies stage was handed no reach though it found the crate list (engine_bodies.rs `BodiesStage::plan_reach` is called before the plan: engine_check.rs `checking`)" }.into());
                    };
                    let Some(row) = crate::engine_body_plan::row_named(&rows, name).copied() else {
                        return Err(EngineCheckError::Unplaced { body: name, what: "a body task no row of the plan's reach names (engine_body_plan.rs `task_rows`)" }.into());
                    };
                    passed.clear();
                    met.clear();
                    let decided = match (row.rank, by_rank.get(row.rank as usize).copied().flatten()) {
                        (NO_UNIT, _) => Decision::Unreached,
                        (_, Some(unit)) => found.holders.decide(row.base, task.planned.file, unit, &mut passed, &mut met),
                        // A unit the reach ranks and the setup did not find: named, read nothing
                        // (owed: entered again), never another unit's row (rule 3).
                        (_, None) => Decision::Refused { why: NO_CLAIM, took: None },
                    };
                    let label_of = |took: Option<u32>| took.and_then(|at| found.took.get(at as usize)).map_or("", |one| one.label);
                    let plan_cells = || crate::engine_body_plan::Holders::cells(unit_cells.as_deref().expect("a found plan has its unit cells"), decided, &passed, &met, frame);
                    let ordered = || -> &'f [CellId] {
                        let mut ids = plan_cells();
                        ids.sort_unstable();
                        ids.dedup();
                        ids.into_bump_slice()
                    };
                    match decided {
                        Decision::Taken { took, .. } => {
                            // A row at a view: checked in its own unit's world (the find `took`
                            // is that unit's: `decide` reads its claims alone), so its item keys
                            // carry that unit's prefix, as run and test ask (run_ids.rs
                            // `views_of`). Counted by its verdict in the plan's line.
                            match found.took.get(took as usize).map(|one| (one, one.item)) {
                                Some((one, Ok((group, items)))) => {
                                    if file as usize >= handles.outlined.len() {
                                        return Err(EngineCheckError::Unplaced { body: name, what: "its file is none of the planner's outlines" }.into());
                                    }
                                    let held = plan_cells().into_bump_slice();
                                    ones.push(One { task: *task, items, group, file, label: one.label, place: one.place, held, part: None, costs: one.costs, asked: one.asked });
                                    continue;
                                }
                                // A taken body is one its records hold (`BodyIs::Held` is an
                                // `Ok` item): named if ever not, never skipped (rule 3).
                                Some((one, Err(why))) => (why, ordered(), one.label),
                                None => ("its checking unit's find is gone (engine_body_plan.rs `decide`)", ordered(), ""),
                            }
                        }
                        // Found by its unit's records, refused by them: their cells.
                        Decision::Refused { why, took } => (why, ordered(), label_of(took)),
                        // Its unit does not hold it: not that unit's body, never a refusal. Done
                        // with no finding on the cell that says so (the unit's items cell), so the
                        // unit coming to hold it enters the row again. The plan's note counts the
                        // body only when no reaching unit holds it ([`Holders::outside`]).
                        Decision::Absent { .. } | Decision::NotHeld => {
                            let label = match decided {
                                Decision::Absent { took } => label_of(took),
                                _ => "",
                            };
                            let cells = ordered();
                            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(cells.len(), alloc);
                            for id in cells.iter() {
                                reads.push((*id, view.cell(id)));
                            }
                            // No finding: the file's note is the plan's, in the lines record
                            // (engine_body_plan.rs `Outside::lines`), so the row keeps the certificate.
                            buffer.done.push(Outcome::made(task, crate::engine_body_plan::outside_out(name), SVec::new_in(alloc), reads, 0, false));
                            if let Some(is) = found.holders.outside(row.base, task.planned.file, reach.ranks(task.planned.file).len()) {
                                self.outside.push(row.base, file, is);
                            }
                            self.clean += 1;
                            self.ran.push(Ran::without_check(name, label, file));
                            continue;
                        }
                        // No member crate's records hold the body's file: checked by no crate. It
                        // reads every member's names cell, so a member's task that comes moves it.
                        Decision::Unreached if found.members.is_empty() => ("no member crate's records could be read, so no crate holds this body's file", &found.member_cells[..], ""),
                        Decision::Unreached => {
                            self.ownerless += 1;
                            if let Err(slot) = self.ownerless_files.binary_search(&file) {
                                self.ownerless_files.insert(slot, file);
                            }
                            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::new_in(alloc);
                            for member in found.members.iter() {
                                let id = CellId { of: *member, cell: WHOLE };
                                reads.push((id, view.cell(&id)));
                            }
                            let out = ContentId::of(OWNERLESS_DOMAIN, &name.subject.0);
                            buffer.done.push(Outcome::made(task, out, SVec::new_in(alloc), reads, 0, false));
                            self.clean += 1;
                            self.ran.push(Ran::without_check(name, "", file));
                            continue;
                        }
                    }
                }
            };
            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(cells.len(), alloc);
            for id in cells.iter() {
                reads.push((*id, view.cell(id)));
            }
            let (done, stands) = Outcome::failed(task, reads, alloc);
            buffer.push(done, stands);
            // A body refused before any records named it is printed by the label a checked body
            // prints (its outline item's name, `groups`), never an empty name (rule 19). A row at
            // a view (D191) is labelled by its BASE name, the one its file's outline gives the
            // body: its note then says what its base row's says, and the printer folds the two
            // (engine_lines.rs `ViewFold`), so a grouped line names and counts the body once.
            let label = match label.is_empty() {
                true => {
                    let base = crate::engine_body_plan::row_named(&rows, name).map_or(name, |row| row.base);
                    planned_label(&mut labels, &cx, handles.root, handles.outlined, file, base, frame)
                }
                false => label,
            };
            let note = frame.alloc(BodyNote { said: Said::RefusedAtPlan(refused), head_failed: None });
            self.ran.push(Ran { note: Some(note), ..Ran::without_check(name, label, file) });
        }
        self.outside.finish();
        // The reach is the request's: every later Bodies phase names its rows from it too.
        self.reach = reach;
        // 3. Your code first is the next piece (engine.md 4); today: by member, file, name.
        ones.sort_unstable_by_key(|one| (one.group, one.file, one.task.planned.name));
        let ones: &'f [One<'f>] = ones.into_bump_slice();
        // The memory status's slots sized once for every body to check: a slice has its
        // entry's path and label, so the entries cover the slices `run_phase` cuts.
        handles.board.bodies_mem.prepare(ones, handles.outlined, heap.shared())?;
        let procs = handles.names.and_then(|inputs| inputs.stage().names.as_ref()).and_then(|names| names.procs());
        let names_keys = handles.names.map(|inputs| inputs.keys());
        Ok(Prepared {
            snapshot: snapshot_owner,
            expansion_keys,
            dep_files: DependencyFiles::new(heap),
            found,
            ones,
            outlined: handles.outlined,
            root: handles.root,
            fs: &handles.env.fs,
            board: handles.board,
            clock: handles.clock,
            listings,
            seeds: request_heads,
            names_keys,
            procs,
            heap,
            keep_scratch: planted_keep_scratch(),
            has_unread_outlines: self.unread.as_ref().is_some_and(|unread| !unread.is_empty()),
            unread_digests: self.unread_digests.as_ref().map(blaze_own::Share::share),
            plants,
        })
    }

    /// One Bodies phase: every task entered once, its result in `buffer`.
    #[expect(clippy::too_many_arguments, reason = "a phase's edge: each argument one handle of it")]
    pub(crate) fn run_phase<'a, 'r, 'p>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        read: &mut EngineRecords<'r>,
        handles: &Handles<'_, '_, '_, '_>,
        tasks: &[Task],
        view: View<'_, 'a>,
        buffer: &mut Buffer<'a>,
        times: &mut crate::engine_times::Times,
        alloc: SharedAlloc<'a>,
        frame: &'f Frame,
        request_heads: Option<&crate::engine_seeds::RequestTable<'_>>,
        listings: &crate::engine_heads::Listings,
        paged_shelf: Option<&dyn blaze_ladder::front2::buckets::ShelfSource>,
        at_phase: SharedAlloc<'p>,
    ) -> Result<MadeFacts<'p>, CliError> {
        let cx = cx.span("engine: one bodies phase");
        let began = handles.clock.now();
        let heap = handles.env.heap;
        // 0 to 2: the phase's setup, its plan-time rows in `buffer` ([`BodiesStage::prepare`]).
        let prepared = self.prepare(&cx, store, read, handles, tasks, &view, buffer, request_heads, listings, alloc, frame)?;
        // 3. The bodies to run, by member, file, name ([`Prepared::ones`]).
        let mut ones: Vec<One<'f>, Global> = Vec::new_in(Global);
        ones.extend_from_slice(prepared.ones);
        // D167 (a): an entry of more than one body predicted past `SLICE_BYTES` (one macro
        // call's expansion) is cut into slices each predicted at most `SLICE_BYTES` (a body past
        // it alone is a slice alone), each its own run ([`Part`]); the entry itself is kept whole
        // in `wholes` for the join, its slices in `parts`, `slice_of` naming each whole's slices.
        // Phase arena: dropped with the phase.
        let mut wholes: SVec<'a, One<'f>> = SVec::new_in(alloc);
        let mut parts: SVec<'a, One<'f>> = SVec::new_in(alloc);
        let mut slice_of: SVec<'a, (usize, usize)> = SVec::new_in(alloc);
        ones.retain(|one| {
            if one.items.len() <= 1 || predicted(one.costs, 0, one.items.len()) <= SLICE_BYTES {
                return true;
            }
            let first = parts.len();
            let whole = u32::try_from(one.items.len()).unwrap_or(u32::MAX);
            let mut from = 0usize;
            while from < one.items.len() {
                let mut to = from + 1;
                let mut cost = predicted(one.costs, from, to);
                while to < one.items.len() && cost + predicted(one.costs, to, to + 1) <= SLICE_BYTES {
                    cost += predicted(one.costs, to, to + 1);
                    to += 1;
                }
                let written = one.place.written;
                let written = &written[from.min(written.len())..to.min(written.len())];
                let costs = &one.costs[from.min(one.costs.len())..to.min(one.costs.len())];
                let asked = &one.asked[from.min(one.asked.len())..to.min(one.asked.len())];
                parts.push(One { items: &one.items[from..to], place: Place { written, ..one.place }, part: Some(Part { from: from as u32, whole }), costs, asked, ..*one });
                from = to;
            }
            slice_of.push((first, parts.len()));
            wholes.push(*one);
            false
        });
        // The slices first, each alone; then the rest, a run ending at `RUN` entries or at
        // `SLICE_BYTES` predicted, whichever comes first. They are started the most predicted
        // first (below), so the longest work is begun first.
        let mut runs: Vec<&[One<'f>], Global> = Vec::new_in(Global);
        for part in parts.iter() {
            runs.push(core::slice::from_ref(part));
        }
        // Adaptive run size: when only a small batch of bodies is dirty (e.g. an incremental edit),
        // split finely across available workers so independent bodies run in parallel instead of
        // bunching into 1-3 serial runs of up to `RUN` (32) entries.
        let workers = usize::try_from(handles.workers.get()).unwrap_or(1).max(1);
        let target_runs = workers.saturating_mul(2);
        let max_run = if ones.len() <= target_runs { 1 } else { ones.len().div_ceil(target_runs).min(RUN) };
        let mut from = 0;
        while from < ones.len() {
            let group = ones[from].group;
            let mut to = from;
            let mut cost = 0u64;
            while to < ones.len() && ones[to].group == group && to - from < max_run {
                let next = predicted(ones[to].costs, 0, ones[to].items.len());
                if to > from && cost + next > SLICE_BYTES {
                    break;
                }
                cost += next;
                to += 1;
            }
            runs.push(&ones[from..to]);
            from = to;
        }
        self.runs = u32::try_from(runs.len()).unwrap_or(u32::MAX);
        // Each run's results in the phase's arena: its `Done`s, its bodies' rows and their
        // findings ([`RunOut`]); merged below, never kept on the process heap.
        let mut buffers: SVec<'a, (Buffer<'a>, SVec<'a, Ran<'f>>, RunOut<'a>)> = SVec::with_capacity_in(runs.len(), alloc);
        for _ in 0..runs.len() {
            buffers.push((Buffer::new_in(alloc), SVec::new_in(alloc), RunOut::new(alloc)));
        }
        let Some(found) = prepared.found.as_ref() else {
            // Nothing to run: every task's result is in `buffer` above.
            self.ran.sort_unstable_by_key(|one| one.name);
            return Ok(MadeFacts { bytes: SVec::new_in(at_phase), index: SVec::new_in(at_phase) });
        };
        // The same each phase (found from the same records): kept, not summed.
        self.uncovered = found.uncovered;
        // Each run's counts of its seeded bodies, summed after the join.
        let mut tallies: Vec<crate::engine_seeds::SeedTally, Global> = Vec::new_in(Global);
        tallies.resize(runs.len(), crate::engine_seeds::SeedTally::default());
        // Each run's thread stretch (start, end), for the stage's floor (engine_times.rs
        // `Times::job`): what bounds the stage is how many ran at once, measured, not the workers.
        let mut spans: SVec<'a, (u64, u64)> = SVec::with_capacity_in(runs.len(), alloc);
        spans.resize(runs.len(), (0, 0));
        // The phase's scope records opened by need, one for all its runs (each crate opened
        // once whichever run asks it first); its counts read after the runs end.
        let by_need = prepared.scope_records();
        prepared.pre_open_lang(&cx, &by_need, handles.workers)?;
        {
            let stage_reads = match paged_shelf {
                Some(source) => self.reads_paged(source),
                None => self.reads_view(),
            };
            let shared = prepared.shared(found, stage_reads, &view, &by_need);
            let scope = cx.scope();
            scope.narrow_workers(handles.workers, Class::Foreground);
            // Each run started the most predicted first (ties in plan order), so the longest work
            // is begun first and the stage does not end on a long run started last.
            let mut started = Vec::with_capacity_in(runs.len(), heap.alloc());
            for (at, (((run, (out, ran, run_out)), tally), span)) in runs.iter().zip(buffers.iter_mut()).zip(tallies.iter_mut()).zip(spans.iter_mut()).enumerate() {
                let cost: u64 = run.iter().map(|one| predicted(one.costs, 0, one.items.len())).sum();
                started.push((core::cmp::Reverse(cost), at, run, out, ran, run_out, tally, span));
            }
            started.sort_unstable_by_key(|one| (one.0, one.1));
            // One handle a run: as many as the project's bodies over `RUN`, so in the heap.
            let mut running = Vec::new_in(heap.alloc());
            for (_, _, run, out, ran, run_out, tally, span) in started.into_iter() {
                let shared = &shared;
                let clock = prepared.clock.clone();
                running.push(scope.may_overlap("engine bodies run", BODIES_FRAME, move |cx: &Cx| {
                    if shared.board.stopping().is_some() {
                        shared.board.refused(Stage::Bodies, u64::try_from(run.len()).unwrap_or(u64::MAX));
                        return Ok::<(), Infallible>(());
                    }
                    let began = crate::engine_times::ns(clock.now().since_start());
                    run_bodies(cx, shared, run, &clock, out, ran, run_out, tally, alloc);
                    *span = (began, crate::engine_times::ns(clock.now().since_start()));
                    Ok::<(), Infallible>(())
                }));
            }
            let mut failed = None;
            for handle in running.into_iter() {
                if let Err(error) = handle.join(&cx) {
                    failed.get_or_insert(error);
                }
            }
            scope.join(&cx).map_err(CliError::Pool)?;
            if let Some(error) = failed {
                return Err(CliError::Pool(error));
            }
        }
        // Stopped while the phase ran: `one_phase` calls `stop_point` right after `run_stage`
        // and drops the phase arena without merging, so skip joining tens of thousands of
        // unbegun bodies to stay well within `STOP_BOUND` (rule 18).
        if handles.board.stopping().is_some() {
            self.wall_ns = self.wall_ns.saturating_add(handles.clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)));
            return Ok(MadeFacts { bytes: SVec::new_in(at_phase), index: SVec::new_in(at_phase) });
        }
        self.expansion_demands = prepared.snapshot.expansion_demands();
        self.expansion_keys = prepared.expansion_keys.len() as u64;
        self.add_by_need(by_need.counts());
        for tally in tallies.iter() {
            self.seeds.add(tally);
        }
        for &(start, end) in spans.iter() {
            times.job(crate::engine_times::Step::BodiesRun, start, end);
        }
        // Predicted against measured, a run at a time: what states the slice cap in ms
        // (`SLICE_BYTES` times the ms a byte) and how far a run strays from it.
        for (run, &(start, end)) in runs.iter().zip(spans.iter()) {
            let cost: u64 = run.iter().map(|one| predicted(one.costs, 0, one.items.len())).sum();
            self.cost_fit.add(cost, end.saturating_sub(start));
        }
        // D167 (a): each sliced entry's slices joined into its one result ([`join_slices`]), in
        // place of the slices' runs: from here `buffers` and `runs` are the entries' runs, the
        // joined ones first. Each slice's time is its own task's in the profile, named by its
        // first body ([`BodiesStage::task_label`]); a joined row's time is not pushed again.
        let joined_runs = wholes.len();
        let mut slices_done = buffers.into_iter();
        let mut buffers: SVec<'a, (Buffer<'a>, SVec<'a, Ran<'f>>, RunOut<'a>)> = SVec::with_capacity_in(runs.len() - parts.len() + wholes.len(), alloc);
        let mut group: SVec<'a, (Buffer<'a>, SVec<'a, Ran<'f>>, RunOut<'a>)> = SVec::new_in(alloc);
        for (whole, &(first, end)) in wholes.iter().zip(slice_of.iter()) {
            group.clear();
            group.extend(slices_done.by_ref().take(end - first));
            for (part, (_, ran, _)) in parts[first..end].iter().zip(group.iter()) {
                let name = Name::new(Kind::BodyCheck, part.items[0].0);
                for one in ran.iter() {
                    times.push(name, one.ns);
                }
                let from = part.part.map_or(0, |at| at.from);
                let label = SliceLabel { name, entry: whole.label, from, to: from + part.items.len() as u32, whole: whole.items.len() as u32 };
                self.slices.get_or_insert_with(|| Vec::new_in(frame)).push(label);
            }
            buffers.push(join_slices(whole, &mut group, heap.alloc(), alloc)?);
        }
        buffers.extend(slices_done);
        let mut entry_runs: SVec<'a, &[One<'f>]> = SVec::with_capacity_in(buffers.len(), alloc);
        entry_runs.extend(wholes.iter().map(core::slice::from_ref));
        entry_runs.extend(runs[parts.len()..].iter().copied());
        let made = self.join_runs(&cx, store, buffer, times, &entry_runs, buffers, joined_runs, heap, frame, at_phase)?;
        self.wall_ns = self.wall_ns.saturating_add(handles.clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)));
        Ok(made)
    }

    /// THE BODIES' JOIN, one function both drivers call (rule 30): each run's results merged
    /// into the stage and the phase's `buffer`, `buffers[i]` the results of the bodies of
    /// `runs[i]` (the `One`s whose covered keys its run facts name). Its rows' `Done`s, made
    /// records and owed names appended to `buffer`; each finding into the request's list, each
    /// note made the body's, each clean body's typed record put, each body's run facts made
    /// (engine_body_facts.rs) and handed back; the rows kept in the stage, sorted by name. A run
    /// at `joined_runs` or after has its bodies' times pushed here (one before was a sliced
    /// entry's join, whose slices pushed theirs). The phase path calls it after its runs
    /// ([`BodiesStage::run_phase`]); the `Ready` driver after its drive over the runs its
    /// `BodyCheck` kind sank ([`Prepared::join_ready`]).
    #[expect(clippy::too_many_arguments, reason = "the join's edge: each argument one handle of it")]
    fn join_runs<'a, 'r, 'p>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        buffer: &mut Buffer<'a>,
        times: &mut crate::engine_times::Times,
        runs: &[&[One<'f>]],
        buffers: SVec<'a, (Buffer<'a>, SVec<'a, Ran<'f>>, RunOut<'a>)>,
        joined_runs: usize,
        heap: &'static blaze_own::Heap,
        frame: &'f Frame,
        at_phase: SharedAlloc<'p>,
    ) -> Result<MadeFacts<'p>, CliError> {
        let findings = self.findings.get_or_insert_with(|| Vec::new_in(frame));
        let mut total = findings.len();
        for (_, ran, run) in &buffers {
            // The exclusive end of every row's range must fit in u32. Name the first
            // unrepresentable finding's body before reserving or merging any of the runs.
            if let Some(&(row, ..)) = run.found.get(u32::MAX as usize - total) {
                return Err(EngineCheckError::Unplaced {
                    body: ran[row].name,
                    what: "the Bodies stage exceeds its limit of 4294967295 retained findings",
                }.into());
            }
            total += run.found.len();
        }
        findings.try_reserve_exact(total - findings.len()).map_err(|_| EngineCheckError::Grow { what: "the Bodies stage's findings", had: findings.len() })?;
        let mut made = MadeFacts { bytes: SVec::new_in(at_phase), index: SVec::new_in(at_phase) };
        let mut covers: SVec<'p, [u8; 16]> = SVec::new_in(at_phase);
        // A run's typed records' keys by check, sorted: what each body's run facts name.
        let mut typed_keys: SVec<'p, (Name, Key)> = SVec::new_in(at_phase);
        for (run_at, (mut out, mut ran, mut run_out)) in buffers.into_iter().enumerate() {
            buffer.done.append(&mut out.done);
            buffer.made.append(&mut out.made);
            buffer.owed.append(&mut out.owed);
            if run_at >= joined_runs {
                for one in ran.iter() {
                    times.push(one.name, one.ns);
                }
            }
            for (row, said, head_failed) in run_out.notes {
                let said = said.map_task(|error| {
                    let text = bstd::mem::format_in!(frame, "{error}");
                    &*text.into_bump_str()
                });
                ran[row].note = Some(frame.alloc(BodyNote { said, head_failed }));
            }
            self.clean += run_out.clean;
            if !run_out.data_files.is_empty() {
                let kept = self.data_files.get_or_insert_with(|| Vec::new_in(frame));
                kept.extend(run_out.data_files.iter().map(|(path, answer)| (crate::work::held_str(path.as_str(), frame), *answer)));
            }
            // Each clean body's typed record, put under the key of its bytes (rule 21: a clean
            // body's verdict digest does not move with every column the record holds, so a key
            // of (check, out) met other bytes on the second command; engine_body_record.rs, THE
            // KEY), and its key kept by check for the body's run facts below, which name it.
            // `blaze run` builds from it.
            self.untyped_clean = self.untyped_clean.saturating_add(run_out.untyped_clean);
            self.setup.add(run_out.setup);
            typed_keys.clear();
            for one in run_out.typed.iter() {
                let key = crate::engine_body_record::typed_key(&one.bytes);
                store.put(cx, key, &one.bytes)?;
                self.typed_bytes = self.typed_bytes.saturating_add(one.bytes.len() as u64);
                self.typed.get_or_insert_with(|| Vec::new_in(heap.alloc())).push(key);
                typed_keys.push((one.check, key));
            }
            typed_keys.sort_unstable_by_key(|one| one.0);
            self.items += run_out.items;
            self.item_tasks += run_out.item_tasks;
            // One copy of the run's messages. Each row addresses its contiguous findings in
            // the request's list, including after the rows are sorted by name.
            self.scratch_most = self.scratch_most.max(run_out.scratch_most);
            self.frames.fold(run_out.frames);
            let messages: &'f [u8] = frame.alloc_slice_copy(&run_out.messages);
            let mut from = 0usize;
            for &(body, rule, at, end) in &run_out.found {
                let range = &mut ran[body].found;
                if range.len == 0 {
                    range.start = findings.len() as u32;
                }
                range.len += 1;
                findings.push(BodyFinding { rule, at, message: &messages[from..end] });
                from = end;
            }
            // Each body of the run: its class from its note and its findings' rules, its covered
            // keys from the run's `One` of that name (a body in a run ran unless it has a note),
            // and each fn declared in a body its check reached (`RunOut::locals`, in run order).
            for (at, one) in ran.iter().enumerate() {
                // Its bodies as a program calls them ([`asked_keys`]: a method by its member
                // key), not each library key's first 16 bytes.
                let asked: &[blaze_typeck::ItemKey] = runs[run_at].iter().find(|task| task.task.planned.name == one.name).map_or(&[], |task| task.asked);
                let rules = run_out.found.iter().filter(|found| found.0 == at).map(|found| found.1);
                let rule = note_rule(one.said(), true);
                let class = crate::engine_body_facts::class_of(rule.into_iter().chain(rules));
                let from = run_out.locals.partition_point(|local| local.0 < at);
                let to = run_out.locals.partition_point(|local| local.0 <= at);
                covers.clear();
                covers.extend(asked.iter().map(|key| key.0.to_be_bytes()));
                // A fn declared in a body is always `Free` (front2 tables/task.rs `locals`): its
                // checker key is its library key's first 16 bytes (keys.rs `Keys::local`).
                for item in run_out.locals[from..to].iter().map(|local| &local.1) {
                    covers.push(blaze_ladder::front2::keys::checker_of(*item).0.to_be_bytes());
                }
                covers.sort_unstable();
                covers.dedup();
                let typed = typed_keys.binary_search_by_key(&one.name, |typed| typed.0).ok().map(|at| typed_keys[at].1);
                let from = made.bytes.len();
                crate::engine_body_facts::write(class, &covers, typed, &mut made.bytes)
                    .map_err(|why| EngineCheckError::BodyFacts { body: one.name, why: crate::engine_body_facts::MakeError::Bytes(why) })?;
                made.index.push((one.name, from as u32, made.bytes.len() as u32));
            }
            self.ran.extend(ran.drain(..));
        }
        made.index.sort_unstable_by_key(|row| row.0);
        self.ran.sort_unstable_by_key(|one| one.name);
        Ok(made)
    }

    /// Each member crate's [`Group`] and each body of its OWED files (`owed`: outline indexes,
    /// sorted) as a [`Took`]; `Err((why, reads))` when no body can be checked this command: the
    /// stop is every body's note, and `reads` the cells it is a function of (plan row A8; none:
    /// owed, see each stop).
    ///
    /// What is done per owed file alone: the file's entries are outlined and keyed by chain
    /// (`index_by_chain`, one call a file, so a file's keys and refusals are a function of that
    /// file's entries alone, whichever files a request owes), and its `FileClaim` and `Took`s
    /// made. A file no entered task is in gets no claim: `Holders::decide` reads only the claims
    /// of the file of the task it decides.
    ///
    /// What the WHOLE listing was for (D149 piece I1): the uncovered note ([`Uncovered`],
    /// published whole each request in `plan_lines`) counts every member's bodies no entry
    /// covers, and a block's body is told from an uncovered one only by the listed body whose
    /// block it is, which may be in any file. That is each member's listing row
    /// (engine_member_rows.rs: its uncovered bodies, the bodies a macro wrote by the file entry
    /// their walk ends at, the bodies of blocks by owner), keyed by everything it is a function
    /// of (its unit, its names record, its files' texts, the tool) and read by that key. A
    /// member is opened only when it holds an owed file or its row is not stored (made from its
    /// whole listing, `engine_member_rows::make`, and put); an owed file's own bodies are listed
    /// from that file alone (`list_files_with`). Floor: members opened = the owed files' members.
    ///
    /// THE SETUP'S SEAM (`S`, engine_members_record.rs, rule 78): under [`StandingMembers`] a
    /// member none of whose held or reached files is among the outlines this request moved or
    /// owes is taken from the last request's members record (its unit, head subjects, claims of
    /// the files it reaches and does not hold, row key and uncovered bodies), unvisited: no names
    /// record asked, no environment made, no items record opened, no module tree walked. Every
    /// member visited ([`EveryMember`]) gives the same outputs; both write the record.
    fn groups<'r, O: OpenWorld, S: SetupInputs>(&mut self, cx: &Cx, store: &mut dyn crate::store::Store<'r>, read: &mut EngineRecords<'r>, handles: &Handles<'_, '_, '_, '_>, view: &(impl CellDigests + ?Sized), _plants: RowPlants, frame: &'f Frame, snapshot: &StoreSnapshot, expansion_keys: &SharedSet<Key, SharedAlloc<'static>>, owed: &[u32], asked: Entered<'_>) -> Result<Result<Found<'f, 'r>, Stop<'f>>, CliError> {
        let Entered { tasks, rows, reach: plan_reach } = asked;
        let heap = handles.env.heap;
        let g = heap.alloc();
        let mut tally = OwedListing::default();
        // What this phase asks of the request's reader, and its wall: the setup's counters.
        let asked_before = read.counts();
        let began = handles.clock.now();
        // Set again when this phase finds the members' records whole (the end of this fn).
        self.groups_found = false;
        self.uncovered_note = None;
        // The graph can be read at ABSENT before a task defines it. A graph with no names
        // world holds an empty crate list. Both stops move when a usable graph is published.
        let graph_cell: &'f [CellId] = frame.alloc_slice_copy(&[CellId { of: Name::unit_graph(), cell: WHOLE }]);
        let Some(graph) = handles.graph else {
            return Ok(Err(("this command has no unit graph (no lock file to match), so no crate's names, so no types", graph_cell)));
        };
        let Some(names) = handles.names else {
            return Ok(Err(("this command resolved no crate's names, so no body is checked", graph_cell)));
        };
        // The crate list is the `UnitGraph` record's: its world not made is a function of the
        // graph's cell; a new graph record moves it.
        let Some(list) = self.kept.list(cx, store, &graph, frame)? else {
            return Ok(Err(("the crates' inputs could not be made (names.rs `world`; the `UnitGraph` record's finding says why)", graph_cell)));
        };
        // The setup's reader of scope crates made by need (`scope_by_need`): a member's row and
        // listing ask its own crate, opened already; one past it is opened here only if asked.
        // The runs read through the request's memo instead (`Prepared::scope_records`).
        let setup_records = LazyScopeRecords::new(snapshot, CrateGraph::len(&list), Some(names.keys()), heap);
        let planted_unreadable = self.unreadable;
        // The trees are packs mapped from the shelf and the sources store (body_types.rs
        // `open_trees`): a refusal there is of the machine's files, not of a cell (a pack missing
        // or not the one the list's world read), so it reads nothing and is owed, entered again
        // each request (rule 3: no invented read to make it stand).
        let packs = blaze_own::Box::new_in(crate::body_types::SyncLazyTrees::new(handles.env.home, handles.env.heap, list, frame), handles.env.heap.alloc());
        let corrected = match crate::body_types::corrected_of(&list, handles.env.heap) {
            Ok(corrected) => corrected,
            Err(why) => return Ok(Err((why, &[]))),
        };
        // The setup's laps ([`SetupParts`]): from its start to here, the list and the trees.
        let mut mark = began;
        tally.parts.list_and_trees_ns = lap(handles.clock, &mut mark);
        // The read grains (engine_scope_reads.rs `ReadGrains`): taken from the last request's
        // record where it is valid today, only the crates whose names or tables task this
        // request entered read from the answer again; the record for the next request put under
        // the key of its bytes, which the driver names under the checkout's grains root.
        let stored_grains = match self.grains_last.and_then(|(record, _)| record) {
            Some(key) => store.get(cx, key)?.map(|held| held.bytes),
            None => None,
        };
        let from = crate::engine_scope_reads::GrainsIn {
            graph_out: graph.out,
            stored: stored_grains,
            last_serial: self.grains_last.map_or(0, |(_, serial)| serial),
            entered: self.grains_entered.as_deref().unwrap_or(&[]),
            #[cfg(test)]
            rebuilt: _plants.grains_rebuilt,
            #[cfg(not(test))]
            rebuilt: false,
        };
        let made = ReadGrains::new(list, graph_cell[0], view, from, heap.shared());
        let grains_key = blaze_drive::key::content_key(&made.record);
        store.put(cx, grains_key, &made.record)?;
        self.grains_key = Some(grains_key);
        match self.grains_line.as_mut() {
            Some(line) => line.add(made.line),
            None => self.grains_line = Some(made.line),
        }
        let grains = made.grains;
        tally.parts.grains_ns = lap(handles.clock, &mut mark);
        // THE MEMBERS RECORD (engine_members_record.rs): today's header (the crate list, the
        // tool, the answer this request starts from); under `S::STANDING` the last request's
        // record, opened once, and the outlines this request moved or owes, sorted. A record
        // that does not open (another version, damage) is counted and every member visited: the
        // record is made again below (D160), never read in part.
        use crate::engine_members_record as rec;
        let today = rec::Header { graph_out: graph.out, tool: handles.tool, made_at: self.members_last.map_or(0, |(_, serial)| serial) };
        let stored_members = match (S::STANDING, self.members_last.and_then(|(record, _)| record)) {
            (true, Some(key)) => store.get(cx, key)?.map(|held| held.bytes),
            _ => None,
        };
        let stood_view = match stored_members.map(|bytes| rec::View::open(bytes, g)) {
            Some(Ok(view)) => {
                tally.members_recorded = u32::try_from(view.len()).unwrap_or(u32::MAX);
                Some(view)
            }
            Some(Err(_refused)) => {
                tally.members_record_refused = tally.members_record_refused.saturating_add(1);
                None
            }
            None => None,
        };
        tally.parts.members_record_ns = lap(handles.clock, &mut mark);
        let mut moved: Vec<Name, blaze_own::Alloc<'static>> = Vec::new_in(g);
        if S::STANDING {
            moved.extend(self.outlines_moved.iter().flatten().copied());
            moved.extend(owed.iter().filter_map(|at| handles.outlined.get(*at as usize)).map(|one| one.name));
            moved.sort_unstable();
            moved.dedup();
        }
        // The planted control (engine_ready_edit.rs `CommitPlant::StandingDropsHeads`).
        let drop_standing_heads = self.standing_drops_heads;
        // The planted control of the members' row key (`CommitPlant::RowKeyUnitOnly`).
        let row_key_unit_only = self.row_key_unit_only;
        // This request's record, made at its answer's serial, one member at a time in the crate
        // list's order.
        let mut members_out = rec::Writer::new(&rec::Header { made_at: today.made_at.saturating_add(1), ..today }, g);
        let mut groups: Vec<Group<'f, 'r>, Global> = Vec::new_in(Global);
        let mut took: Vec<Took<'f>, blaze_own::Alloc<'static>> = Vec::new_in(g);
        // The first list that could not grow: the stage's typed stop (rule 3).
        let mut grown: Result<(), EngineCheckError> = Ok(());
        // Each member unit's word on each file its module tree reaches, and the units in the
        // crate list's order (engine_body_plan.rs: who checks each body is decided from these).
        let mut member_files: Vec<FileClaim<'f>, Global> = Vec::new_in(Global);
        let mut units: Vec<crate::engine_body_plan::Unit, Global> = Vec::new_in(Global);
        // A unit whose cfg holds `test` (the crate list's `TEST` flag, written from the world
        // crate's cfg: crate_list.rs `cfg_test`): ranked after the others.
        let world = names.stage().names.as_ref();
        let is_test = |name: Name| world.is_some_and(|names| names.crate_at(name.subject).is_some_and(|at| names.list().test(at)));
        let mut members: Vec<Name, Global> = Vec::new_in(Global);
        // Every member's names cell, with a `Names` task or without: what "no member crate's
        // records could be read" reads.
        let mut member_cells: Vec<CellId, Global> = Vec::new_in(Global);
        member_cells.push(graph_cell[0]);
        let mut uncovered = 0u32;
        // The first of them by label (at most [`MOST_UNCOVERED_NAMED`]), in the request's heap.
        let mut uncovered_named: Vec<&'static str, blaze_own::Alloc<'static>> = Vec::new_in(g);
        let mut heads = SVec::new_in(heap.shared());
        let procs = names.stage().names.as_ref().and_then(|names| names.procs());
        let input_at = |at: usize| names.input_of(blaze_drive::key::crate_scope_name(&list.key(at)));
        // The cells a refusal is a function of, in the request's heap with the bodies' labels:
        // each crate's names cell (`WHOLE`: its `Names` task defines it, or none does and it is
        // read ABSENT, so the task that comes, or the record that changes, moves it) and the
        // graph's, which the crate list and each check scope are made from (rule 21).
        let cells_of = |crates: &[Name]| -> &'f [CellId] {
            let mut ids: Vec<CellId, blaze_own::Alloc<'static>> = Vec::new_in(g);
            ids.push(CellId { of: graph.name, cell: WHOLE });
            for one in crates.iter() {
                ids.push(grains.place(CellId { of: *one, cell: WHOLE }, false));
            }
            ids.sort_unstable();
            ids.dedup();
            frame.alloc_slice_copy(&ids)
        };
        // PIECE A, THE MEMBERS STOOD BELOW THEIR HEADS ([`stands_below`]): a member a moved file
        // is in, each such file's listing facts unmoved (a body edit), is taken from the record
        // as a standing member is, in the list's order, and kept here with its unit. Once every
        // member is taken or visited, each of them is visited only when an entered task is its
        // own row of a file it holds ([`own_row_entered`], D191): a row is decided from its own
        // unit's claims alone, so a file two units hold (a lib and its test harness) is listed by
        // each unit whose row is entered, and by no other. A visit then adds the member's claims
        // alone: its entry, row, head subjects and uncovered bodies are the record's, taken
        // already (`again`: its unit, made at its first turn).
        let mut first_turn = (0..CrateGraph::len(&list)).filter(|at| list.member(*at));
        // The entered tasks' names, sorted: the only finds a decision reads (piece B, the
        // listing's walk by need). Each is a body's row at one unit: a unit's find of a body is
        // asked when the body's name AT THAT UNIT'S VIEW is entered (below, `own_view`).
        let mut entered: Vec<Name, blaze_own::Alloc<'static>> = Vec::with_capacity_in(tasks.len(), g);
        entered.extend(tasks.iter().map(|task| task.planned.name));
        entered.sort_unstable();
        // The holders' views of the body at hand, scratch for `Reach::view_in`.
        let mut views: Vec<blaze_drive::view::ViewKey, blaze_own::Alloc<'static>> = Vec::new_in(g);
        // Each outline's cfg cuts (engine_plan_cfg.rs `PlanCuts`), computed lazily per file on
        // first ask so unvisited files' outlines are never parsed or lexed.
        let mut plan_cuts: Option<crate::engine_plan_cfg::PlanCuts<'_, '_, 'static>> = None;
        let mut below: Vec<(usize, u32), blaze_own::Alloc<'static>> = Vec::new_in(g);
        let mut below_at = 0usize;
        // The units visited at need, to check their claims once every member is done.
        let mut needed: Vec<u32, blaze_own::Alloc<'static>> = Vec::new_in(g);
        let deferral = self.deferral;
        // A held file's listing facts today, by its outline's name: a file with a head alone
        // (engine_member_rows.rs `FileHead::Head`, the part its row key reads), else none.
        let entries_now = |slot: Name| -> Option<ContentId> {
            let at = handles.outlined.binary_search_by(|one| one.name.cmp(&slot)).ok()?;
            let one = &handles.outlined[at];
            match (deferral, crate::engine_member_rows::FileHead::of(one)) {
                (Deferral::ByText, _) => Some(one.outline_source),
                (_, crate::engine_member_rows::FileHead::Head(_)) => Some(crate::engine_listing_record::entries_of(one)),
                (_, crate::engine_member_rows::FileHead::Text(_) | crate::engine_member_rows::FileHead::NotOutlined) => None,
            }
        };
        loop {
            let (member, again) = match first_turn.next() {
                Some(member) => (member, None),
                None => {
                    let Some(&(member, unit)) = below.get(below_at) else { break };
                    below_at += 1;
                    let kept = stood_view.as_ref().and_then(|view| view.member(u32::try_from(member).unwrap_or(u32::MAX)));
                    let rank = plan_reach.and_then(|reach| reach.rank_of(units[unit as usize].name));
                    let visit = deferral != Deferral::NeverVisited && kept.is_none_or(|kept| own_row_entered(kept, rows, rank));
                    if !visit {
                        tally.members_stood = tally.members_stood.saturating_add(1);
                        tally.members_below = tally.members_below.saturating_add(1);
                        continue;
                    }
                    tally.members_needed = tally.members_needed.saturating_add(1);
                    needed.push(unit);
                    (member, Some(unit))
                }
            };
            // This member's laps ([`SetupParts`]): stood, or each part of its visit.
            let mut member_mark = handles.clock.now();
            let own_name = blaze_drive::key::crate_scope_name(&list.key(member));
            let unit_val = crate::engine_body_plan::Unit { name: own_name, prefix: list.key(member).prefix(), test: is_test(own_name) };
            let unit = match again {
                Some(unit) => unit,
                None => {
                    tally.members = tally.members.saturating_add(1);
                    let unit = u32::try_from(units.len()).unwrap_or(u32::MAX);
                    units.push(unit_val);
                    member_cells.push(CellId { of: own_name, cell: WHOLE });
                    unit
                }
            };
            let own_cells = cells_of(&[own_name]);
            // A STANDING member (`S::STANDING`, engine_members_record.rs `View::standing`): the
            // record made over the answer this request starts from, today's crate list and tool,
            // no outline it holds or reaches moved or owed, and its unit and its names input (name
            // and out) today's. Its module tree is the record's: a reach moves only through a
            // reached outline's file parts, and every outline it reaches is unmoved (a moved one
            // visits it), so its claims are made by name, with no outline of it read.
            // Its outputs are the record's, as a visit gives them: its names input among the
            // members, its head subjects, its claims of the files it does not hold, its row (read,
            // not asked: no file of it is owed) and its uncovered bodies; written again whole.
            let place = u32::try_from(member).unwrap_or(u32::MAX);
            // Stood whole, or below its heads (piece A: kept in `below`, counted at its turn).
            let standing = match (again, stood_view.as_ref()) {
                (None, Some(view)) => match stands(view, place, &today, &moved, &unit_val, names) {
                    Some(kept) => Some((kept, false)),
                    None => stands_below(view, place, &today, &moved, &unit_val, names, &entries_now).map(|kept| (kept, true)),
                },
                _ => None,
            };
            if let Some((kept, below_heads)) = standing {
                match &kept.state {
                    rec::MemberState::Held(held) => {
                        if below_heads {
                            below.push((member, unit));
                        } else {
                            tally.members_stood = tally.members_stood.saturating_add(1);
                        }
                        members.push(held.names);
                        if !drop_standing_heads {
                            heads.extend(held.held.iter().map(|h| h.subject(kept.unit.prefix)));
                        }
                        member_files.extend(kept.not_held.iter().map(|file| FileClaim { file: *file, unit, is: FileIs::NotHeld, reads: own_cells }));
                        tally.files = tally.files.saturating_add(u32::try_from(held.held.len()).unwrap_or(u32::MAX));
                        self.member_rows.get_or_insert_with(|| Vec::new_in(g)).push(held.row);
                        uncovered = uncovered.saturating_add(held.uncovered);
                        for label in held.named.iter() {
                            if uncovered_named.len() < MOST_UNCOVERED_NAMED {
                                uncovered_named.push(crate::engine_head_notes::leak(g, format_args!("{label}")));
                            }
                        }
                        members_out.member(kept);
                        tally.parts.stood_ns = tally.parts.stood_ns.saturating_add(lap(handles.clock, &mut member_mark));
                        continue;
                    }
                    rec::MemberState::CutScope(cut) => {
                        tally.members_stood = tally.members_stood.saturating_add(1);
                        members.push(cut.names);
                        if !drop_standing_heads {
                            heads.extend(cut.held.iter().map(|h| h.subject(kept.unit.prefix)));
                        }
                        member_files.extend(kept.not_held.iter().map(|file| FileClaim { file: *file, unit, is: FileIs::NotHeld, reads: own_cells }));
                        tally.files = tally.files.saturating_add(u32::try_from(cut.held.len()).unwrap_or(u32::MAX));
                        members_out.member(kept);
                        tally.parts.stood_ns = tally.parts.stood_ns.saturating_add(lap(handles.clock, &mut member_mark));
                        continue;
                    }
                    rec::MemberState::Refused(why) => {
                        if let Some(words) = why.words() {
                            tally.members_stood = tally.members_stood.saturating_add(1);
                            member_files.extend(kept.not_held.iter().map(|file| FileClaim { file: *file, unit, is: FileIs::Refused(words), reads: own_cells }));
                            members_out.member(kept);
                            tally.parts.stood_ns = tally.parts.stood_ns.saturating_add(lap(handles.clock, &mut member_mark));
                            continue;
                        }
                    }
                }
            }
            // The member's files: what its module tree reaches from its root under what plan
            // time knows of its `test` (engine_body_plan.rs `reached_under`), the walk the
            // planner took (the `Owners` fact `check` reads, blaze-drive owners.rs): each is
            // this member's, checked or refused by name, never in "no crate holds" (rule 3). The
            // atom is the plan's (engine_plan_cfg.rs `test_atom`), not the script run's answer
            // this stage has: the walk must be the plan's reach (below).
            let cuts = plan_cuts.get_or_insert_with(|| crate::engine_plan_cfg::PlanCuts::of(handles.outlined, g));
            // Its found files beside its root (engine_body_plan.rs `FoundEdge`), as the plan's walk took them.
            let found = plan_reach.map_or(&[][..], |plan| plan.found_of(own_name));
            let reach = crate::engine_body_plan::reached_under(handles.outlined, cuts, crate::engine_body_plan::root_file(&list, member), crate::engine_plan_cfg::test_atom(&list, member), found, g).map_err(EngineCheckError::Unoutlined)?;
            // The plan named this unit's rows from its reach (engine_body_plan.rs `Reach`): the
            // walk today must be that reach, or a row would be planned for a file it does not
            // reach, or none for one it does (the ready plan's reach is the last members
            // record's: a `mod` the edit moved is this stop, named, rule 3).
            if let Some(file) = plan_reach.and_then(|plan| plan.disagrees(own_name, &reach)) {
                return Err(EngineCheckError::ReadyHost {
                    task: file,
                    what: "a member's module tree reaches this file, or no longer reaches it, unlike the reach the plan named its body rows from (a `mod` moved since the reach was read: engine_body_plan.rs `Reach`)",
                }
                .into());
            }
            // What the members record keeps of a visit: each held file's head parts and each
            // reached file it does not hold, in the order the claims are made.
            let mut held_kept: Vec<rec::Held<'_>, blaze_own::Alloc<'static>> = Vec::new_in(g);
            let mut not_held_kept: Vec<Name, blaze_own::Alloc<'static>> = Vec::new_in(g);
            let mut refuse_all = |files: &[Name], why: &'static str, reads: &'f [CellId]| member_files.extend(files.iter().map(|file| FileClaim { file: *file, unit, is: FileIs::Refused(why), reads }));
            let Some(own) = input_at(member) else {
                // Its crate's own names failed (a build script, its resolution): that failure
                // is the crate's `CrateScope` row, reported there once; its bodies say so. Read
                // as it is today: no task defines the crate's names cell (ABSENT), or its task
                // failed and defines none; the task that comes, or its fix, moves it.
                let why = if names.failed(own_name) { rec::RefusedWhy::ScopeFailed } else { rec::RefusedWhy::NoNamesTask };
                refuse_all(&reach, why.words().unwrap_or("its crate has no `Names` task this command (engine_names.rs `plan`)"), own_cells);
                if again.is_none() {
                    members_out.member(&rec::Member { at: place, unit: unit_val, state: rec::MemberState::Refused(why), not_held: reach });
                }
                continue;
            };
            // A member visited at need was pushed at its first turn (piece A).
            if again.is_none() {
                members.push(own.name);
            }
            // THE MEMBER'S OWN NAMES RECORD (D168 (2) (a)): one ask, by its key, through the
            // request's reader. Its check scope (every scope crate's names record) is opened
            // below only when the member needs it ([`OpenWorld`]); the oracle arm opens it here,
            // as before. `scope_of` reads the crate's own names record and the check scope the
            // graph makes (blaze-world check_scope.rs): both cells. The cause printed, typed,
            // the crate named (rule 19), in both arms in the same words.
            let krate = CrateGraph::name(&list, member);
            // The scope, its crates past the member left unopened (`Some`: made by need,
            // [`scope_by_need`]; `None`: every crate opened, [`scope_of`]), and its wholeness.
            let mut whole_scope: Option<MadeScope<'r>> = None;
            let own_records = if O::BY_NEED {
                match read.scope_records(cx, store, &own, list.key(member), krate, g) {
                    Ok(records) => records,
                    Err(error) => {
                        let cause = ScopeRefusal::Records { krate, at: member, error };
                        refuse_all(&reach, crate::engine_head_notes::leak(heap.alloc(), format_args!("its crate's check scope was not read: {cause}")), own_cells);
                        if again.is_none() {
                            members_out.member(&rec::Member { at: place, unit: unit_val, state: rec::MemberState::Refused(rec::RefusedWhy::Unstood), not_held: reach });
                        }
                        continue;
                    }
                }
            } else {
                match scope_of(read, cx, store, names, &list, member, heap) {
                    Ok((scope, whole)) => {
                        let own_records = scope.own_records;
                        whole_scope = Some((scope, None, whole));
                        own_records
                    }
                    Err(cause) => {
                        refuse_all(&reach, crate::engine_head_notes::leak(heap.alloc(), format_args!("its crate's check scope was not read: {cause}")), own_cells);
                        if again.is_none() {
                            members_out.member(&rec::Member { at: place, unit: unit_val, state: rec::MemberState::Refused(rec::RefusedWhy::Unstood), not_held: reach });
                        }
                        continue;
                    }
                }
            };
            tally.parts.own_records_ns = tally.parts.own_records_ns.saturating_add(lap(handles.clock, &mut member_mark));
            // The member's environment, its script run's record handed by the Scripts stage:
            // made for every member in both arms (a refused one drops its row in both).
            let env = crate::body_types::owner_env_by(cx, store, &|script| names.input_of(script), &list, member, handles.env.home, frame)?;
            let (env, script, env_refused) = match env {
                Ok((env, script)) => (env, script.map(|(name, _)| name), None),
                Err(why) => (None, None, Some(why)),
            };
            tally.parts.env_ns = tally.parts.env_ns.saturating_add(lap(handles.clock, &mut member_mark));
            // A scope refused at need takes back the member's head subjects (as the oracle arm
            // refuses before the first).
            let heads_at = heads.len();
            // The member's files: its items record's file rows, matched to the planner's outlines.
            let Ok(items) = ItemsRecord::open(own_records.items, g) else {
                // The items record is the crate's names task's: a function of its cell.
                refuse_all(&reach, "its crate's items record does not open (blaze-scope items_record.rs `ItemsRecord::open`)", own_cells);
                if again.is_none() {
                    members_out.member(&rec::Member { at: place, unit: unit_val, state: rec::MemberState::Refused(rec::RefusedWhy::Unstood), not_held: reach });
                }
                continue;
            };
            // Each member's text length of the impls and traits a macro wrote, made the first
            // time a body is asked that no item row holds ([`text_bytes`]).
            let mut member_lens: Option<Vec<(ItemKey, u32), blaze_own::Alloc<'static>>> = None;
            let mut files: Vec<(u32, u32), Global> = Vec::new_in(Global);
            // Each file row's head, by index: what the member's listing row is keyed by besides
            // its names record (engine_member_rows.rs `row_key`).
            let mut file_heads: Vec<crate::engine_member_rows::FileHead, blaze_own::Alloc<'static>> = Vec::with_capacity_in(items.files(), g);
            for file in 0..u32::try_from(items.files()).unwrap_or(u32::MAX) {
                let head = 'head: {
                    let Ok(Some(row)) = items.file_surface(file) else { break 'head crate::engine_member_rows::FileHead::NotOutlined };
                    let Some(path) = row.path.in_package(list.shown_from(member), g) else { break 'head crate::engine_member_rows::FileHead::NotOutlined };
                    let Some(path) = path.strip_prefix("/0/") else { break 'head crate::engine_member_rows::FileHead::NotOutlined };
                    // The outline this unit reaches, of the file's outlines (one a name it is
                    // planned by: a `mod x;` slot and a `#[path]` to `x/mod.rs` are two).
                    let Some(at) = crate::engine_body_plan::outline_at_path(handles.outlined, path.as_bytes(), &reach) else {
                        // A file the request has an outline of but did not read back (the ready
                        // setup's, [`BodiesStage::unread_outlines`]): the phase path would match
                        // it, so a `NotOutlined` head here would key the member's row apart from
                        // the phase path's. The setup's typed refusal, never that row (rule 3).
                        if S::STANDING
                            && let Some(unread) = self.unread.as_ref()
                            && let Some(file) = slot_names(path, g).into_iter().flatten().find(|name| unread.binary_search(name).is_ok())
                        {
                            let root = Name::file(list.root(member).strip_prefix("/0/").unwrap_or(list.root(member)).as_bytes());
                            return Err(EngineCheckError::Unoutlined(crate::engine_body_plan::Unoutlined { file, root }).into());
                        }
                        break 'head crate::engine_member_rows::FileHead::NotOutlined
                    };
                    files.push((file, u32::try_from(at).unwrap_or(u32::MAX)));
                    // A member visited at need (piece A) gave its head subjects at its first
                    // turn, from the record: equal to these (its files' listing facts are).
                    if again.is_none() {
                        heads.push(crate::engine_heads::Subject::new(list.key(member).prefix(), file, &handles.outlined[at]));
                    }
                    crate::engine_member_rows::FileHead::of(&handles.outlined[at])
                };
                file_heads.push(head);
            }
            // The member's files this phase owes: the only ones outlined and keyed below.
            let mut owed_files: Vec<(u32, u32), blaze_own::Alloc<'static>> = Vec::new_in(g);
            owed_files.extend(files.iter().copied().filter(|(_, outline)| owed.binary_search(outline).is_ok()));
            // THE MEMBER'S LISTING ROW (engine_member_rows.rs, D149 piece I1): what the rest of
            // its whole listing was for (the bodies a macro wrote, of blocks, the uncovered
            // note), keyed by its unit, its names record, its files' texts and the tool: all
            // known from its own names record, so its stored row is read before its scope is
            // asked for. Its snapshot's refusal is raised below, only where the row is asked of.
            let row_key = if row_key_unit_only {
                // The planted control: no names record's `out`, no file's head.
                crate::engine_member_rows::row_key(&crate::engine_member_rows::RowKey { unit: own_name, names: Input { out: blaze_drive::key::ABSENT, ..own }, files: &[], tool: handles.tool })
            } else {
                crate::engine_member_rows::row_key(&crate::engine_member_rows::RowKey { unit: own_name, names: own, files: &file_heads, tool: handles.tool })
            };
            let probed = snapshot.get(row_key).map(|got| got.map(|bytes| crate::engine_member_rows::open(bytes, g)));
            tally.parts.files_ns = tally.parts.files_ns.saturating_add(lap(handles.clock, &mut member_mark));
            // Standing member on the ready driver with no `CrateScope` entered this request and no
            // planted unreadable crate: its scope stood whole in the members record, so defer both
            // dependency `names.input_of` (stored page reads) and `present` checks to first ask.
            // On all paths (both `ready` and `phases`) when no unreadable crate is planted, defer
            // upfront `front_load::present` CAS lookups to `LazyScopeRecords::open` on first ask
            // while still checking `names.input_of` (in-memory on `phases`) so failed `CrateScope`
            // dependencies still cut the scope immediately.
            let stood_in_record = again.is_some() || stood_view.as_ref().and_then(|view| view.member(place)).is_some_and(|m| matches!(m.state, rec::MemberState::Held(_)));
            let defer_scope_inputs = stood_in_record
                && names.stood_on().is_some()
                && names.stage().results.is_empty()
                && self.grains_entered.as_deref().unwrap_or(&[]).iter().all(|name| name.kind != Kind::CrateScope)
                && planted_unreadable.is_none();
            let defer_present = planted_unreadable.is_none();
            // D168 (2) (c): the member's check scope, at need ([`OpenWorld`]): its bodies are
            // owed, or its row must be made (not stored, or its bytes do not open), or a crate of
            // its scope has no names input (the refusal `scope_of` words). Else nothing of its
            // scope is opened: no body of it runs, and its row is the stored one.
            let needs_scope = !O::BY_NEED
                || !owed_files.is_empty()
                || !matches!(probed, Ok(Some(Ok(_))))
                || (!defer_scope_inputs && !scope_inputs_whole(names, &list, member, &own_records, g));
            tally.scopes_needed = tally.scopes_needed.saturating_add(u32::from(needs_scope));
            if needs_scope && whole_scope.is_none() {
                // Piece C (a): made by need, the member's own record the one opened; every other
                // scope crate's opened at a body's first question of it (`LazyScopeRecords`).
                match scope_by_need(names, &list, member, own_records, snapshot, planted_unreadable, (defer_scope_inputs, defer_present), heap)? {
                    Ok(made) => whole_scope = Some(made),
                    Err(cause) => {
                        // As the oracle arm refuses it: before any claim or head subject of it.
                        heads.truncate(heads_at);
                        refuse_all(&reach, crate::engine_head_notes::leak(heap.alloc(), format_args!("its crate's check scope was not read: {cause}")), own_cells);
                        if again.is_none() {
                            members_out.member(&rec::Member { at: place, unit: unit_val, state: rec::MemberState::Refused(rec::RefusedWhy::Unstood), not_held: reach });
                        }
                        continue;
                    }
                }
            }
            tally.scopes_opened = tally.scopes_opened.saturating_add(u32::from(whole_scope.is_some()));
            tally.parts.scope_ns = tally.parts.scope_ns.saturating_add(lap(handles.clock, &mut member_mark));
            // The scope's cut, the cells a refusal of it reads, and the member's group: of an
            // opened scope alone. Not opened: no file of it is owed, its row stands, and every
            // crate of its scope has an input (`scope_of` would cut none at an input).
            let (whole, cut_words, cut_missing, scope_cells, group) = match whole_scope {
                Some((mut scope, unopened, whole)) => {
                    let cut_words = scope.cut_cause(&list).map(|cause| crate::engine_head_notes::leak(heap.alloc(), format_args!("its member's {cause}")));
                    let cut_missing = match scope.cut {
                        Some((at, CutWhy::NoNamesRecord { failed })) => Some((blaze_drive::key::crate_scope_name(&list.key(at)), failed)),
                        _ => None,
                    };
                    // The cells a cut scope's refusal reads: the graph's, each scope crate's that
                    // had records and the first that had none (the one whose task comes or is fixed).
                    let mut cut_reads: Vec<Name, Global> = Vec::new_in(Global);
                    cut_reads.push(own_name);
                    cut_reads.extend(scope.scope.iter().copied());
                    cut_reads.extend(scope.cut_name(&list));
                    let scope_cells = cells_of(&cut_reads);
                    for name in &mut scope.scope {
                        #[cfg(test)]
                        if _plants.all_dependencies_settled && *name != own.name {
                            *name = graph.name;
                            continue;
                        }
                        *name = grains.place(CellId { of: *name, cell: WHOLE }, false).of;
                    }
                    scope.scope.sort_unstable();
                    scope.scope.dedup();
                    // Made by need, the member is its one opened crate (`scope_by_need`); a scope
                    // cut before the member has no group to check (the member is refused below).
                    let crates = match (unopened, scope.crates.first().copied()) {
                        (None, _) => Some(GroupCrates::Opened(scope.crates)),
                        (Some(rest), Some(own)) => Some(GroupCrates::ByNeed { own, rest }),
                        (Some(_), None) => None,
                    };
                    (whole, cut_words, cut_missing, scope_cells, crates.map(|crates| Group { list, own: own.name, crates, env, script, lang: scope.lang, scope: scope.scope }))
                }
                None => (true, None, None, own_cells, None),
            };
            // A file the planner's cfg-blind walk reaches that the items record lists at no path
            // of it: this unit's cfg cut its `mod` (engine_body_plan.rs). Not this unit's file,
            // never a refusal: another unit may hold it, or none does and the file says so once.
            // A member visited at need (piece A) made these claims at its first turn, from the
            // record.
            for file in reach.iter().filter(|file| again.is_none() && !files.iter().any(|(_, outline)| handles.outlined[*outline as usize].name == **file)) {
                member_files.push(FileClaim { file: *file, unit, is: FileIs::NotHeld, reads: own_cells });
                not_held_kept.push(*file);
            }
            held_kept.extend(files.iter().map(|&(file, at)| {
                let one = &handles.outlined[at as usize];
                rec::Held { file, slot: one.name, path: one.path.as_slice(), entries: crate::engine_listing_record::entries_of(one) }
            }));
            if again.is_none() {
                tally.files = tally.files.saturating_add(u32::try_from(files.len()).unwrap_or(u32::MAX));
            }
            tally.files_owed =tally.files_owed.saturating_add(u32::try_from(owed_files.len()).unwrap_or(u32::MAX));
            tally.members_owed = tally.members_owed.saturating_add(u32::from(!owed_files.is_empty()));
            let at = u32::try_from(groups.len()).unwrap_or(u32::MAX);
            // A member whose environment could not be made is refused by name, never checked
            // under none (rule 3).
            // The note names the crate that failed (rule 19), once per member, never per body.
            // The cut scope reads the cells it was cut at ([`Scope::cut`]). A refused environment
            // (body_types.rs `owner_env_by`) is a function of the crate list (the graph's cell:
            // the script row, the package version) and of the member's build-script run: that
            // `Script` task's `WHOLE` cell, read ABSENT when its run failed and defined none, or
            // with the digest of the record it refused; its fix or a changed record moves it.
            let empty: &'f [CellId] = &[];
            let refused = match (whole, env_refused) {
                (false, _) => Some((
                    // `scope_of` returns `false` exactly when it cuts the scope, the cause with it.
                    cut_words.unwrap_or("its member's check scope was cut and `scope_of` kept no cut (engine_bodies.rs `scope_of`)"),
                    scope_cells,
                )),
                (true, Some(why)) => {
                    let mut env_reads: Vec<Name, Global> = Vec::new_in(Global);
                    env_reads.push(own_name);
                    env_reads.extend(list.script(member).and_then(crate::native_image::script_name));
                    Some((why, cells_of(&env_reads)))
                }
                (true, None) => None,
            };
            // The member's row, asked only of a member whose scope and environment stand: a
            // refused member lists nothing and says no uncovered body, as before.
            let stored = match refused {
                Some(_) => None,
                None => match probed {
                    Ok(stored) => stored,
                    // The committed snapshot's own refusal: a fault of the machine's store, named
                    // with the member, never read as absent (rule 3).
                    Err(why) => return Err(CliError::Text { what: crate::engine_head_notes::leak(g, format_args!("`{}`: its member listing row {row_key} does not read from the store's snapshot: {why:?} (engine_member_rows.rs)", list.root(member))) }),
                },
            };
            // Opened only when a body of the member is owed, or its row is not in the store:
            // a member none of whose files is owed is READ, never listed (rule 77).
            let open_records = refused.is_none() && (!owed_files.is_empty() || !matches!(stored, Some(Ok(_))));
            // Each owed body's item key, through the member's records (opened here once).
            // Both lanes' inputs (merged 2026-11-18): the planner's outlines for current file
            // ranges, and the request's snapshot for lazy expansion texts.
            // (Not named `files`: that is this group's file list, used below.)
            // `open_records` holds only where the scope was needed, so opened: `group` is there.
            // A scope made by need is read through the setup's own memo (`setup_records`): the
            // row and the listing ask the member's own crate, opened already; a crate past it
            // is opened only if they ask it.
            let opened = match (open_records, group.as_ref()) {
                (true, Some(group)) => {
                    let record_files = ((&*packs, &handles.env.fs), Some(handles.outlined));
                    Some(crate::run_names::names_records_over(cx, handles.root, group.crates.scope(&setup_records), record_files, corrected, procs, std::vec::Vec::new_in(Global), env_in(group.env, frame).ok().flatten(), Some(snapshot), Some(expansion_keys), frame, g))
                }
                _ => None,
            };
            if opened.is_some() {
                tally.members_opened = tally.members_opened.saturating_add(1);
            }
            let (records, refused) = match (refused, opened) {
                (Some(why), _) => (None, Some(why)),
                (None, Some(Ok(opened))) => (Some(opened.records), None),
                // The scope crates' records, each its names task's: a function of their cells.
                (None, Some(Err(_))) => (None, Some(("its member's items record does not open for its edition (run_names.rs `names_records`)", scope_cells))),
                // Not opened: no file of it is owed, and its row stands.
                (None, None) => (None, None),
            };
            let expansions = group.as_ref().and_then(|group| group.crates.own()).map(|own| own.records.expansions);
            // The row: read, or made from the member's WHOLE listing (its fresh check, a moved
            // head or names record, or bytes that did not open: counted), then opened from its
            // own bytes as a stored row is (one path: restore equals fresh).
            // `kept`: whether the store holds this row under its key now (read, or put), so the
            // answer's root names it for the next command (`member_row_keys`).
            let mut kept = false;
            let row = match (stored, records) {
                (Some(Ok(row)), Some(_) | None) if refused.is_none() => {
                    tally.rows.read = tally.rows.read.saturating_add(1);
                    kept = true;
                    // THE ORACLE (tests only): a row read by key is the row today's whole listing
                    // makes, byte for byte, wherever the member's records are open; else its key
                    // missed an input (engine_member_rows.rs THE KEY, rule 21).
                    #[cfg(test)]
                    if let Some(records) = records {
                        let made = crate::engine_member_rows::make(records, expansions.as_ref(), frame, g);
                        let (mut fresh, mut read): (Vec<u8, blaze_own::Alloc<'static>>, Vec<u8, blaze_own::Alloc<'static>>) = (Vec::new_in(g), Vec::new_in(g));
                        crate::engine_member_rows::encode(&made, &mut fresh);
                        crate::engine_member_rows::encode(&row, &mut read);
                        if fresh != read {
                            tally.rows.unequal = tally.rows.unequal.saturating_add(1);
                        }
                    }
                    Some(row)
                }
                (stored, Some(records)) => {
                    let damaged = matches!(stored, Some(Err(_)));
                    let made = crate::engine_member_rows::make(records, expansions.as_ref(), frame, g);
                    let mut bytes: Vec<u8, blaze_own::Alloc<'static>> = Vec::new_in(g);
                    crate::engine_member_rows::encode(&made, &mut bytes);
                    let bytes: &'static [u8] = bytes.leak_static();
                    if damaged {
                        // Other bytes stand under its key: the store refuses a second form (rule
                        // 21), so the row is made each command and counted, never put over.
                        tally.rows.damaged = tally.rows.damaged.saturating_add(1);
                    } else {
                        store.put(cx, row_key, bytes)?;
                        kept = true;
                    }
                    tally.rows.made = tally.rows.made.saturating_add(1);
                    match crate::engine_member_rows::open(bytes, g) {
                        Ok(row) => Some(row),
                        // The bytes just encoded do not open: the form's own bug, named (rule 3).
                        Err(why) => return Err(CliError::Text { what: crate::engine_head_notes::leak(g, format_args!("`{}`: its member listing row as made does not open: {why} (engine_member_rows.rs)", list.root(member))) }),
                    }
                }
                // Refused before its records: no row, no uncovered body said, as before.
                (_, None) => None,
            };
            // A member visited at need (piece A) named its row at its first turn, from the record.
            if kept && again.is_none() {
                self.member_rows.get_or_insert_with(|| Vec::new_in(g)).push(row_key);
            }
            tally.parts.opened_ns = tally.parts.opened_ns.saturating_add(lap(handles.clock, &mut member_mark));
            // The member into this request's members record: one whose row the store holds
            // (`kept`: so its scope and environment stood) and every file row of which an outline
            // matched (a row no outline matched is a file that may appear: never stood on). A
            // member visited at need (piece A) was written at its first turn, from the record.
            let all_outlined = !file_heads.iter().any(|head| matches!(head, crate::engine_member_rows::FileHead::NotOutlined));
            if kept
                && again.is_none()
                && let Some(row) = row.as_ref()
                && all_outlined
            {
                let mut named: Vec<&str, blaze_own::Alloc<'static>> = Vec::with_capacity_in(row.named.len(), g);
                named.extend(row.named.iter().copied());
                members_out.member(&rec::Member {
                    at: place,
                    unit: unit_val,
                    state: rec::MemberState::Held(rec::HeldMember {
                        names: own.name,
                        names_out: own.out,
                        row: row_key,
                        uncovered: row.uncovered,
                        named,
                        held: held_kept,
                    }),
                    not_held: not_held_kept,
                });
            } else if !whole
                && again.is_none()
                && all_outlined
                && let Some((missing, missing_failed)) = cut_missing
            {
                members_out.member(&rec::Member {
                    at: place,
                    unit: unit_val,
                    state: rec::MemberState::CutScope(rec::CutScopeMember {
                        names: own.name,
                        names_out: own.out,
                        missing,
                        missing_failed,
                        held: held_kept,
                    }),
                    not_held: not_held_kept,
                });
            } else if again.is_none() {
                members_out.member(&rec::Member {
                    at: place,
                    unit: unit_val,
                    state: rec::MemberState::Refused(rec::RefusedWhy::Unstood),
                    not_held: reach,
                });
            }
            // The bodies a macro wrote that no entry covers: neither at a file entry nor in a
            // block's module: each NAMED in the request's lines (`plan_lines`), never dropped in
            // silence (rule 3), and counted in the profile. The member's first ones are its
            // row's, in the walk's order, so the note over every member is the same bytes
            // whether its part was read or made. A member visited at need (piece A) said its own at
            // its first turn, from the record.
            if again.is_none()
                && let Some(row) = row.as_ref()
            {
                uncovered = uncovered.saturating_add(row.uncovered);
                for label in row.named.iter() {
                    if uncovered_named.len() < MOST_UNCOVERED_NAMED {
                        uncovered_named.push(crate::engine_head_notes::leak(g, format_args!("{label}")));
                    }
                }
            }
            let mut list_mark = handles.clock.now();
            let index = records.zip(row).map(|(records, row)| {
                // The listed bodies of the member's OWED files alone (front2 project_records.rs
                // `list_files_with`: no other file's item is asked for its text), sorted by file:
                // each owed file's run is keyed by chain on its own (`index_outlined_by_chain`
                // below, in the walk's one outline of the file).
                let mut owed_in: Vec<u32, blaze_own::Alloc<'static>> = Vec::with_capacity_in(owed_files.len(), g);
                owed_in.extend(owed_files.iter().map(|(file, _)| *file));
                let listed = list_files_with(records, Listing::FnsAndInitializers, &owed_in, g);
                let mut mine: blaze_own::AVec<'_, blaze_ladder::front2::project_records::Listed<'_>> = blaze_own::AVec::new_in(g);
                mine.extend(listed.iter().copied().filter(|l| l.entry.is_some_and(|(f, _)| owed_in.contains(&f))));
                mine.sort_unstable_by_key(|l| l.entry.map(|(f, _)| f));
                // The unit's cfg, as the names library lowered the crate (body_types.rs
                // `Checker::holds`): what tells an entry a `cfg` left out from one the records
                // lost ([`absent_entry`]).
                let cfg = NamesSource::cfg_atoms(records, 0).map(|atoms| blaze_scope::task::Cfg::from_atoms(atoms, g));
                // Free fn/const/static items in block modules (e.g. `bstd_hosted_start` inside
                // `bstd::main!`'s launcher `main`): `covered::in_blocks` skips them so each body
                // is checked once via `each_from`, while `program.rs` calls exported block items
                // by their block-module library key. Pre-index `(expansion_key, item_key)` once
                // per member so each body adds its library-key aliases to `covers` in O(log N).
                let mut block_locals: Vec<(ItemKey, ItemKey), blaze_own::Alloc<'static>> = Vec::new_in(g);
                for idx in 0..items.items() {
                    if let Ok(Some(id)) = items.identity_at(idx)
                        && matches!(id.kind, blaze_surface::ItemKind::Function | blaze_surface::ItemKind::Const | blaze_surface::ItemKind::Static)
                        && let Some(blaze_scope::resolve::At::Expansion(exp)) = id.at
                    {
                        block_locals.push((exp, id.key));
                    }
                }
                block_locals.sort_unstable();
                (records, mine, row, cfg, block_locals)
            });
            tally.parts.listing_list_ns = tally.parts.listing_list_ns.saturating_add(lap(handles.clock, &mut list_mark));
            // The member's crate root, by its path: its top-level `fn main` is D104's
            // (body_types/covered.rs `root_of`, `Root::Replaced`).
            let root_path = list.root(member).strip_prefix("/0/").unwrap_or(list.root(member));
            // Only the owed files: a file no entered task is in is asked of by no decision
            // (`Holders::decide` reads the claims of the decided task's file alone).
            for &(file, outline_at) in owed_files.iter() {
                let file_name = handles.outlined[outline_at as usize].name;
                let root_file = handles.outlined[outline_at as usize].path.as_slice() == root_path.as_bytes();
                let Some((records, mine, row, cfg, block_locals)) = index.as_ref() else {
                    // Its file is the member's; its scope could not be read: every body says why.
                    let (why, reads) = refused.unwrap_or((RULE, empty));
                    member_files.push(FileClaim { file: file_name, unit, is: FileIs::Refused(why), reads });
                    continue;
                };
                let records = *records;
                // The bodies a macro wrote whose walk ends at an entry of this file, and the
                // member's bodies of blocks, from its listing row (engine_member_rows.rs).
                let mut written: Vec<&crate::engine_member_rows::Written<'_>, blaze_own::Alloc<'static>> = Vec::new_in(g);
                written.extend(row.written_in(file));
                let blocks = &row.blocks;
                // The file's listed bodies keyed by chain, this file's run of `mine` alone: its
                // keys, and a refusal of them, are a function of this file's entries, never of
                // which other files the request owes (a fresh check and an edit agree).
                let from = mine.partition_point(|l| l.entry.map(|(f, _)| f) < Some(file));
                let run = &mine[from..];
                let run = &run[..run.partition_point(|l| l.entry.map(|(f, _)| f) == Some(file))];
                tally.files_listed = tally.files_listed.saturating_add(1);
                // A body of its file the records do not list, or list without a key, is refused
                // on the member's names cell: the listing is its record's.
                member_files.push(FileClaim { file: file_name, unit, is: FileIs::Held, reads: own_cells });
                // THIS UNIT'S ROWS of this file's bodies (D191): a body's name at this unit's view
                // among the body's holders (the plan's spelling, engine_body_plan.rs
                // `Reach::view_in`, by each entry's cfg cut); the base where the plan has no
                // reach (no rows at a view). An entry this unit's `test` cuts is no row of it.
                let own_rank = plan_reach.and_then(|plan| plan.rank_of(own_name).map(|rank| (plan, rank)));
                let file_cuts = match own_rank {
                    Some(_) => plan_cuts.get_or_insert_with(|| crate::engine_plan_cfg::PlanCuts::of(handles.outlined, g)).file(outline_at as usize),
                    None => None,
                };
                let (chains_ns, entries_keyed) = (&mut tally.parts.listing_chains_ns, &mut tally.entries_keyed);
                let read = records.with_outline(file, g, &mut |text, outline| {
                    // PIECE B: the run keyed by chain in this one outline of the file (front2
                    // project_records.rs `index_outlined_by_chain`, `index_by_chain`'s rows and
                    // refusals with no second outline made).
                    let mut chain_mark = handles.clock.now();
                    let chains = index_outlined_by_chain(file, run, text, outline, &blaze_file_items::item_chain, g);
                    *chains_ns = chains_ns.saturating_add(lap(handles.clock, &mut chain_mark));
                    *entries_keyed = entries_keyed.saturating_add(chains.as_ref().map_or(0, |by| by.rows().len() as u64));
                    let chains = &chains;
                    for (entry, item) in outline.items().iter().enumerate() {
                        let body = work::checked_body(outline, item).is_some();
                        if !body && !work::expansion_entry(outline, item, text) {
                            continue;
                        }
                        let name = work::body_name(file_name, outline, item, text);
                        let own_view = match own_rank {
                            Some((plan, rank)) => {
                                let cut = file_cuts.map_or(crate::engine_plan_cfg::Cut::HOLDS, |cuts| cuts.of(item.range.start));
                                match plan.view_in::<Production, _>(file_name, cut, rank, &mut views) {
                                    Some(view) => view,
                                    None => continue,
                                }
                            }
                            None => blaze_drive::view::UnitView::Base,
                        };
                        // PIECE B, BY NEED: only an entered row is decided (`prepare` asks
                        // `Holders::decide` of each of `tasks` alone, and a decision reads its own
                        // unit's find of its own body alone), so an entry whose row at this unit
                        // no entered task names is found by no one: its keys, label and cost are
                        // not made. The find keeps the body's base name (a claim is by body).
                        if entered.binary_search(&at_view(name, own_view)).is_err() {
                            continue;
                        }
                        // The bodies an expansion at this entry wrote: the entry's task checks
                        // each (one task an entry, planned from text; covered.rs).
                        let top = (file, blaze_surface::ItemIndex(u32::try_from(entry).unwrap_or(u32::MAX)));
                        let by = written.iter().find(|w| w.top == top).map(|w| w.by);
                        // One a body: in the request's heap with its list, never the fixed frame.
                        let shown = crate::item_name::shown_name(item, text);
                        let label = match (shown.is_empty(), by) {
                            (false, _) => bstd::mem::format_in!(g, "{shown}"),
                            (true, Some(by)) => bstd::mem::format_in!(g, "{{the expansion of `{by}!`}}"),
                            (true, None) => bstd::mem::format_in!(g, "{{a macro call}}"),
                        };
                        let label: &'f str = blaze_own::Box::leak_static(label.into_boxed_str());
                        // The entry's bodies, each with where its findings are printed: the
                        // entry's own (`None`: from the item's start), or each body an expansion
                        // at it wrote, by its label as `check` names it (covered.rs
                        // `written_label`); then each body of a block's module whose owner is one
                        // of them (covered.rs `in_blocks`), by its label. Each body is in one
                        // entry's list alone: an entry's own key is listed at its file entry, a
                        // written body's `top` is one entry, a block's body has one owner, and
                        // the three are apart (a listed body with an entry is no macro's, and a
                        // walk that ends in a file is no block's): so checked once (rule 77).
                        let mut keys: Vec<ItemKey, _> = Vec::new_in(g);
                        let mut labels: Vec<Option<Unplaced<'static>>, _> = Vec::new_in(g);
                        // D167 (a): each key's predicted cost, made from the rows once the
                        // keys are found (below).
                        let mut costs: Vec<u32, _> = Vec::new_in(g);
                        // COVERAGE (body_types/covered.rs, the reference): every body of the entry
                        // is in its task's list, or the entry is refused by name; an entry is left
                        // to no unit (`ENTRY_ABSENT`) only when its `cfg` is false for this unit.
                        let own = match chains {
                            Err(_) => Err("its file's bodies could not be indexed by their outline entries (front2 project_records.rs `index_outlined_by_chain`)"),
                            Ok(chains) => 'own: {
                                // 1. The entry's own body, listed at its chain.
                                let listed = if body { chains.get(file, blaze_file_items::item_chain(outline, item, text)) } else { None };
                                if let Some(listed) = listed {
                                    match crate::body_types::covered::body_key(records, listed) {
                                        Some(key) => {
                                            keys.push(key);
                                            labels.push(None);
                                        }
                                        None => break 'own Err("a member the records list has no key of its own"),
                                    }
                                }
                                // 2. Each body an expansion at the entry wrote: an expansion
                                // entry's, and an attribute's at a body entry (its output kept or
                                // replaced the item: D122, the entry's task checks them).
                                for w in written.iter().filter(|w| w.top == top) {
                                    match w.key {
                                        Some(key) => {
                                            keys.push(key);
                                            let shown = bstd::mem::format_in!(g, "{}", w.label);
                                            let shown: &'static str = blaze_own::Box::leak_static(shown.into_boxed_str());
                                            labels.push(Some(Unplaced { label: shown, words: crate::body_types::covered::WRITTEN_UNPLACED }));
                                        }
                                        None => break 'own Err("a member a macro wrote has no key of its own in its crate's records"),
                                    }
                                }
                                // 3. A body entry the records list no body at, and no macro wrote
                                // one at: the root's replaced `fn main`, a `cfg` that is false
                                // (not this unit's), or a refusal naming why (never silent).
                                // An entry written inside an item that an attribute macro took
                                // (`#[async_trait] impl Signer for S { fn address(&self) .. }`):
                                // the Reference, "Procedural Macros", "Attribute macros": the
                                // attribute's output REPLACES the item, so the member's text is
                                // no body of the crate. What the crate holds there is the
                                // output, whose bodies an expansion at that ancestor wrote and
                                // whose entry's task checks them (step 2 at the ancestor). Only
                                // an ancestor some expansion wrote a body at counts: one whose
                                // expansion is not in the records stays refused below.
                                let taken_above = body && keys.is_empty() && {
                                    let all = outline.items();
                                    let mut up = item.parent;
                                    let mut steps = 0usize;
                                    let mut taken = false;
                                    while let Some(p) = up {
                                        if written.iter().any(|w| w.top == (file, p)) {
                                            taken = true;
                                            break;
                                        }
                                        steps += 1;
                                        // Bounded by the outline's length (rule 18).
                                        if steps > all.len() {
                                            break;
                                        }
                                        up = all.get(p.0 as usize).and_then(|e| e.parent);
                                    }
                                    taken
                                };
                                if body && keys.is_empty() && !taken_above {
                                    match absent_entry(records, outline, item, text, root_file, cfg.as_ref(), g) {
                                        Ok(key) => {
                                            keys.push(key);
                                            labels.push(Some(Unplaced { label: REPLACED_MAIN, words: crate::body_types::covered::WRITTEN_UNPLACED }));
                                        }
                                        Err(why) => break 'own Err(why),
                                    }
                                }
                                Ok(())
                            }
                        };
                        let found = match (own, blocks) {
                            (Err(why), _) => Err(why),
                            // Its blocks' bodies could not be told apart: refused by name, never
                            // checked without them (rule 3).
                            (Ok(()), Err(why)) if !keys.is_empty() => Err(why.words()),
                            (Ok(()), _) => {
                                if let Ok(blocks) = blocks {
                                    let owners = keys.len();
                                    for at_owner in 0..owners {
                                        let owner = keys[at_owner];
                                        let from = blocks.partition_point(|b| b.owner < owner);
                                        for b in blocks[from..].iter().take_while(|b| b.owner == owner) {
                                            let shown = bstd::mem::format_in!(g, "{}", BlockLabel { entry: label, name: b.name });
                                            let shown: &'static str = blaze_own::Box::leak_static(shown.into_boxed_str());
                                            keys.push(b.key);
                                            labels.push(Some(Unplaced { label: shown, words: crate::body_types::covered::BLOCK_UNPLACED }));
                                        }
                                    }
                                }
                                // The entry's own body is the outline's item; every other key
                                // is read from the items record's rows, no text read.
                                for (k, key) in keys.iter().enumerate() {
                                    let own_body = k == 0 && labels.first().is_some_and(Option::is_none);
                                    costs.push(if own_body { item.range.len().max(MIN_BODY_BYTES) } else { text_bytes(&items, &mut member_lens, outline, file, *key, g) });
                                }
                                Ok((at, &*keys.leak_static()))
                            }
                        };
                        let place = Place { chain: work::item_chain(outline, item, text), file, item: item.range, written: &*labels.leak_static() };
                        let costs: &'f [u32] = costs.leak_static();
                        // Each key as a program calls it, over the records it was found in.
                        let mut called: Vec<blaze_typeck::ItemKey, _> = Vec::new_in(g);
                        if let Ok((_, keys)) = found {
                            asked_keys(records, keys, &mut called);
                            if !block_locals.is_empty() {
                                let krate = items.key();
                                let mut queue: Vec<ItemKey, _> = Vec::with_capacity_in(keys.len(), g);
                                queue.extend_from_slice(keys);
                                let mut head = 0usize;
                                let limit = block_locals.len().saturating_add(keys.len());
                                while head < queue.len() && head < limit {
                                    let owner = queue[head];
                                    head += 1;
                                    let block = crate::body_types::covered::block_key(krate, owner);
                                    let from = block_locals.partition_point(|(exp, _)| *exp < block);
                                    for &(_, alias) in block_locals[from..].iter().take_while(|(exp, _)| *exp == block) {
                                        let asked_alias = blaze_ladder::front2::keys::asked_key(records, alias);
                                        if called.scan_for(&asked_alias).is_none() {
                                            called.push(asked_alias);
                                        }
                                        // Also include nested locals derived from this block-module
                                        // alias (e.g. `hosted` inside `bstd_hosted_start`, when
                                        // `program.rs` enters `bstd_hosted_start` by its
                                        // block-module library key instead of its `INNER` key).
                                        if let Some(alias_text) = NamesSource::text_of(records, alias)
                                            && let Some(atoms) = NamesSource::cfg_atoms(records, 0)
                                            && let Ok(walk) = blaze_ladder::front2::nested_exports::walk_every(
                                                blaze_ladder::front2::nested_exports::Top {
                                                    text: alias_text.text,
                                                    name: NamesSource::name_of(records, alias).unwrap_or("_"),
                                                    library: Some((krate, alias)),
                                                },
                                                None,
                                                blaze_ladder::front2::nested_exports::Reading {
                                                    edition: items.edition(),
                                                    atoms,
                                                    alloc: g,
                                                },
                                            )
                                        {
                                            for local in walk.locals.iter() {
                                                if let Ok(local_key) = local.key {
                                                    let asked_local = blaze_ladder::front2::keys::checker_of(local_key);
                                                    if called.scan_for(&asked_local).is_none() {
                                                        called.push(asked_local);
                                                    }
                                                }
                                            }
                                        }
                                        if queue.scan_for(&alias).is_none() {
                                            queue.push(alias);
                                        }
                                    }
                                }
                            }
                        }
                        let called: &'f [blaze_typeck::ItemKey] = called.leak_static();
                        if grown.is_ok() {
                            grown = EngineCheckError::push(&mut took, Took { name, label, item: found, place, unit, costs, asked: called }, "the bodies found in their crates' records (engine_bodies.rs `groups`)");
                        }
                    }
                });
                core::mem::replace(&mut grown, Ok(()))?;
                if !read && let Some(last) = member_files.last_mut() {
                    // The file's text as it is now, through its crate's records (front2
                    // project_records.rs `with_outline`: `file_now`, the crate's edition): a
                    // function of the file's outline cell (`WHOLE`, which its text moves), the
                    // member's names cell (the records) and the graph's (the packs): the file
                    // edited, or the records made again, moves it.
                    last.is = FileIs::Refused("its file's text could not be read through its crate's records");
                    last.reads = cells_of(&[own_name, file_name]);
                }
            }
            tally.parts.listing_ns = tally.parts.listing_ns.saturating_add(lap(handles.clock, &mut member_mark));
            // A member not opened has no owed file, so no body of it names its group.
            if let Some(group) = group {
                groups.push(group);
            }
        }
        // A member visited at need (piece A) was taken from the record first: its records must
        // read as the record says, or the record stood a member they refuse. Its claims would then
        // not be a whole visit's (its entry and head subjects were the record's), so the setup
        // stops, naming the file (rule 3), never a body decided over the mix.
        if let Some(claim) = member_files.iter().find(|claim| matches!(claim.is, FileIs::Refused(_)) && needed.contains(&claim.unit)) {
            return Err(EngineCheckError::ReadyHost {
                task: claim.file,
                what: "a member taken from the members record below its heads was visited for an entered body no unit ranked before it holds, and its records refused this file: the record stood a member its records do not read (engine_bodies.rs `groups`, piece A)",
            }
            .into());
        }
        // Each unit's word on each body it found (its base name): a row is decided from its own
        // unit's word alone (engine_body_plan.rs `Holders::decide`, D191); what no unit holds is
        // no unit's.
        let claims = took.iter().enumerate().map(|(at, one)| BodyClaim {
            name: one.name,
            unit: one.unit,
            is: match one.item {
                Ok(_) => BodyIs::Held,
                Err(why) => BodyIs::of_refusal(why),
            },
            took: u32::try_from(at).unwrap_or(u32::MAX),
        });
        let holders = crate::engine_body_plan::Holders::new(units, member_files, claims);
        heads.sort_unstable();
        heads.dedup();
        member_cells.sort_unstable();
        member_cells.dedup();
        // This request's members record, under the key of its bytes; the driver names it under
        // the checkout's members root at the commit (engine_check.rs).
        let mut put_mark = handles.clock.now();
        let members_bytes = members_out.finish();
        let members_key = blaze_drive::key::content_key(&members_bytes);
        store.put(cx, members_key, &members_bytes)?;
        self.members_key = Some(members_key);
        tally.parts.put_ns = lap(handles.clock, &mut put_mark);
        self.uncovered_note = (uncovered > 0).then(|| crate::engine_head_notes::leak(g, format_args!("{}", Uncovered { count: uncovered, named: &uncovered_named })));
        self.groups_found = true;
        let counts = read.counts();
        tally.records_opened = counts.records.saturating_sub(asked_before.records);
        tally.record_bytes = counts.bytes.saturating_sub(asked_before.bytes);
        tally.repeat_asks = counts.hits.saturating_sub(asked_before.hits);
        tally.wall_ns = handles.clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        self.owed_listing.add(tally);
        Ok(Ok(Found { grains, groups, took, holders, members, member_cells, heads, uncovered, trees: (packs, corrected) }))
    }

    /// Body `name`'s block of the request's lines (engine_lines.rs): each finding of its verdict
    /// as `check` files it, placed in its entry's item by the item's chain in its file (a
    /// placed finding: printed, counted and stored as an outline's or a names task's findings
    /// are), then its note when it has one. Nothing when it was not entered. A finding whose
    /// file or edition is not known is a typed stop naming the body, never dropped (rule 3).
    pub(crate) fn lines(&self, name: Name, outlined: &[Outlined<'_>], lines: &mut crate::engine_lines::Lines<'_>, frame: &Frame) -> Result<(), EngineCheckError> {
        let Ok(at) = self.ran.binary_search_by_key(&name, |one| one.name) else { return Ok(()) };
        let one = &self.ran[at];
        // A row of a body two units or more reach (D191) opens its block with its view: the
        // body, its place among the units and each unit's label (engine_lines.rs `VIEW`), so the
        // printer folds the body's rows' equal findings and names the view of the rest.
        let says = one.found.len != 0 || one.said().is_some() || one.note.and_then(|note| note.head_failed).is_some();
        let row = self.shared_rows.as_deref().and_then(|rows| crate::engine_body_plan::row_named(rows, name));
        if let (true, Some(row), Some(reach)) = (says, row, self.reach.as_ref()) {
            let mut units: Vec<crate::engine_lines::ViewUnit<'_>, &Frame> = Vec::new_in(frame);
            // The body's holders (its file's reachers less those its cfg cuts), whose places
            // `row.at` counts among.
            for rank in reach.body_ranks(row.file, row.cut) {
                units.push(reach.label(rank).ok_or(EngineCheckError::Unplaced { body: name, what: "a unit that holds a row of the body has no label in the plan's reach (engine_body_plan.rs `Reach::label`)" })?);
            }
            lines.view(name, row.base, row.at, &units);
        }
        if one.found.len != 0 {
            let file = outlined.get(one.file as usize).ok_or(EngineCheckError::Unplaced { body: name, what: "its file is none of the planner's outlines" })?;
            let edition = blaze_surface::Edition::from_byte(file.edition).ok_or(EngineCheckError::Unplaced { body: name, what: "its file's edition byte is no edition" })?;
            let start = one.found.start as usize;
            let findings = self.findings.as_deref().and_then(|all| all.get(start..start + one.found.len as usize))
                .ok_or(EngineCheckError::Unplaced { body: name, what: "its finding range is outside the Bodies stage's list" })?;
            for found in findings {
                match found.at {
                    FoundAt::Start => lines.placed(name, found.rule, 0, 0, &file.path, one.chain, edition, &found.message),
                    // Printed at the tokens' bytes in the item as it is when printed (print.rs).
                    FoundAt::Tokens(at) => lines.placed_tokens(name, found.rule, at, &file.path, one.chain, edition, &found.message),
                }
            }
        }
        // Its note as records (D164): a noted finding a cause its verdict says (`Said::causes`),
        // its subject the body's file and item; what each counts as, how it is drawn and
        // whether the default view groups it (a refused call's readers, the plan's refusal of a
        // member's bodies: one line a cause) are its rule's row's, never decided here. The own
        // causes come before the calls: the printer counts the body once, at its first.
        if let Some(said) = one.said() {
            let subject = body_subject(one, name, row.map_or(name, |row| row.base), outlined, frame)?;
            said.causes(&mut |cause| lines.noted(name, subject, cause));
        }
        if let Some(first) = one.note.and_then(|note| note.head_failed) {
            lines.note(name, crate::engine_lines::Class::Head, crate::engine_head_notes::seed_words(first.head, first.why, frame).as_bytes());
        }
        Ok(())
    }

    /// Body `name`'s item as written, when it ran.
    pub(crate) fn label(&self, name: Name) -> Option<&'f str> {
        self.ran.binary_search_by_key(&name, |one| one.name).ok().map(|at| self.ran[at].label).filter(|label| !label.is_empty())
    }

    /// A body task's time as the profile names it (engine_times.rs `Label`): a slice of an
    /// entry ([`Part`]) by its entry's item and its bodies' place in it, else the body's item.
    pub(crate) fn task_label(&self, name: Name, into: &mut dyn core::fmt::Write) -> bool {
        if let Some(slice) = self.slices.as_ref().and_then(|slices| slices.iter().find(|slice| slice.name == name)) {
            return into.write_fmt(format_args!("{} (bodies {}..{} of {})", slice.entry, slice.from, slice.to, slice.whole)).is_ok();
        }
        self.label(name).is_some_and(|label| into.write_str(label).is_ok())
    }

    /// Whether body `name` was entered this request (else its row stands from the stored answer).
    pub(crate) fn entered(&self, name: Name) -> bool {
        self.ran.binary_search_by_key(&name, |one| one.name).is_ok()
    }

    /// Whether body `name` is an item the check could not check (refused by this stage or by
    /// the checker), which `check` counts apart from failures (main.rs `UNCHECKED_ITEMS`); a
    /// finding or a task error is a failure instead.
    pub(crate) fn not_checked(&self, name: Name) -> bool {
        self.ran.binary_search_by_key(&name, |one| one.name).is_ok_and(|at| matches!(self.ran[at].said(), Some(Said::Refused(_) | Said::RefusedAtPlan(_) | Said::Unchecked { .. } | Said::Panicked | Said::FrameUsedUp { .. })))
    }

    /// Body `name`'s run facts record's payload (engine_body_facts.rs) onto `into`: its class and
    /// the sorted item keys its task covers, made from what this stage holds of the body.
    ///
    /// THE CLASS is `engine_body_facts::class_of` over the rules of the task's findings (their
    /// rules, as the lines record keeps them) and the one rule its note says: `Said::Unchecked`
    /// is `TYPES_NOT_CHECKED`, `Said::BorrowsUnchecked` is `BORROWS_NOT_CHECKED`, and every
    /// refusal, panic, frame refusal and task fault ([`Said::Task`], [`Said::Panicked`],
    /// [`Said::FrameUsedUp`], [`Said::Refused`]) is `work::NOT_RUN`, since these notes carry no
    /// rule number of their own (only [`RULE`], the file's name). A body that was not checked and
    /// has no note (no unit holds its file, or none holds the file's crate) is `NOT_RUN` too: never
    /// `Clean` for a body nobody checked. So only a body whose check ran to a verdict, with no error
    /// finding and no note, is `Clean`.
    ///
    /// THE COVERED KEYS are the task's `One::asked`: each of its items as a program calls it
    /// ([`asked_keys`]: a fn of an impl or a trait by its member key, any other item by its
    /// library key's first 16 bytes), then each fn its check reached declared in a body
    /// (`checker_of`), sorted, one each. A task refused before its records named its items
    /// covers none.
    ///
    /// `Err(MakeError::NotRan)`: `name` was not entered this request (its record stands).
    ///
    /// The bytes of a body that ran in a run were made at the merge, while its `One` (its items)
    /// was in hand ([`BodiesStage::run_phase`]), so a body row does not hold them; a body that
    /// never reached a run (a file no unit holds, or refused at the plan) has no items and no
    /// check: `NOT_RUN`, with its note's rule when it has one.
    pub(crate) fn write_facts<A: core::alloc::Allocator + Copy>(&self, made: &MadeFacts<'_>, name: Name, into: &mut Vec<u8, A>) -> Result<(), crate::engine_body_facts::MakeError> {
        use crate::engine_body_facts::MakeError;
        if let Ok(at) = made.index.binary_search_by_key(&name, |row| row.0) {
            let (_, from, to) = made.index[at];
            into.extend_from_slice(&made.bytes[from as usize..to as usize]);
            return Ok(());
        }
        let at = self.ran.binary_search_by_key(&name, |one| one.name).map_err(|_| MakeError::NotRan)?;
        let class = crate::engine_body_facts::class_of([note_rule(self.ran[at].said(), false).unwrap_or(work::NOT_RUN)]);
        crate::engine_body_facts::write(class, &[], None, into).map_err(MakeError::Bytes)
    }

    /// Every body's note ([`BodiesStage::note`]) in name order, and how many bodies were
    /// checked with no finding and no note: what the command's tests read.
    pub(crate) fn notes(&self, outlined: &[Outlined<'_>], frame: &Frame) -> (std::vec::Vec<std::string::String, Global>, u32) {
        let mut notes = std::vec::Vec::new_in(Global);
        // D191, A BODY IS ONE ITEM: the rows of a body several units hold are its rows, so a note
        // their rows say in equal words is one note (as the printer folds it, engine_lines.rs
        // `print_with`), by the body's base name; and a body is counted clean once, when every
        // one of its rows is (a row whose unit does not hold the body checks nothing and is
        // clean): `extra` is the clean rows past each such body's one count.
        let shared = self.shared_rows.as_deref();
        let base_of = |name: Name| shared.and_then(|rows| crate::engine_body_plan::row_named(rows, name)).map(|row| row.base);
        let mut said_of: blaze_own::BTreeSet<(Name, &str), &Frame> = blaze_own::BTreeSet::new_in(frame);
        let mut by_base: blaze_own::BTreeMap<Name, (u32, u32), &Frame> = blaze_own::BTreeMap::new_in(frame);
        for one in self.ran.iter() {
            let Some(base) = base_of(one.name) else { continue };
            let counts = by_base.entry(base).or_insert((0, 0));
            counts.0 += 1;
            counts.1 += u32::from(one.note.is_none() && one.found.len == 0);
        }
        let extra: u32 = by_base.values().map(|&(rows, clean)| clean - u32::from(clean == rows && clean != 0)).sum();
        // A query over the bodies' noted findings (D164): each said as `--each` says it, a
        // finding a line. A body whose subject cannot be named says that error instead, never
        // a "?" in its place (rule 19; [`BodiesStage::lines`] stops on it).
        for one in self.ran.iter() {
            let Some(said) = one.said() else { continue };
            let base = base_of(one.name);
            let subject = match body_subject(one, one.name, base.unwrap_or(one.name), outlined, frame) {
                Ok(subject) => subject,
                Err(why) => {
                    notes.push(std::string::String::from(bstd::mem::format_in!(frame, "{why}").as_str()));
                    continue;
                }
            };
            said.causes(&mut |cause| {
                let mut words = bstd::mem::format_in!(frame, "");
                let line = match blaze_report::finding::Finding::of_cause(subject, cause).words(&mut words) {
                    Ok(()) => words,
                    Err(why) => bstd::mem::format_in!(frame, "{why}"),
                };
                // Another row of this body said these words already: one note (D191).
                if let Some(base) = base
                    && !said_of.insert((base, work::held_str(line.as_str(), frame)))
                {
                    return;
                }
                notes.push(std::string::String::from(line.as_str()));
            });
        }
        // A file no unit holds: one note each (engine_body_plan.rs).
        notes.extend(self.outside.notes(outlined, frame));
        // A body in a file no member holds, or no unit's, has no note of its own and was not
        // checked (never by its time: a simulated clock gives a fast body 0 ns).
        (notes, self.clean.saturating_sub(extra).saturating_sub(self.ownerless).saturating_sub(self.outside.len()))
    }

    /// What the stage holds past its bodies' tasks, an upper bound: each body's row, its label's
    /// bytes, its findings (rows and messages) and its note (record and words), summed over the bodies
    /// entered (a label or words shared by many bodies is counted at each). Its floor is
    /// [`HELD_FLOOR`] a body (rule 67); the profile's `bodies:` line says both.
    pub(crate) fn held(&self) -> Held {
        let mut held = Held { bodies: self.ran.len() as u64, labels: 0, findings: 0, words: 0 };
        for one in self.ran.iter() {
            held.labels += one.label.len() as u64;
            if let Some(note) = one.note {
                let words = match &note.said {
                    Said::Task(words) | Said::Refused(words) | Said::RefusedAtPlan(words) | Said::BorrowsUnchecked(words) => words.len(),
                    // The calls' list and each call's words, counted at each body under them.
                    Said::Unchecked { why, calls } => why.len() + core::mem::size_of_val(*calls) + calls.iter().map(|call| call.len()).sum::<usize>(),
                    Said::Panicked | Said::FrameUsedUp { .. } => 0,
                };
                let head = note.head_failed.map_or(0, |first| first.head.len() + first.why.len());
                held.words += (core::mem::size_of_val(note) + words + head) as u64;
            }
        }
        if let Some(findings) = &self.findings {
            for finding in findings {
                held.findings += (core::mem::size_of_val(finding) + finding.message.len()) as u64;
            }
        }
        held
    }

    /// The profile's lines: per body microseconds (sum, median, the 10 slowest with their
    /// paths), bodies per second of CPU, and the phase's wall against its CPU.
    pub(crate) fn profile(&self, outlined: &[Outlined<'_>], workers: NonZeroU32, into: &mut blaze_own::String<&Frame>) {
        use core::fmt::Write as _;
        let mut checked: std::vec::Vec<(u64, Name, u32), Global> = std::vec::Vec::new_in(Global);
        checked.extend(self.ran.iter().filter(|one| one.note.is_none() && one.ns > 0).map(|one| (one.ns, one.name, one.file)));
        checked.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let sum: u64 = checked.iter().map(|one| one.0).sum();
        let median = checked.get(checked.len() / 2).map_or(0, |one| one.0);
        let per_cpu_s = if sum == 0 { 0 } else { (checked.len() as u64).saturating_mul(1_000_000_000) / sum };
        let parallel = if self.wall_ns == 0 { 0.0 } else { sum as f64 / self.wall_ns as f64 };
        let _ = writeln!(into, "\nbodies: {} entered, {} checked, {} refused, {} in files no crate holds; {} run(s) on {} worker(s)", self.ran.len(), checked.len(), self.ran.iter().filter(|one| one.note.is_some()).count(), self.ownerless, self.runs, workers);
        let fit = self.cost_fit;
        if fit.runs > 0 && fit.bytes > 0 {
            let ns_a_byte = fit.ns as f64 / fit.bytes as f64;
            let _ = writeln!(
                into,
                "bodies cost: {} runs, {} bytes of body text predicted, {:.1} ms of thread time: {:.1} us a KiB, so the cap of {} bytes a run is {:.1} ms predicted; the longest run {:.1} ms at {} bytes ({:.1} ms predicted); the densest {:.1} ms at {} bytes ({:.1}x the mean a byte)",
                fit.runs,
                fit.bytes,
                fit.ns as f64 / 1e6,
                ns_a_byte * 1024.0 / 1e3,
                SLICE_BYTES,
                ns_a_byte * SLICE_BYTES as f64 / 1e6,
                fit.longest.0 as f64 / 1e6,
                fit.longest.1,
                ns_a_byte * fit.longest.1 as f64 / 1e6,
                fit.densest.0 as f64 / 1e6,
                fit.densest.1,
                if fit.densest.1 == 0 || ns_a_byte == 0.0 { 0.0 } else { fit.densest.0 as f64 / fit.densest.1 as f64 / ns_a_byte }
            );
        }
        for one in &self.ran {
            if let Some(Said::FrameUsedUp { used, reserve, what, offers, kept, kinds, .. }) = one.said() {
                let _ = writeln!(into, "bodies: frame used up by `{}`: {what}, {used} of {reserve} bytes reserved, offers {offers}, kept {kept} distinct, top kept [{kinds}]", one.label);
            }
        }
        let frame = *into.allocator();
        let mut refused = blaze_own::BTreeMap::new_in(frame);
        for one in self.ran.iter() {
            let Some(note) = one.note else { continue };
            let cause = refused_cause(&note.said, note.head_failed, frame);
            match refused.entry(cause) {
                blaze_own::btree::Entry::Vacant(entry) => {
                    entry.insert(RefusedCause { count: 1, file: one.file });
                }
                blaze_own::btree::Entry::Occupied(mut entry) => {
                    let stats = entry.get_mut();
                    stats.count += 1;
                    stats.file = stats.file.min(one.file);
                }
            }
        }
        let mut ordered = blaze_own::Vec::with_capacity_in(refused.len(), frame);
        ordered.extend(refused.iter().map(|(cause, stats)| (cause, stats)));
        ordered.sort_unstable_by(|(left, left_stats), (right, right_stats)| right_stats.count.cmp(&left_stats.count).then(left.cmp(right)));
        let mut rest = 0usize;
        let mut rest_classes = 0usize;
        for (at, (cause, stats)) in ordered.iter().enumerate() {
            if at < REFUSED_CAUSES_SHOWN {
                let path = outlined.get(stats.file as usize).map_or("?", |one| core::str::from_utf8(&one.path).unwrap_or("?"));
                let _ = writeln!(into, "bodies: refused by cause: {} {} (first: {path})", stats.count, cause);
            } else {
                rest += stats.count;
                rest_classes += 1;
            }
        }
        if rest_classes != 0 {
            let _ = writeln!(into, "bodies: refused by cause: {rest} in {rest_classes} other classes");
        }
        for path in self.ownerless_paths(outlined).iter() {
            let _ = writeln!(into, "bodies: in no member's module tree: {path}");
        }
        // What outlives the bodies' tasks (rule 67: the floor and the ratio), and the most one
        // body's scratch held (reset as each body ends).
        let held = self.held();
        let _ = writeln!(
            into,
            "bodies: held per body {} bytes (floor {HELD_FLOOR}, {:.1}x) over {} bodies; row {}, label {}, findings {}, words {}; a body's scratch at most {} bytes",
            held.per_body(),
            held.per_body() as f64 / HELD_FLOOR as f64,
            held.bodies,
            Held::ROW,
            held.mean(held.labels),
            held.mean(held.findings),
            held.mean(held.words),
            self.scratch_most
        );
        // What the runs' frames held committed (rule 16, rule 67): a scratch's floor is the most
        // one body used, so the ratio is committed over used; a reset keeps up to
        // `SCRATCH_KEEP` and a trim gives the rest back as the next body begins.
        let frames = self.frames;
        let _ = writeln!(
            into,
            "bodies: frames committed: a run's scratch at most {} bytes (floor {} used, {:.2}x; kept {SCRATCH_KEEP} between bodies, trimmed {} times); a stretch's frame at most {} bytes at its end (given back with it)",
            frames.scratch_committed_most,
            self.scratch_most,
            frames.scratch_committed_most as f64 / self.scratch_most.max(1) as f64,
            frames.scratch_trims,
            frames.stretch_committed_most
        );
        // What the plan left to no unit (engine_body_plan.rs): entries, and files, counted; and
        // the rows entered by view (D191): a row is called checked only when its own check ran
        // to a verdict ([`BodiesStage::not_checked`] is false), a view row by its own unit.
        // A row its unit does not hold (or no unit's) checks nothing ([`Ran::without_check`]: no
        // stretch, no note): it is "not", never "checked".
        //
        // THE BODIES, DISTINCT (D191): a body's rows (its base row and each view row) are one
        // body, by its base name: HELD when some unit holds it (a row with a note, or one that
        // ran in a stretch); of those, REFUSED when some row's check did not reach a verdict
        // ([`BodiesStage::not_checked`]), else CHECKED WHOLE; NOT CHECKED when no unit holds it
        // (a file no unit holds, an entry no unit's records hold, no member's tree). So held =
        // checked whole + refused, and held + not checked = the distinct bodies entered.
        let mut by_view = crate::engine_body_plan::RowsByView::default();
        let mut distinct: blaze_own::BTreeMap<Name, (bool, bool), &Frame> = blaze_own::BTreeMap::new_in(frame);
        for one in self.ran.iter() {
            let row = self.shared_rows.as_deref().and_then(|rows| crate::engine_body_plan::row_named(rows, one.name));
            let at_view = row.is_some_and(|row| row.at > 0);
            let held = one.note.is_some() || !matches!(one.stretch, InStretch::No);
            let refused = self.not_checked(one.name);
            let checked = held && !refused;
            match (at_view, checked) {
                (false, true) => by_view.base_checked += 1,
                (false, false) => by_view.base_not += 1,
                (true, true) => by_view.view_checked += 1,
                (true, false) => by_view.view_not += 1,
            }
            let body = distinct.entry(row.map_or(one.name, |row| row.base)).or_insert((false, false));
            body.0 |= held;
            body.1 |= held && refused;
        }
        self.outside.profile(self.ran.len(), by_view, outlined, into, frame);
        let held_bodies = distinct.values().filter(|body| body.0).count();
        let refused_bodies = distinct.values().filter(|body| body.1).count();
        let _ = writeln!(
            into,
            "bodies (distinct): {held_bodies} held, {} checked whole, {refused_bodies} refused, {} not checked",
            held_bodies - refused_bodies,
            distinct.len() - held_bodies
        );
        // What `groups` opened, listed and keyed against what the phase owed (rule 67): the
        // floor is the members holding an owed file and the owed files (each a unit's).
        let o = self.owed_listing;
        let ratio = |did: u64, floor: u64| if floor == 0 { did as f64 } else { did as f64 / floor as f64 };
        let _ = writeln!(
            into,
            "bodies: members opened {} of {}, files listed {} of {}, entries keyed {} (floor: the owed entries' members and files, {} and {}; members {:.1}x, files {:.1}x); member rows read {}, made {}, damaged {}",
            o.members_opened,
            o.members,
            o.files_listed,
            o.files,
            o.entries_keyed,
            o.members_owed,
            o.files_owed,
            ratio(u64::from(o.members_opened), u64::from(o.members_owed)),
            ratio(u64::from(o.files_listed), u64::from(o.files_owed)),
            o.rows.read,
            o.rows.made,
            o.rows.damaged,
        );
        // D168 (2): what the members' world opening asked of the request's reader, each against
        // its floor (rule 67): scopes opened against the members that needed one, member rows
        // against the owed members (the rows of the rest are read by key, small), repeat asks
        // against 0, and the setup's wall beside the bodies' CPU.
        let _ = writeln!(
            into,
            "bodies: world opened by need: scopes opened {} of {} members (floor: the members that needed one, {}; {:.1}x); names records opened {} ({} bytes; floor: the opened scopes' crates, each once); repeat asks {} (floor 0); member rows read {} (floor: the owed members, {}); members stood from the members record {} of {} recorded (below their heads {}; stood below their heads and visited for a body no unit before them holds {}; records refused {}); members' setup wall {} us",
            o.scopes_opened,
            o.members,
            o.scopes_needed,
            ratio(u64::from(o.scopes_opened), u64::from(o.scopes_needed)),
            o.records_opened,
            o.record_bytes,
            o.repeat_asks,
            o.rows.read,
            o.members_owed,
            o.members_stood,
            o.members_recorded,
            o.members_below,
            o.members_needed,
            o.members_record_refused,
            o.wall_ns / 1000,
        );
        // Where that wall went, by part ([`SetupParts`]): what names the setup's cost (rule 17).
        let p = o.parts;
        let _ = writeln!(
            into,
            "bodies: members' setup by part (us): list and trees {}, grains {}, members record {}, stood {}; visited: own records {}, environment {}, files and row probe {}, scope {}, records opened and row {}, listing {} (its list {}, chains {}, entries {}); members record put {}; rest {}",
            p.list_and_trees_ns / 1000,
            p.grains_ns / 1000,
            p.members_record_ns / 1000,
            p.stood_ns / 1000,
            p.own_records_ns / 1000,
            p.env_ns / 1000,
            p.files_ns / 1000,
            p.scope_ns / 1000,
            p.opened_ns / 1000,
            p.listing_ns / 1000,
            p.listing_list_ns / 1000,
            p.listing_chains_ns / 1000,
            p.listing_ns.saturating_sub(p.listing_list_ns).saturating_sub(p.listing_chains_ns) / 1000,
            p.put_ns / 1000,
            o.wall_ns.saturating_sub(p.sum()) / 1000,
        );
        // Piece C (a): the runs' scope crates opened by need (front_load.rs `ScopeRecords`),
        // against their floor (rule 67): each crate a check asked, opened once.
        let b = self.by_need;
        let _ = writeln!(
            into,
            "bodies: scope crates opened by need {} ({} bytes; floor: the crates the checks asked, each once); asks {}",
            b.opened, b.bytes, b.asks,
        );
        // The read grains' facts, against their floor: the crates whose names or tables task ran
        // (engine_scope_reads.rs `GrainsLine`).
        if let Some(line) = self.grains_line {
            let _ = writeln!(into, "{line}");
        }
        let s = self.setup;
        let [first, second, third] = s.largest();
        let _ = writeln!(
            into,
            "bodies: stretch setup {} us over {} stretch(es), before each first body (floor: the asks of its first body alone); largest: {} {} us, {} {} us, {} {} us",
            s.ns / 1000,
            s.stretches,
            first.0,
            first.1 / 1000,
            second.0,
            second.1 / 1000,
            third.0,
            third.1 / 1000,
        );
        // Each item body is in one task's list alone (engine_bodies.rs `groups`), so this sum
        // counts each item body checked this request once (rule 77).
        let items = self.items;
        let tasks = self.item_tasks;
        let _ = writeln!(into, "bodies: {items} item bodies checked in {tasks} task(s); {} body(ies) a macro wrote that no entry covers", self.uncovered);
        let _ = writeln!(into, "bodies: us a body: sum {}, median {}; {} bodies a CPU second; phase wall {} us against {} us of body CPU ({parallel:.1}x)", sum / 1000, median / 1000, per_cpu_s, self.wall_ns / 1000, sum / 1000);
        // What the bodies' recorders were offered and kept (front2 tables/recorder.rs
        // `ReadTally`): a repeat is never pushed, so only `kept` reads are sorted.
        let reads = self.seeds.reads;
        let _ = writeln!(into, "bodies: reads offered {}, kept {}, compares sorting them {} (front2 tables/recorder.rs)", reads.offered, reads.kept, reads.sort_compares);
        // The floor is 1.0 offered per kept (each question distinct, each cell offered once).
        let per_kept = ratio(reads.offered, reads.kept);
        let _ = write!(into, "bodies: reads answered from the run memo {} (not offered), met by the filter {}, kept under a whole names read {}; {per_kept:.2} offered per kept (floor 1.00); most offered by question:", reads.memo, reads.filtered, reads.covered);
        let (most, shown) = reads.most_offering(10);
        for &(slot, n) in &most[..shown] {
            let _ = write!(into, " {} {n};", blaze_ladder::front2::tables::recorder::ReadTally::kind_name(slot));
        }
        let _ = writeln!(into);
        let _ = writeln!(into, "bodies: expansion origin columns opened {} (once per expansion asked per stretch: front2 source.rs `Records::text_marks`)", self.seeds.origins_opened);
        let _ = writeln!(into, "bodies: expansion text record demands {}, distinct keys {} (engine_bodies.rs `StoreSnapshot::get`)", self.expansion_demands, self.expansion_keys);
        // What `blaze run` builds from (blaze-typeck checked_record.rs): a clean body with no
        // record is one `run` must check a second time (floor 0).
        let _ = writeln!(
            into,
            "bodies: typed records put {} ({} bytes); bodies clean with no typed record {} (floor 0)",
            self.typed.as_ref().map_or(0, |keys| keys.len()),
            self.typed_bytes,
            self.untyped_clean,
        );
        for (ns, _, file) in checked.iter().take(10) {
            let path = outlined.get(*file as usize).map_or("?", |one| core::str::from_utf8(&one.path).unwrap_or("?"));
            let _ = writeln!(into, "bodies: slow  {:>9} us  {path}", ns / 1000);
        }
        // The most one body grew its stretch's frame by, and which body (the frame's cap is
        // `BODIES_FRAME`; a stretch ends past `STRETCH_FULL`).
        if let Some(most) = self.ran.iter().max_by(|a, b| a.bytes.cmp(&b.bytes).then(b.name.cmp(&a.name))).filter(|one| one.bytes > 0) {
            let path = outlined.get(most.file as usize).map_or("?", |one| core::str::from_utf8(&one.path).unwrap_or("?"));
            let used_up = self.ran.iter().filter(|one| matches!(one.said(), Some(Said::FrameUsedUp { .. }))).count();
            let _ = writeln!(into, "bodies: largest frame a body used: {} bytes, `{}` in {path} (of {BODIES_FRAME} reserved a stretch, a stretch ends past {STRETCH_FULL}); {used_up} body(ies) used one up", most.bytes, most.label);
        }
        // What the bodies left in their stretches' frames, summed (the guard's line).
        crate::engine_memory::BodyFrames::of(self.ran.iter().map(|one| (one.bytes, one.stretch, one.label))).line(into);
    }
}

/// A body task's row, built here alone: whatever runs the check (a phase's run, the `Ready`
/// driver's worker, D187), a made row and a failed one (a refusal, a panic, a frame's limit)
/// are these two calls, never a hand-built [`Done`].
pub(crate) struct Outcome;

impl Outcome {
    /// The row the check made: its verdict `out`, the cells it defines, what it read and its
    /// findings; `failed` when the body failed by a read (a head with no frozen head).
    pub(crate) fn made<'a>(task: &Task, out: ContentId, defines: SVec<'a, (Cell, ContentId)>, reads: SVec<'a, (CellId, ContentId)>, findings: u32, failed: bool) -> Done<'a> {
        Done { name: task.planned.name, key: task.key, out, defines, reads, findings, failed }
    }

    /// A failed row and how it stands: on its reads while it has any (it is entered again when
    /// one moves), else owed to the next request (a panic, a frame's limit, a refusal before
    /// any read): a failed row on no read would stand on nothing (rule 3).
    pub(crate) fn failed<'a>(task: &Task, reads: SVec<'a, (CellId, ContentId)>, alloc: SharedAlloc<'a>) -> (Done<'a>, Stands) {
        let stands = if reads.is_empty() { Stands::OwedNextRequest } else { Stands::ByReads };
        (Done { reads, ..crate::engine_tasks::failed(task, alloc) }, stands)
    }

    /// A failed row on no read: owed to the next request (a panic, a frame's limit).
    pub(crate) fn owed<'a>(task: &Task, alloc: SharedAlloc<'a>) -> (Done<'a>, Stands) {
        Outcome::failed(task, SVec::new_in(alloc), alloc)
    }
}

/// What every run reads: all read-only, and shared across the phase's workers when `R` is
/// `Sync` (the phase path's [`View`]). Today's digests are read through blaze-engine's
/// [`CellDigests`] alone (by [`Shared::reads`]; [`row_of`], [`widen`]), never a concrete view:
/// the phase driver's [`View`], or the `Ready` driver's recorded host, one body on the thread
/// that runs it (engine_ready_check.rs `HostCells`, D187).
struct Shared<'s, 'a, 'f, 'r, R: ?Sized + CellDigests> {
    grains: &'s ReadGrains<'static>,
    outlined: &'s [Outlined<'s>],
    /// The request's listings of each crate's heads, made once a crate (engine_heads.rs
    /// `Listings`, lane OW1): every stretch's `Owners` places its reads in them.
    listings: &'s crate::engine_heads::Listings,
    root: &'s Dir<ReadWrite>,
    fs: &'s bstd::fs::Fs,
    packs: &'s crate::body_types::SyncLazyTrees<'f>,
    corrected: &'s [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    groups: &'s [Group<'f, 'r>],
    snapshot: &'s StoreSnapshot,
    expansion_keys: &'s SharedSet<Key, SharedAlloc<'static>>,
    heads: &'s crate::engine_heads::Subjects,
    /// Today's digests ([`CellDigests`]): the phase's view, or the `Ready` driver's record.
    reads: &'s R,
    /// The runs' rows are made in `'a` (the phase's [`Buffer`]s); no field holds it now that the
    /// view is behind [`CellDigests`].
    rows_in: core::marker::PhantomData<&'a ()>,
    heap: &'static blaze_own::Heap,
    /// Every crate's impl tables (the Heads stage's), read only.
    shelf: &'s ImplShelf,
    /// Where a crate the shelf lacks is read from ([`StageReads`]'s `source`).
    source: Option<&'s dyn blaze_ladder::front2::buckets::ShelfSource>,
    /// The request's frozen heads (engine_seeds.rs), read only but for its insert-only maps;
    /// `None`: each body lowers its heads in its own store (`HeadsFrom::Lowered`).
    seeds: Option<&'s crate::engine_seeds::RequestTable<'s>>,
    /// Every head not frozen (engine_head_notes.rs).
    heads_failed: &'s crate::engine_head_notes::Failed,
    /// How a body's names reads are kept (engine_scope_reads.rs).
    scope_reads: crate::engine_scope_reads::ScopeReadsAs,
    board: &'s crate::engine_status::Board<'s>,
    /// PLANTED BUG when true, tests only ([`PLANT_KEEP_SCRATCH`]): a body's scratch is not
    /// reset when it ends.
    keep_scratch: bool,
    /// True when the ready setup left standing dependency outlines unread (`BodiesStage::unread`),
    /// so `run_stretch` passes `None` for outlines and lets `StoreLoad::file_items` outline any
    /// dependency file actually queried on demand (`CurrentFile::from_text`).
    has_unread_outlines: bool,
    unread_digests: Option<&'s [(blaze_own::String<SharedAlloc<'static>>, ContentId)]>,
    /// PLANTED BUGS, tests only ([`RowPlants`]): the old row's capacity, or the coarse step off.
    plants: RowPlants,
    /// The request's scope records opened by need ([`BodyRunner::by_need`]).
    by_need: &'s dyn crate::front_load::OpenByNeed,
    /// Outlined dependency files cached once per phase across stretches.
    dep_files: &'s DependencyFiles,
}

impl<R: ?Sized + CellDigests> Shared<'_, '_, '_, '_, R> {
    fn record_memory(&self, one: &One<'_>, mem: &BodyMem) {
        self.board.bodies_mem.add(mem, one.task.planned.name, one.path(self.outlined), one.label);
    }

    fn missing_data(&self, label: &str, path: &str, id: CellId) -> Option<&'static str> {
        (self.reads.row_failed(id.of) != Some(false)).then(|| {
            leak(self.heap.alloc(), format_args!("body `{label}` read `{path}` without an available FileItems record"))
        })
    }

    /// A recorder that filled, or a fact with no cell, cannot certify its prefix. Names
    /// records alone miss edits inside an existing item (FileItems::SURFACE did not move).
    /// Cover each checkout file in the scope, the graph, names, impl tables, script and
    /// holder selection. Includes can lie outside those files, even when read at ABSENT.
    /// Build this proof outside both worker frames so exhausting either can still file it.
    fn refused_scope<'a>(&self, one: &One<'_>, group: &Group<'_, '_>, extra: impl Iterator<Item = CellId>, alloc: SharedAlloc<'a>) -> Done<'a> {
        let whole = |of| CellId { of, cell: WHOLE };
        let mut cells = Vec::new_in(self.heap.alloc());
        cells.push(whole(Name::unit_graph()));
        cells.extend(group.scope.iter().copied().map(whole));
        cells.extend(group.script.map(whole));
        for key in group.crates.keys() {
            cells.push(self.grains.place(whole(blaze_ladder::front2::buckets::impl_tables_name(key)), false));
            let prefix = key.prefix();
            let at = self.heads.partition_point(|file| file.prefix < prefix);
            cells.extend(self.heads[at..].iter().take_while(|file| file.prefix == prefix).map(|file| whole(file.slot)));
        }
        cells.extend_from_slice(one.held);
        cells.extend(extra);
        cells.sort_unstable();
        cells.dedup();
        let mut reads = SVec::with_capacity_in(cells.len(), alloc);
        reads.extend(cells.iter().map(|id| (*id, self.reads.cell(id))));
        Outcome::failed(&one.task, reads, alloc).0
    }
}

/// A gauge that reads nothing: a body's [`Cost`] counts its rounds alone here (the profile's
/// spans time the bodies).
struct NoGauge;

struct BodyReadKeyer<'g, 'a, 't, 'n> {
    grains: &'g ReadGrains<'a>,
    fine_buckets: bool,
    ids: core::cell::RefCell<&'g mut ReadIds<'t, 'n, 'a>>,
}

impl ReadKeyer for BodyReadKeyer<'_, '_, '_, '_> {
    fn keeps_item_keys(&self) -> bool {
        // ReadGrains maps only CrateScope and ImplBucket. Head and source questions keep
        // their item and part until Owners or source_read places the completed batch.
        true
    }

    fn key(&self, _: Tables<'_, '_>, read: TableRead) -> Result<Option<PlacedCell>, TaskError> {
        let Some(id) = self.ids.borrow_mut().id(read)? else { return Ok(None) };
        let id = self.grains.place(id, self.fine_buckets);
        Ok(Some(PlacedCell { kind: id.of.kind as u8, subject: id.of.subject.0, cell: id.cell.0 }))
    }
}

impl Gauge for NoGauge {
    fn now_ns(&self) -> u64 {
        0
    }

    fn bytes(&self) -> u64 {
        0
    }
}

static NO_GAUGE: NoGauge = NoGauge;

/// What a seed's freeze error says of the head a body needed ([`unmade_head`]).
enum Unmade {
    /// The head of this cell could not be made: the body is not checked, its note naming it.
    Head(Want),
    /// A head could not be made and the error names no cell of an item (an anonymous
    /// constant's key): not checked, its note the error.
    Unnamed,
    /// The engine's own inconsistency, not the head's: the body's failure.
    Engine,
}

/// The head a seed's freeze could not make, by the error's kind. Every variant is named (no
/// `_` arm), so a new one is classed here when it is added (rule 76).
fn unmade_cell(error: &FreezeError) -> Unmade {
    match error {
        FreezeError::Fill { want, .. }
        | FreezeError::Head { want, .. }
        | FreezeError::Const { want, .. }
        | FreezeError::Record { want, .. }
        | FreezeError::TooBig { want, .. }
        | FreezeError::NoText { want }
        | FreezeError::Steps { want, .. } => Unmade::Head(*want),
        // The body read this cell. Its retained direct need may lead to a different root.
        FreezeError::Again { want, .. } => Unmade::Head(*want),
        // A member's head not frozen for this body's unit (engine_seeds.rs `StretchHeads`):
        // seeded as `SeedError::NotInItsUnit`/`NoMemberCell`, here only as its kind.
        FreezeError::NotTheirs { want, .. } => Unmade::Head(*want),
        FreezeError::TooDeep { outermost, .. } => Unmade::Head(*outermost),
        FreezeError::NoWant { .. } => Unmade::Unnamed,
        // A cell asked that is no cell, bytes that do not read, or the driver's own signal
        // escaping it: faults of the engine, never of the project's head.
        FreezeError::NotACell { .. } | FreezeError::Read { .. } | FreezeError::Unfrozen { .. } => Unmade::Engine,
    }
}

/// Explain an ask using the same root for its words and its counter. Direct errors from
/// outside the freezer (for example, a failed import) are still classed by the caller.
pub(crate) fn unmade_head<E>(
    asked: Want,
    step: impl FnMut(Want) -> Option<Step>,
    label: impl Fn(Want) -> &'static str,
    tally: &mut crate::engine_seeds::SeedTally,
    scratch: impl core::alloc::Allocator + Copy,
    g: blaze_own::Alloc<'static>,
) -> Said<E> {
    let cause = root_cause(asked, step, &label, scratch, g);
    if cause.kind == RootKind::Constant {
        tally.refused_const = tally.refused_const.saturating_add(1);
    }
    let words = leak(g, format_args!("not checked because the head of `{}` it reads could not be frozen: {}", label(asked), cause.words));
    match cause.kind {
        RootKind::Engine => Said::Refused(words),
        RootKind::Head | RootKind::Constant => Said::Unchecked { why: words, calls: &[] },
    }
}

/// A root in the stretch's freezer. Its first publication has the words used to group
/// dependency heads across workers. Even past the publication cap the source can explain it.
fn seed_cause<'n>(
    cell: Want,
    failure: CellFailure<'_>,
    table: Option<&crate::engine_seeds::RequestTable<'_>>,
    records: &'n blaze_ladder::front2::source::Records<'n>,
    grouped: &mut Option<crate::engine_seeds::FirstFailure>,
    g: blaze_own::Alloc<'static>,
) -> Step {
    use crate::engine_seeds::FirstFailed;
    let CellFailure::Own(error) = failure else { return Step::retained(cell, failure, records, g) };
    let cause = match table.map(|table| table.first_failure(cell)) {
        Some(FirstFailed::Kept(first)) => {
            let cause = Cause::from_reason(error, first.why, records, g);
            if cause.kind != RootKind::Engine {
                *grouped = Some(first);
            }
            cause
        }
        Some(FirstFailed::PastCap { kept }) => {
            let mut cause = Cause::own(error, records, g);
            cause.words = leak(g, format_args!("{} (the request's table of {kept} failed heads is full)", cause.words));
            cause
        }
        Some(FirstFailed::None) | None => {
            let mut cause = Cause::own(error, records, g);
            cause.words = leak(g, format_args!("{} (the request has no entry for the stretch's failed head)", cause.words));
            cause
        }
    };
    Step::End(cell, cause)
}

/// A stretch's frame past this many bytes takes no further body: the next body opens its
/// records into a new frame. A body's local items' rows live in the frame until the records go
/// (`check_body` borrows them for the records' lifetime; no question copies into it any more:
/// front2 source.rs `Records::walk`, `VariantRows`), so a frame is reset by ending its stretch, never per body; half the reserve leaves the next body the
/// other half (tempo, g124650: three 32-body runs used up the whole 1 GiB in one frame).
const STRETCH_FULL: usize = BODIES_FRAME / 2;

/// One run: its bodies checked in stretches, each over a frame of its own that ends with it
/// ([`STRETCH_FULL`]), so no run uses up a frame however many bodies it holds.
#[expect(clippy::too_many_arguments, reason = "a run's edge: each argument one handle of it")]
fn run_bodies<'a, 'f, R: ?Sized + CellDigests>(cx: &Cx, shared: &Shared<'_, 'a, '_, '_, R>, run: &[One<'f>], clock: &Clock, out: &mut Buffer<'a>, ran: &mut SVec<'a, Ran<'f>>, run_out: &mut RunOut<'a>, tally: &mut crate::engine_seeds::SeedTally, alloc: SharedAlloc<'a>) {
    #[cfg(test)]
    crate::engine_check::stack_probe::at("run_bodies");
    // Filing a refusal must work even when the allocation for the successful row failed.
    // Each body consumes at most one slot in each of these four lists.
    out.done.reserve_exact(run.len());
    out.owed.reserve_exact(run.len());
    ran.reserve_exact(run.len());
    run_out.notes.reserve_exact(run.len());
    // The bodies' scratch: one reservation a run, reset as each body ends (rule 11).
    // (Its own tag, as the stretch's below: the status's `memory by tag` line names each.)
    let Ok(mut scratch) = Frame::with_reserve(BODY_SCRATCH, bstd::mem::Tag::Diagnostics) else {
        refuse::<R>(run, "the scratch frame of its run's bodies could not be reserved (engine_bodies.rs `BODY_SCRATCH`)", None, out, ran, run_out, alloc);
        return;
    };
    let mut at = 0;
    while at < run.len() {
        let rest = &run[at..];
        // Stopped (engine_status.rs): the bodies not begun are refused, each named, and the
        // driver stops before the merge.
        if shared.board.stopping().is_some() {
            shared.board.refused(Stage::Bodies, rest.len() as u64);
            refuse::<R>(rest, "not checked: the command was stopped by a signal before this body began (engine_status.rs)", None, out, ran, run_out, alloc);
            return;
        }
        let took = match Frame::with_reserve(BODIES_FRAME, bstd::mem::Tag::Infer) {
            Ok(stretch) => {
                let took = run_stretch(cx, shared, rest, &stretch, &mut scratch, clock, out, ran, run_out, tally, alloc);
                // Its records' pages go as the stretch ends, not the run (the frame's drop: what
                // its thread's shelf has no room for goes back, blaze-mem frame.rs `give_span`).
                run_out.frames.stretch_committed_most = run_out.frames.stretch_committed_most.max(stretch.committed() as u64);
                took
            }
            Err(_) => refuse::<R>(rest, "the frame of its run's stretch could not be reserved (engine_bodies.rs `BODIES_FRAME`)", None, out, ran, run_out, alloc),
        };
        at += took.max(1);
    }
}

/// Every body of `rest` refused for one reason, each named (rule 3): how many it took (all).
fn refuse<'a, 'f, R: ?Sized + CellDigests>(rest: &[One<'f>], why: &'static str, scope: Option<(&Shared<'_, 'a, '_, '_, R>, &Group<'_, '_>)>, out: &mut Buffer<'a>, ran: &mut SVec<'a, Ran<'f>>, run_out: &mut RunOut<'a>, alloc: SharedAlloc<'a>) -> usize {
    for one in rest {
        if let Some((shared, group)) = scope {
            let done = shared.refused_scope(one, group, core::iter::empty(), alloc);
            let mut mem = BodyMem { scope: group.crates.len() as u64, ..BodyMem::default() };
            mem.count_scope(&done);
            mem.count_done(&done);
            shared.record_memory(one, &mem);
            out.push(done, Stands::ByReads);
        } else {
            let (done, stands) = Outcome::owed(&one.task, alloc);
            out.push(done, stands);
        }
        run_out.notes.push((ran.len(), Said::Refused(why), None));
        ran.push(Ran::without_check(one.task.planned.name, one.label, one.file));
    }
    rest.len()
}

/// D167 (a): the runs of one entry's slices ([`Part`], in item order, one row each) joined into
/// the run one run of the whole entry would have made, so its key, its out and its row's shape
/// are an unsliced entry's: out the digest of every body's verdict in item order under
/// [`ENTRY_DOMAIN`] (the slices' [`RunOut::outs`]), the verdict cell defined unless a slice
/// cleared it (a head with no frozen head), findings summed, failed if any slice failed, the
/// reads the union (each cell once, sorted: each slice's row holds today's digest of each cell
/// from the one view), the findings re-sorted as one body's are (rule 25), the not-checked
/// notes one note (their words joined by "; " and the calls each once, as the bodies of one
/// entry's are). The first slice that was not checked (a refusal, a panic, a frame used up)
/// decides as the first body that stopped decides an entry: its result and note, the reads of
/// the slices before it beside its own, none of their findings.
fn join_slices<'a, 'f>(
    entry: &One<'f>,
    slices: &mut SVec<'a, (Buffer<'a>, SVec<'a, Ran<'f>>, RunOut<'a>)>,
    g: blaze_own::Alloc<'static>,
    alloc: SharedAlloc<'a>,
) -> Result<(Buffer<'a>, SVec<'a, Ran<'f>>, RunOut<'a>), EngineCheckError> {
    let name = entry.task.planned.name;
    let mut buffer = Buffer::new_in(alloc);
    let mut joined = RunOut::new(alloc);
    let mut reads: SVec<'a, (CellId, ContentId)> = SVec::new_in(alloc);
    let (mut ns, mut bytes, mut stretch) = (0u64, 0u64, InStretch::No);
    let mut stopped = None;
    for (at, (out, ran, run_out)) in slices.iter_mut().enumerate() {
        if ran.len() != 1 || out.done.len() != 1 {
            return Err(EngineCheckError::Sliced { body: name, what: "a slice's run did not file exactly one row" });
        }
        ns = ns.saturating_add(ran[0].ns);
        bytes = bytes.max(ran[0].bytes);
        if at == 0 {
            stretch = ran[0].stretch;
        }
        joined.scratch_most = joined.scratch_most.max(run_out.scratch_most);
        joined.frames.fold(run_out.frames);
        reads.extend_from_slice(&out.done[0].reads);
        joined.locals.extend(run_out.locals.iter().map(|local| (0, local.1)));
        // Each slice's bodies' typed records are their own (keyed by each body's check name and
        // verdict digest): the join keeps every one, a stopped slice's earlier bodies' too.
        joined.typed.append(&mut run_out.typed);
        joined.untyped_clean = joined.untyped_clean.saturating_add(run_out.untyped_clean);
        joined.setup.add(run_out.setup);
        // Its data files followed as its reads are joined: each slice's up to a stopped one.
        joined.data_files.append(&mut run_out.data_files);
        buffer.made.append(&mut out.made);
        // Not checked: `run_stretch` counts a slice's items only when it was.
        if run_out.items == 0 {
            stopped = Some(at);
            break;
        }
    }
    // The slices past a stopped one ran too: their clean bodies' typed records are kept (each
    // under its own body's check name and verdict), and their stretches' setup counted.
    if let Some(at) = stopped {
        for (_, _, run_out) in slices.iter_mut().skip(at + 1) {
            joined.typed.append(&mut run_out.typed);
            joined.untyped_clean = joined.untyped_clean.saturating_add(run_out.untyped_clean);
            joined.setup.add(run_out.setup);
        }
    }
    reads.sort_unstable_by_key(|read| read.0);
    reads.dedup_by_key(|read| read.0);
    let mut ran: SVec<'a, Ran<'f>> = SVec::with_capacity_in(1, alloc);
    ran.push(Ran { name, label: entry.label, ns, bytes, chain: entry.place.chain, note: None, found: FindingRange::default(), file: entry.file, stretch });
    if let Some(at) = stopped {
        let (out, _, run_out) = &mut slices[at];
        let Some(mut done) = out.done.pop() else {
            return Err(EngineCheckError::Sliced { body: name, what: "its stopped slice filed no row" });
        };
        if out.owed.is_empty() {
            done.reads = reads;
        } else {
            // Right for this request only: entered again whatever it read.
            buffer.owed.push(name);
        }
        joined.notes.extend(run_out.notes.drain(..).map(|(_, said, head_failed)| (0, said, head_failed)));
        buffer.done.push(done);
        return Ok((buffer, ran, joined));
    }
    let mut whole = Hasher::new(ENTRY_DOMAIN);
    let (mut verdict, mut failed, mut findings) = (true, false, 0u32);
    // The notes' words in the request's heap, as each body's are (`leak`).
    let mut joined_why = blaze_own::String::new_in(g);
    let mut calls: Vec<&'static str, blaze_own::Alloc<'static>> = Vec::new_in(g);
    let (mut noted, mut types_short) = (false, false);
    for (out, _, run_out) in slices.iter() {
        let done = &out.done[0];
        for out in run_out.outs.iter() {
            whole.update(&out.0);
        }
        verdict &= !done.defines.is_empty();
        failed |= done.failed;
        findings = findings.saturating_add(done.findings);
        joined.items += run_out.items;
        for (_, said, _) in run_out.notes.iter() {
            let words = match said {
                Said::Unchecked { why, calls: under } => {
                    types_short = true;
                    for call in under.iter() {
                        if !calls.contains(call) {
                            calls.push(*call);
                        }
                    }
                    *why
                }
                Said::BorrowsUnchecked(why) => *why,
                _ => return Err(EngineCheckError::Sliced { body: name, what: "a checked slice's note is no not-checked note" }),
            };
            noted = true;
            if !words.is_empty() {
                if !joined_why.is_empty() {
                    joined_why.push_str("; ");
                }
                joined_why.push_str(words);
            }
        }
    }
    let out = whole.finish();
    let mut defines: SVec<'a, (Cell, ContentId)> = SVec::with_capacity_in(usize::from(verdict), alloc);
    if verdict {
        defines.push((blaze_ladder::front2::tables::task::VERDICT_CELL, out));
    }
    if noted {
        let words = leak(g, format_args!("{joined_why}"));
        let said = match types_short {
            true => Said::Unchecked { why: words, calls: if calls.is_empty() { &[] } else { calls.leak_static() } },
            false => Said::BorrowsUnchecked(words),
        };
        joined.notes.push((0, said, None));
    } else if findings == 0 {
        joined.clean = 1;
    }
    joined.item_tasks = 1;
    // Rule 25: one entry's findings in one order, whatever its slices' were.
    let mut found: SVec<'a, (FoundAt, u16, &[u8])> = SVec::new_in(alloc);
    for (_, _, run_out) in slices.iter() {
        let mut from = 0;
        for &(_, rule, at, end) in run_out.found.iter() {
            found.push((at, rule, &run_out.messages[from..end]));
            from = end;
        }
    }
    found.sort_unstable();
    for &(at, rule, message) in found.iter() {
        joined.messages.extend_from_slice(message);
        joined.found.push((0, rule, at, joined.messages.len()));
    }
    buffer.push(Outcome::made(&entry.task, out, defines, reads, findings, failed), Stands::ByReads);
    Ok((buffer, ran, joined))
}

#[derive(Debug)]
struct LangIndexRefused(&'static str);

impl core::fmt::Display for LangIndexRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

/// Per-stretch lazy wrapper over `BodySeed`: serves all already-frozen member and dependency heads
/// (`Known::Bytes`) and member refusals (`Known::Refused`) directly in O(1) without constructing
/// `lang_index`, `Pointers`, `HeadConsts`, or `SeededHeads`. Only on a cold dependency head miss
/// (`Known::Freeze`) does it lazily build the freezer once per stretch into `frame`.
struct LazyBodySeed<'k, 'n, 't> {
    names: &'n blaze_ladder::front2::source::Records<'n>,
    heads: &'k crate::engine_seeds::StretchHeads<'k, 'k, 'n>,
    tables: Tables<'t, 'n>,
    evaluated: &'n Recorder<'n>,
    lang_crates: &'t [usize],
    frame: &'n Frame,
    fb: blaze_own::Bump<'n>,
    fa: blaze_own::Alloc<'n>,
    g: blaze_own::Alloc<'static>,
    body_locals: blaze_ladder::front2::tables::seeds::BodyLocals,
    index_reads: core::cell::Cell<u64>,
    lang_refused: core::cell::Cell<Option<&'static str>>,
    inner: core::cell::OnceCell<Result<&'n crate::engine_seeds::BodySeed<'k, 'n, 'n, &'n blaze_ladder::front2::source::Records<'n>, crate::engine_seeds::StretchHeads<'k, 'k, 'n>>, LangIndexRefused>>,
}

impl<'k, 'n, 't> LazyBodySeed<'k, 'n, 't> {
    fn index_reads(&self) -> u64 {
        self.index_reads.get()
    }

    fn lang_refused(&self) -> Option<&'static str> {
        self.lang_refused.get()
    }

    fn with_failure<R>(&self, cell: Want, inspect: impl FnOnce(CellFailure<'_>) -> R) -> Option<R> {
        self.inner.get()?.as_ref().ok()?.with_failure(cell, inspect)
    }

    #[inline(never)]
    fn build_lang_index(&self) -> Result<&'n blaze_ladder::front2::index::Index<'n>, LangIndexRefused> {
        match crate::engine_head_consts::lang_index(self.tables, self.evaluated, self.lang_crates) {
            Ok(index) => Ok(self.frame.alloc(index)),
            Err(twice) => {
                let words = crate::engine_head_notes::leak(self.g, format_args!("its scope's constants have no lang index: {twice} (front2 index.rs `LangTwice`)"));
                self.lang_refused.set(Some(words));
                Err(LangIndexRefused(words))
            }
        }
    }

    #[inline(never)]
    fn build_head_consts(&self, index: &'n blaze_ladder::front2::index::Index<'n>) -> &'n mut (dyn ConstValues<'n> + 'n) {
        self.frame.alloc(crate::engine_head_consts::HeadConsts::new(self.tables.reading_into(self.evaluated), index, self.fb))
    }

    #[inline(never)]
    fn build_body_seed(
        &self,
        index: &'n blaze_ladder::front2::index::Index<'n>,
        consts: Option<&'n mut (dyn ConstValues<'n> + 'n)>,
    ) -> &'n crate::engine_seeds::BodySeed<'k, 'n, 'n, &'n blaze_ladder::front2::source::Records<'n>, crate::engine_seeds::StretchHeads<'k, 'k, 'n>> {
        let sized = index.lang("sized");
        // The program's receiver pointer types, as the Heads stage's freezes are made with
        // (engine_heads.rs `heads_of`): without them this freezer would write other bytes
        // under a dependency head's key (rule 21; lane H9).
        let pointers = blaze_ladder::front2::receiver::Pointers::find(index, blaze_ladder::resolve::Paths { names: self.tables });
        let freezer = SeededHeads::new(SeededHeadsOpts { names: self.names, sized, pointers, frame: self.frame, scratch: self.g, alloc: self.fa, consts, known: Some(self.heads), owners: None });
        self.frame.alloc(crate::engine_seeds::BodySeed::new(RequestSeedOpts { names: self.names, heads: self.heads, freezer, body_locals: self.body_locals }))
    }

    #[inline(never)]
    fn build_freezer(&self) -> Result<&'n crate::engine_seeds::BodySeed<'k, 'n, 'n, &'n blaze_ladder::front2::source::Records<'n>, crate::engine_seeds::StretchHeads<'k, 'k, 'n>>, LangIndexRefused> {
        let index = self.build_lang_index()?;
        self.index_reads.set(self.evaluated.tally().offered);
        let consts = Some(self.build_head_consts(index));
        Ok(self.build_body_seed(index, consts))
    }
}

impl<'n> HeadSeed<'n> for LazyBodySeed<'_, 'n, '_> {
    fn seed(&self, store: &blaze_ladder::front2::heads::Store<'n>, keys: &blaze_ladder::front2::keys::Keys, need: Want, scratch: blaze_own::Alloc<'static>) -> Result<blaze_ladder::front2::heads::Seeded, SeedError> {
        if self.body_locals == blaze_ladder::front2::tables::seeds::BodyLocals::Lowered
            && blaze_ladder::front2::tables::seeds::of_the_body(store, need)
        {
            return Ok(blaze_ladder::front2::heads::Seeded::Lower);
        }
        let cell = blaze_ladder::front2::tables::head::cell_of(self.names, need).map_err(SeedError::Freeze)?;
        match blaze_ladder::front2::tables::head::KnownHeads::known(self.heads, cell) {
            blaze_ladder::front2::tables::head::Known::Bytes(bytes) => {
                let frozen = blaze_ladder::front2::tables::head::FrozenHead::read(bytes)
                    .map_err(|error| SeedError::Freeze(FreezeError::Read { want: cell, error }))?;
                blaze_ladder::front2::tables::head::file_frozen(self.names, cell, frozen, store, keys, scratch)
                    .map_err(SeedError::Freeze)?;
                return Ok(blaze_ladder::front2::heads::Seeded::Filed);
            }
            blaze_ladder::front2::tables::head::Known::Refused(why) => {
                return Err(SeedError::of_freeze(FreezeError::NotTheirs { want: cell, why }));
            }
            blaze_ladder::front2::tables::head::Known::Freeze => {}
        }
        let inner = self.inner.get_or_init(|| self.build_freezer());
        match inner {
            Ok(body_seed) => body_seed.seed(store, keys, need, scratch),
            Err(refused) => Err(SeedError::of_freeze(FreezeError::NotTheirs {
                want: cell,
                why: NotTheirs::Failed { said: refused.0 },
            })),
        }
    }
}

/// One stretch of a run: its member's records opened once into `frame`, each body checked over
/// one view until the frame is past [`STRETCH_FULL`]. How many bodies it took (at least one).
#[expect(clippy::too_many_arguments, reason = "a stretch's edge: each argument one handle of it")]
fn run_stretch<'a, 'f, R: ?Sized + CellDigests>(
    cx: &Cx,
    shared: &Shared<'_, 'a, '_, '_, R>,
    run: &[One<'f>],
    frame: &Frame,
    scratch: &mut Frame,
    clock: &Clock,
    out: &mut Buffer<'a>,
    ran: &mut SVec<'a, Ran<'f>>,
    run_out: &mut RunOut<'a>,
    tally: &mut crate::engine_seeds::SeedTally,
    alloc: SharedAlloc<'a>,
) -> usize {
    #[cfg(test)]
    crate::engine_check::stack_probe::at("run_stretch");
    let Some(first) = run.first() else { return 0 };
    let Some(group) = shared.groups.get(first.group as usize) else {
        return refuse::<R>(run, "a body's member group is gone", None, out, ran, run_out, alloc);
    };
    // The setup's marks (D168 (2) (e)): what the stretch does before its first body begins.
    let set_began = clock.now();
    let g = shared.heap.alloc();
    // The stretch's frame as an allocator: what is placed in the frame (which runs no drop)
    // allocates its lists in the frame too, so they end with the stretch's reset. Before, the
    // impl table's notes, the evaluator's sink and the constants' lists were placed in the frame
    // but grew on the process heap (`g`), never freed until the request ended: the Bodies
    // stage's growth on a 700-crate workspace (engine_bodies.rs memory table, lane BD1).
    let fb = frame.alloc(blaze_own::Erased::new(frame)).bump();
    let fa = fb.alloc();
    // The impls from the request's shelf: each impl question reads its crate's bucket cell
    // (front2 tables/task.rs `cell_id`), which the Heads stage defined. Held in the stretch's
    // frame, so it lives as long as the records it is read beside (`Tables` takes both for one
    // lifetime); its notes in the frame too (`fa`).
    // On the `Ready` driver a crate the shelf lacks is read from the stored rows on its first
    // probe (engine_shelf_pages.rs), the same tables the phase path's shelf holds (rule 78).
    let impls: &TableSet<'_> = frame.alloc(match shared.source {
        Some(source) => TableSet::paged(shared.shelf, source, fa),
        None => TableSet::lent(shared.shelf, fa),
    });
    let scope = Some((shared, group));
    let env = match env_in(group.env, frame) {
        Ok(env) => env,
        Err(error) => return refuse(run, leak(g, format_args!("its member's environment did not copy into the run: {error:?} (blaze-scope crate_env.rs `EnvLent::copy_in`)")), scope, out, ran, run_out, alloc),
    };
    // Its scope crates past the member opened by need, at their first question, through the
    // request's memo (`shared.by_need`, piece C (a)); the oracle arm's all opened at the setup.
    let opened = open_stretch_records(
        cx,
        shared.root,
        group.crates.scope(shared.by_need),
        &group.list,
        (shared.packs, shared.fs).into(),
        shared.outlined,
        shared.has_unread_outlines,
        shared.unread_digests,
        shared.dep_files,
        shared.corrected,
        shared.procs,
        env,
        shared.snapshot,
        shared.expansion_keys,
        frame,
        g,
    );
    let set_records = clock.now();
    let unused: &Recorder<'_> = frame.alloc(Recorder::new(RecorderOpts { alloc: fa }));
    let tables = Tables::new(TablesOpts { records: opened.records, impls: Some(impls), lang_crates: &group.lang, reads: unused });
    // A stack owner drops the memo when this stretch ends. Its caller's heap reclaims old
    // buffers as it grows; a bump frame would retain every superseded capacity.
    let mut ids = ReadIds::new(tables, g);
    // `First`: under `All` clap-root's `Cmd` was refused with a false cycle among core's
    // iterator traits (front2 heads.rs `Store::new`; lane FL).
    // The request's frozen heads (engine_seeds.rs): each head a body needs imported from them,
    // or frozen here on its first read and published; none lowered in the body's store. Both
    // in the stretch's frame, for the records' lifetime the body's store takes them for (the
    // frame runs no drop: the freezer's lists are in the frame too, through `fa`). A want is
    // placed by this stretch's names the one way `widen` places a read (engine_seeds.rs
    // `StretchHeads`), on the stack so its `Owners` is dropped with the stretch.
    let stretch_heads = shared.seeds.map(|table| crate::engine_seeds::StretchHeads::new(table, shared.heads_failed, opened.records, shared.heads, group.crates.scope(shared.by_need), shared.listings, g));
    let set_heads = clock.now();
    // The seed freezer's constant evaluator and lang index are built lazily on the first cold
    // dependency head miss (`LazyBodySeed`): warm edits (0 heads frozen) pay 0 us here.
    let evaluated: &Recorder<'_> = frame.alloc(Recorder::new(RecorderOpts { alloc: fa }));
    let mut eval = EvalReads { placed: 0, ids: Vec::new_in(fa) };
    let seed: Option<&LazyBodySeed<'_, '_, '_>> = stretch_heads.as_ref().map(|table| {
        &*frame.alloc(LazyBodySeed {
            names: opened.records,
            heads: table,
            tables,
            evaluated,
            lang_crates: &group.lang,
            frame,
            fb,
            fa,
            g,
            body_locals: table.body_locals(),
            index_reads: core::cell::Cell::new(0),
            lang_refused: core::cell::Cell::new(None),
            inner: core::cell::OnceCell::new(),
        })
    });
    // Each body's rounds (front2 tables/cost.rs `Spent::rounds`): one when its heads are all
    // seeded (runtime.md point 3).
    let cost: &Cost<'_> = frame.alloc(Cost::new(&NO_GAUGE));
    // One stretch-scoped macro preparation table shared across every body in the stretch so
    // `ProjectWorld::over_prepared` allocates 0 bytes in `frame` per body and macros are prepared
    // once per stretch rather than leaked per body.
    let prepared: &blaze_scope::prepares::Prepares<'_> = blaze_scope::prepares::Prepares::new_in(fb);
    let body_frame = BodyFrame { frame, alloc: g, plant_drop: None, cost: Some(cost), needs: NeedsMet::First, heads: seed.map(|s| s as &dyn HeadSeed<'_>), sources: Some(opened.sources), goals: None, prepared: Some(prepared) };
    // Each crate's listing from the request's table (engine_heads.rs `Listings`): made once a
    // request, not once a stretch that places a read in it.
    let set_lang = clock.now();
    let mut owners = crate::engine_heads::Owners::new(shared.heads, group.crates.scope(shared.by_need), shared.listings, g);
    let set_done = clock.now();
    run_out.setup.add(StretchSetup {
        stretches: 1,
        ns: ns_between(set_began, set_done),
        records_ns: ns_between(set_began, set_records),
        heads_ns: ns_between(set_records, set_heads),
        lang_ns: ns_between(set_heads, set_lang),
        owners_ns: ns_between(set_lang, set_done),
    });
    let mut taken = 0usize;
    for one in run {
        if taken > 0 && frame.used() > STRETCH_FULL {
            break;
        }
        // The workers' scheduling point (engine_status.rs): stopped, the stretch ends, and
        // `run_bodies` refuses the rest.
        if taken > 0 && shared.board.stopping().is_some() {
            break;
        }
        taken += 1;
        // The body's scratch starts empty (rule 11): the last body's findings and words were
        // copied out as it ended. Kept only under the plant (the answers must not change).
        run_out.frames.scratch_committed_most = run_out.frames.scratch_committed_most.max(scratch.committed() as u64);
        if !shared.keep_scratch {
            scratch.reset();
            // A reset keeps every block the last body grew it to: past `SCRATCH_KEEP` they go
            // back now, so a large body's high-water is not held through the run's rest.
            if scratch.committed() > SCRATCH_KEEP {
                scratch.trim(SCRATCH_KEEP);
                run_out.frames.scratch_trims += 1;
            }
        }
        #[cfg(test)]
        let small = shared.plants.fault.is_some_and(|(item, _)| item == one.label).then(|| {
            Frame::with_reserve(8 << 20, bstd::mem::Tag::Diagnostics).expect("the planted body's reservation")
        });
        let sc: &Frame = scratch;
        #[cfg(test)]
        let sc = small.as_ref().unwrap_or(sc);
        // The scratch as the allocator the check works in (rule 11; lane BD3): its recorder,
        // its cells, its words, every list on the way to its row; the row alone is copied out
        // into the request's arena (`row_of`). Before, all of it was in that arena, and the
        // row kept the capacity of the list before widening.
        let erased = blaze_own::Erased::new(sc);
        let w = erased.handle();
        let mut mem = BodyMem {
            allocating: "the body's input records",
            scope: group.crates.len() as u64,
            #[cfg(test)]
            fault: shared.plants.fault.filter(|(name, _)| *name == one.label).map(|(_, fault)| fault),
            ..BodyMem::default()
        };
        // The guard (rule 54): the frame's use before and after each body, one load each; a
        // later body's growth is what a question kept past its body (engine_memory.rs
        // `BodyFrames`).
        let stretch = if taken == 1 { InStretch::First } else { InStretch::Later };
        let at_start = frame.used();
        let _body = cx.span("engine: one body");
        let began = clock.now();
        let running = shared.board.show(Stage::Bodies, crate::engine_status::Label::of(one.label), at_start as u64, shared.board.now(clock));
        let name = one.task.planned.name;
        let (made_before, hits_before) = ids.counts();
        let mut body_tally = crate::engine_seeds::SeedTally::default();
        let mut recording = Some(Recorder::new(RecorderOpts { alloc: w }));
        // Keep the include log until publication: a frame can fill after the last include
        // and before the row's reads have been copied out.
        let data_at = opened.sources.0.data.borrow().len();
        // The reads of this body's calls that left its package inside the workspace (D111), told
        // to the texts while it is checked (job_texts.rs `left_package`): its warnings.
        let left_at = opened.sources.0.left.borrow().len();
        // A refused large allocation need not increase the peak, and an ordinary panic can
        // happen arbitrarily close to the limit. Read the typed refusal at its source.
        let frame_limit = |what: &'static str| {
            let scratch_full = sc.take_exhaustion();
            // A shared stretch has space left by its preceding bodies. No source cell
            // certifies that capacity. Only the body's private reservation starts empty.
            match frame.take_exhaustion() {
                Some(used) => Some((used, frame.reserved(), what, Stands::OwedNextRequest)),
                None => scratch_full.map(|used| (used, sc.reserved(), what, Stands::ByReads)),
            }
        };
        let gather = || {
            #[cfg(test)]
            if mem.fault == Some(BodyFault::WidenPanic) {
                // Above half full, but the frame still grants allocations. The ordinary
                // panic in widening must be reported as Panicked even at this high water.
                sc.alloc_layout(core::alloc::Layout::array::<u8>(sc.reserved() / 2).expect("small test scratch"));
            }
            // Each body the task checks (one; or each an expansion at its entry wrote), merged into
            // the task's one result: its reads the union, its verdict the digest of theirs.
            let mut whole = Hasher::new(ENTRY_DOMAIN);
            let mut reads = blaze_own::AVec::new_in(w);
            let mut entry_consts = (one.items.len() > 1).then(|| DistinctIds::new_in(w));
            let mut encoding = blaze_own::AString::new_in(w);
            let offered_before = evaluated.tally().offered;
            // One item's verdict digest: the task's out as it is (more items: the merged digest).
            let mut single: Option<ContentId> = None;
            let (mut findings, mut failed, mut said) = (0u32, false, None);
            // The body's words why and its findings, in its scratch until the body ends.
            let mut why = blaze_own::String::new_in(sc);
            // The refused macro calls the body is not checked under, each once, its words in the
            // request's heap: the note's `calls` ([`Said::Unchecked`]), copied out when it is made.
            let mut calls: Vec<&'static str, _> = Vec::new_in(sc);
            // Whether a cause of `why` is not about borrows alone: the body's types were then not
            // checked whole ([`Said::Unchecked`]); else only its borrows were ([`Said::BorrowsUnchecked`]).
            let mut types_short = false;
            let mut found: Vec<BodyFinding<'_>, &Frame> = Vec::new_in(sc);
            // The fns declared in its bodies that the check reached: its covers too.
            let mut locals: Vec<ItemKey, &Frame> = Vec::new_in(sc);
            // Its clean bodies' typed records, copied out of the checker's work ([`BodyResult::typed`]).
            let mut typed: Vec<(Name, &[u8]), &Frame> = Vec::new_in(sc);
            let mut untyped_clean = 0u32;
            let before = cost.spent();
            // A head the frozen form could not give it (rule 3: never lowered instead).
            let mut seed_refused = false;
            let mut scope_required = false;
            // The entry's text as its bodies were checked from (its crate's records, the member at
            // 0), and its crate's edition: what its findings' ranges are said as tokens of.
            let item_text = opened.records.file_now(0, one.place.file).and_then(|text| text.get(one.place.item.to_usize())).zip(NamesSource::edition_of(opened.records, 0));
            for (at, item) in one.items.iter().enumerate() {
                let mut asked = one.task;
                asked.planned.name = Name::new(Kind::BodyCheck, item.0);
                mem.allocating = "the body checker's verdict lists";
                #[cfg(test)]
                crate::engine_check::stack_probe::at("run_stretch: a body's check");
                let result = {
                    let keyer = BodyReadKeyer {
                        grains: shared.grains,
                        fine_buckets: shared.plants.fine_buckets,
                        ids: core::cell::RefCell::new(&mut ids),
                    };
                    let active = recording.as_mut().expect("the entry owns its recorder");
                    let mut check = || check_body_item(tables, &asked, &body_frame, w, active, Some(&keyer), &mut encoding);
                    #[cfg(test)]
                    let checked = if shared.plants.check_only_guard {
                        match work::unwinding(check) {
                            Ok(result) => result,
                            Err(work::Panicked) => {
                                said = Some(Said::Panicked);
                                break;
                            }
                        }
                    } else {
                        check()
                    };
                    #[cfg(not(test))]
                    let checked = check();
                    checked
                };
                #[cfg(test)]
                let result = result.and_then(|body| {
                    use blaze_ladder::front2::tables::recorder::ReadSink;
                    if !matches!(mem.fault, Some(BodyFault::MissingFact | BodyFault::OweMissingFact | BodyFault::DropMissingFactRead)) {
                        return Ok(body);
                    }
                    let log = opened.sources.0.data.borrow();
                    let (path, _, _) = log.get(data_at).expect("the missing-fact fixture must read its include");
                    let data = CellId { of: Name::file(path.as_bytes()), cell: WHOLE };
                    let key = blaze_typeck::ItemKey(u128::from_be_bytes(shared.reads.cell(&data).0[..16].try_into().expect("a digest prefix")));
                    let keys = blaze_ladder::front2::keys::Keys::new();
                    let error = blaze_ladder::front2::tables::head::fact_cell(opened.records, &keys, key).expect_err("no head for the data digest");
                    let shown = blaze_ladder::front2::item_shown::FactDefinition::of(opened.records, key, None, w);
                    let active = recording.insert(Recorder::new(RecorderOpts { alloc: w }));
                    for &key in &body.reads {
                        active.read(key);
                    }
                    Err(TaskError::Cell { key, ask: blaze_ladder::front2::tables::task::CellAsk::Fact(blaze_ladder::front2::reads::FactKind::Head), holder: key, shown, error })
                });
                match result {
                    Ok(body) => {
                        mem.recorded += body.read_tally.kept;
                        mem.body_offers += body.read_tally.offered;
                        mem.offered += body.read_tally.offered;
                        mem.asks.add(body.read_tally);
                        add_work(&mut mem.work, &body.work);
                        for &key in &body.reads {
                            mem.kinds.add(key);
                        }
                        mem.allocating = "the body's local function key list";
                        locals.extend_from_slice(&body.locals);
                        // Its typed record (D163 R1), keyed later by its bytes and named by its
                        // run facts: what `blaze run` builds from instead of checking it again.
                        mem.allocating = "the body's typed record";
                        match body.typed.as_ref() {
                            Some(bytes) => typed.push((asked.planned.name, sc.alloc_slice_copy(bytes.as_slice()))),
                            None if !body.failed => untyped_clean = untyped_clean.saturating_add(1),
                            None => {}
                        }
                        // The constants' defining texts the body's evaluator read are reads of the
                        // body (K1h): placed like a head's, or the body is refused by name.
                        mem.allocating = "the constant text cell list";
                        if let Err(unplaced) = crate::engine_head_consts::eval_reads(opened.records, shared.heads, &body.const_texts, &mut |cell| {
                            if entry_consts.as_mut().is_none_or(|seen| seen.insert(cell)) {
                                reads.push(cell);
                            }
                        }) {
                            let crates = group.crates.opened(shared.by_need, g);
                            let why = crate::engine_head_consts::UnplacedIn { unplaced: &unplaced, crates: &crates };
                            scope_required = true;
                            said = Some(Said::Refused(crate::engine_head_notes::leak(g, format_args!("a constant body's text read: {why}"))));
                            break;
                        }
                        whole.update(&body.out.0);
                        // A slice's verdicts, digested into its entry's out at the join.
                        if one.part.is_some() {
                            run_out.outs.push(body.out);
                        }
                        mem.allocating = "the recorded cell ID list";
                        reads.reserve(body.reads.len());
                        for &key in &body.reads {
                            match EvalReadError::place(key, &mut ids, opened.records, shared.heads) {
                                Ok(Some(id)) => reads.push(id),
                                Ok(None) => {}
                                Err(error) => {
                                    scope_required = true;
                                    said = Some(match error {
                                        EvalReadError::Id(error) => Said::Task(error),
                                        other => Said::Refused(other.words(&group.crates.opened(shared.by_need, g), g)),
                                    });
                                    break;
                                }
                            }
                        }
                        if said.is_some() {
                            break;
                        }
                        mem.allocating = "the body's findings and note words";
                        findings = findings.saturating_add(body.findings);
                        // Its findings as `check` files them. A type error that a refused macro
                        // call may have caused (front2 path_miss_shown.rs `MACRO_REFUSED`: the
                        // name may be one the call would have made) is no error of the body: the
                        // body is NOT CHECKED under that call, and its note says so in the form
                        // every not-checked note has ("not checked because <cause>: ..", as a head
                        // that could not be frozen says it), so the reader sees which item went
                        // unchecked and why (rules 3, 19; check-is-a-whole-check).
                        let written = one.place.written.get(at).copied().flatten();
                        for line in body.said.iter() {
                            if let SaidWhat::Type { .. } = line.what
                                && let Some(cut) = line.words.as_str().find(blaze_ladder::front2::MACRO_REFUSED)
                            {
                                let cause = &line.words.as_str()[cut + blaze_ladder::front2::MACRO_REFUSED.len()..];
                                types_short = true;
                                // Each call once, apart from the own causes: the cause is the
                                // call's failure alone, said once (the miss's words before it only
                                // repeat it: "function `foo` .. unknown, because a macro call of
                                // its module was refused: <the call's failure>").
                                if !calls.iter().any(|&seen| seen == cause) {
                                    calls.push(leak(g, format_args!("{cause}")));
                                }
                                continue;
                            }
                            // A finding in a local fn of the body (front2 tables/task.rs
                            // `Said::within`) is shown at the item's start, labelled
                            // `<outer>::<local>` (the `Unplaced` form).
                            let local;
                            let written = if let Some(name) = &line.within {
                                let outer = written.map_or(one.label, |place| place.label);
                                local = bstd::mem::format_in!(sc, "{outer}::{name}");
                                let words = written.map_or("a local function, shown at the item's start: ", |place| place.words);
                                Some(Unplaced { label: local.as_str(), words })
                            } else {
                                written
                            };
                            match found_of(line, written, item_text, sc) {
                                Ok(Some(one_found)) => found.push(one_found),
                                Ok(None) => {}
                                // Never quietly dropped (rule 3): the body is then not checked whole.
                                Err(cause) => {
                                    types_short = true;
                                    if !why.is_empty() {
                                        why.push_str("; ");
                                    }
                                    why.push_str(cause);
                                }
                            }
                        }
                        // The first failed body's words why (rule 19): its verdict's not-checked lines.
                        if body.failed && !failed {
                            for line in body.said.iter() {
                                let cause = match &line.what {
                                    SaidWhat::NotChecked { cause } => cause.as_str(),
                                    SaidWhat::BorrowsNotChecked => "borrows not checked",
                                    SaidWhat::Type { .. } | SaidWhat::Lint { .. } | SaidWhat::Borrow { .. } => continue,
                                };
                                if !why.is_empty() {
                                    why.push_str("; ");
                                }
                                why.push_str(cause);
                                why.push_str(": ");
                                push_keyless(&mut why, line.words.as_str());
                            }
                        }
                        // A failed verdict whose only gap is the borrow check: types checked whole.
                        let borrows_only = body.said.iter().any(|l| matches!(l.what, SaidWhat::BorrowsNotChecked)) && !body.said.iter().any(|l| matches!(l.what, SaidWhat::NotChecked { .. }));
                        types_short |= body.failed && !borrows_only;
                        failed |= body.failed;
                        if one.items.len() == 1 && one.part.is_none() {
                            single = Some(body.out);
                        } else {
                            recording.as_mut().expect("the entry owns its recorder").reuse_item_buffer(body.reads);
                        }
                    }
                    Err(mut error) => {
                        if let Some(what) = recording.as_ref().and_then(Recorder::allocating) {
                            mem.allocating = what;
                        }
                        if let Some(limit) = frame_limit(mem.allocating) {
                            // A fallible write can return TaskError instead of unwinding.
                            return Err(limit);
                        }
                        if let TaskError::Cell { shown, .. } = &mut error {
                            shown.version = shown.krate.and_then(|i| group.crates.world_at(i)).map(|at| group.list.version(at));
                        }
                        seed_refused = matches!(error, TaskError::Seed { .. } | TaskError::Cell { error: FreezeError::Record { .. }, .. });
                        // The checker converts its verdict's facts to reads after checking.
                        // If that conversion stopped, even the unvisited suffix can have
                        // affected the answer. Only a stop during setup has a complete prefix.
                        scope_required = !matches!(error, TaskError::NotABody { .. } | TaskError::Lang(_));
                        if let Some(active) = recording.take() {
                            mem.unfinished_reads(&active);
                            // A head-seed refusal is itself a complete prefix.  The body has
                            // read exactly the cells up to the failed head, so retain that
                            // prefix instead of widening to the whole scope.  A recorder that
                            // filled, or a placement failure, still has no certifying list.
                            if !scope_required || seed_refused {
                                // The owner's root and the graph selected the lang crates
                                // and the unit in which body_of looked for this item.
                                // An unplaceable key below restores the scope requirement.
                                scope_required = false;
                                reads.push(CellId { of: group.own, cell: WHOLE });
                                // D187: the graph's cells of the owner's crate and of its lang
                                // crates (engine_units.rs `crate_cell`), what selected them, not
                                // the whole graph's: a bump of another crate leaves this body.
                                // The graph defines them whenever it has a crate list, the
                                // precondition of any group (engine_check.rs `publish`).
                                for &at in core::iter::once(&0).chain(group.lang.iter()) {
                                    if let Some(world_at) = group.crates.world_at(at) {
                                        reads.push(crate::engine_units::crate_cell_id(&group.list.key(world_at)));
                                    }
                                }
                                match active.keys() {
                                    Ok(keys) => {
                                        for &key in keys.iter() {
                                            match EvalReadError::place(key, &mut ids, opened.records, shared.heads) {
                                                Ok(id) => reads.extend(id),
                                                Err(_) => { scope_required = true; break; }
                                            }
                                        }
                                    }
                                    Err(_) => scope_required = true,
                                }
                            }
                        } else {
                            scope_required = true;
                        }
                        said = Some(Said::Task(error));
                        break;
                    }
                }
            }
            // AN INCLUDED DATA FILE IS A READ of the body that included it (rule 21), with the
            // digest of what the body was handed (job_texts.rs `read_data`), so an edit of the
            // file enters the body again: through its `FileItems` record when today's plan has one
            // (a literal include), else through its checkout-read `Given` cell, the one a build
            // script's `file:` read of it has, followed from the next request on ([`data_read`]).
            mem.allocating = "the included data-file read list";
            let mut data_reads = blaze_own::AVec::new_in(w);
            let mut data_seen = blaze_own::AVec::new_in(w);
            let mut data_unnamed = false;
            for &(path, _, bytes) in &opened.sources.0.data.borrow()[data_at..] {
                let read = data_read(shared.reads, path, bytes);
                data_reads.push(read.id);
                data_seen.push((read.id, read.seen));
                match read.cell {
                    DataCell::Planned => {}
                    DataCell::Followed => {
                        // PLANTED CONTROL, tests only (rule 23): the leaf is never kept, so no
                        // task defines the cell and an edit of the file is missed.
                        #[cfg(test)]
                        if mem.fault == Some(BodyFault::DropDataLeaf) {
                            continue;
                        }
                        run_out.data_files.push((blaze_own::String::from_str_in(path, alloc), read.seen));
                    }
                    DataCell::Unnamed => data_unnamed = true,
                }
            }
            // D111, A BODY'S CALL THAT READ OUTSIDE ITS PACKAGE (inside the workspace): a warning
            // finding of this body at the call, with the words names/include_outside.rs gives the
            // same call at item level (the call, the file read, the package), never an error.
            mem.allocating = "the body's left-package warnings";
            let package = group.crates.world_at(0).map(|at| group.list.shown_from(at));
            for &(from, at, shape, call, path) in &opened.sources.0.left.borrow()[left_at..] {
                found.push(left_found((from, at, shape, call, path), package, one.place.item, item_text, sc));
            }
            // A scope crate opened by need whose record did not open (front_load.rs
            // `StoreLoad::refused`): the records answered this body, and every later one of the
            // stretch, as if the crate held nothing (front2 keeps the miss per job), so its verdict
            // stands on nothing and is refused by name, as the oracle arm cuts the scope at the
            // setup (`scope_of`, `CutWhy::Unreadable`). Sticky: every body after the first ask.
            if let Some((k, words)) = opened.load.refused() && !matches!(said, Some(Said::Panicked)) {
                let krate = group.crates.world_at(k).map_or("?", |at| CrateGraph::name(&group.list, at));
                said = Some(Said::Refused(leak(g, format_args!("its member's check scope holds `{krate}`, whose names record could not be read: {words} (engine_bodies.rs `run_stretch`)"))));
                scope_required = true;
                seed_refused = false;
            }
            if let Some(words) = seed.and_then(|s| s.lang_refused()) && !matches!(said, Some(Said::Panicked)) {
                said = Some(Said::Refused(words));
                scope_required = true;
                seed_refused = false;
            }
            // A checked verdict and a refusal at a seed both depend on the evaluator. Its
            // prepared definitions persist across this stretch, so each row takes its prefix.
            mem.allocating = "the constant evaluator's cell ID list";
            let index_reads = seed.map_or(0, |s| s.index_reads());
            let placed = if !scope_required {
                eval.update(evaluated, index_reads, &mut ids, opened.records, shared.heads)
                    .map_err(|error| error.words(&group.crates.opened(shared.by_need, g), g))
            } else {
                Ok(())
            };
            mem.offered += evaluated.tally().offered - offered_before;
            let mut head_failed = None;
            if seed_refused {
                // Refused at a seed; never checked by lowering instead (rule 3). A head that cannot
                // be made is reported once, at the head (its head task's refusal, or `check`'s), and
                // each body that needs it is NOT CHECKED, its note naming the head by path (rule 19):
                // the incomplete status, as `check`'s, never a second error a body. Only the engine's
                // own inconsistency (unequal bytes on a second freeze, a table full, a cell the table
                // should hold and does not, bytes that do not read) stays the body's failure.
                mem.allocating = "the refused head's cause and scope read list";
                body_tally.refused = body_tally.refused.saturating_add(1);
                let mut head: Option<Want> = None;
                said = match said {
                    Some(Said::Task(TaskError::Seed { body, error: SeedError::Freeze(error) })) => {
                        match unmade_cell(&error) {
                            Unmade::Head(asked) => {
                                head = Some(asked);
                                let label = |cell| crate::engine_head_notes::cause_label(opened.records, cell, g);
                                let step = |cell| {
                                    // Only failed freezes need a retained entry. A refusal from cell_of,
                                    // for example, is the typed error of this ask itself.
                                    if !matches!(error, FreezeError::Again { .. } | FreezeError::NotTheirs { why: NotTheirs::Failed { .. }, .. }) {
                                        return Some(Step::End(cell, Cause::own(&error, opened.records, g)));
                                    }
                                    if let Some(local) = seed.and_then(|s| s.with_failure(cell, |failure| seed_cause(cell, failure, shared.seeds, opened.records, &mut head_failed, g))) {
                                        return Some(local);
                                    }
                                    // A member's head belongs to its head task. This also covers a
                                    // stretch's last Needs edge into another source.
                                    let crate::engine_heads::HeadAt::Cell { cell: id, .. } = owners.place(opened.records, &head_cell_id(cell), g)? else { return None };
                                    let refused = shared.heads_failed.find(id.of, id.cell)?;
                                    Some(Step::End(cell, refused.cause))
                                };
                                Some(unmade_head(asked, step, label, &mut body_tally, sc, g))
                            }
                            Unmade::Unnamed => Some(Said::Unchecked { why: leak(g, format_args!("not checked because a head it reads could not be made: {error}")), calls: &[] }),
                            Unmade::Engine => Some(Said::Task(TaskError::Seed { body, error: SeedError::Freeze(error) })),
                        }
                    }
                    Some(Said::Task(error @ TaskError::Cell { error: FreezeError::Record { want, .. }, .. })) => {
                        head = Some(want);
                        Some(Said::Task(error))
                    }
                    Some(Said::Task(TaskError::Seed { body, error })) => {
                        head = match &error {
                            SeedError::NotFrozen { cell, .. } | SeedError::Unequal { cell }
                            | SeedError::Full { cell, .. } | SeedError::NotInItsUnit { cell, .. }
                            | SeedError::NoMemberCell { cell, .. } => Some(*cell),
                            SeedError::Freeze(_) => unreachable!("freeze handled above"),
                        };
                        Some(Said::Task(TaskError::Seed { body, error }))
                    }
                    other => other,
                };
                match head {
                    Some(want) => {
                        // Place the failed head's cell to its owning `Signature` task (same rule
                        // as `widen`, rule 21): `head_cell_id(want)` is unplaced (keyed by item,
                        // not task name), and `data_reads` bypasses `widen_as`, so leaving it
                        // unplaced recorded a phantom cell that was forever `ABSENT` and never
                        // woke the body when the head was repaired. Widening a placed member head
                        // to its file would wake unrelated edits; keep its exact head cell.
                        let raw = head_cell_id(want);
                        match owners.place(opened.records, &raw, g) {
                            Some(crate::engine_heads::HeadAt::Cell { cell, .. }) => data_reads.push(cell),
                            Some(
                                crate::engine_heads::HeadAt::Dependency(of)
                                | crate::engine_heads::HeadAt::OfTheBody(of)
                                | crate::engine_heads::HeadAt::Unlisted { scope: of, .. },
                            ) => data_reads.push(CellId { of, cell: WHOLE }),
                            None => scope_required = true,
                        }
                    }
                    None => scope_required = true,
                }
            }
            if let Err(words) = placed {
                scope_required = true;
                said.get_or_insert(Said::Refused(words));
            }
            if shared.seeds.is_some() && said.is_none() {
                body_tally.seeded = body_tally.seeded.saturating_add(1);
                body_tally.rounds = body_tally.rounds.saturating_add(cost.spent().since(&before).rounds);
            }
            if let Some(said) = said {
                let (done, stands) = if matches!(said, Said::Panicked) {
                    Outcome::owed(&one.task, alloc)
                } else if scope_required {
                    let done = shared.refused_scope(one, group, data_reads.iter().copied(), alloc);
                    mem.count_scope(&done);
                    (done, Stands::ByReads)
                } else {
                    reads.extend_from_slice(&eval.ids);
                    mem.before = reads.len() as u64;
                    let (wide, _) = widen_as(shared, &reads, group, &mut owners, opened.records, w, &mut mem);
                    let row = row_of(wide, one.held, &data_reads, shared, &mut mem, sc, alloc);
                    Outcome::failed(&one.task, row, alloc)
                };
                let (mut done, stands) = (done, data_stands(stands, data_unnamed));
                saw_data(&mut done.reads, &data_seen);
                #[cfg(test)]
                let (done, stands) = match mem.fault {
                    Some(BodyFault::OweMissingFact) => (done, Stands::OwedNextRequest),
                    Some(BodyFault::DropMissingFactRead) => {
                        let mut done = done;
                        done.reads.retain(|(id, _)| *id != data_reads[0]);
                        (done, stands)
                    }
                    _ => (done, stands),
                };
                let note = Some((said.map_task(|error| blaze_own::Box::new_in(error, alloc)), head_failed));
                // Its locals keep the entry's class: a call of one is refused naming it, not
                // as a fn the table holds nothing for.
                // Not checked whole: no typed record of it is put.
                return Ok(BodyResult { done, note, found: Vec::new_in(sc), locals, typed: Vec::new_in(sc), untyped_clean: 0, checked: false, stands });
            }
            // Rule 25: the lines record's bytes a function of the findings alone.
            found.sort_unstable_by(|a, b| (a.at, a.rule, a.message).cmp(&(b.at, b.rule, b.message)));
            // Its verdict: one item's as it is (its one define the verdict cell unless it failed);
            // an entry's expansion bodies (or none: an expansion that wrote no body is a pass that
            // read its crate's names), one verdict cell, the merged digest.
            let (out_id, defines_verdict) = match single {
                Some(out_id) => (out_id, !failed),
                None => (whole.finish(), true),
            };
            if one.items.is_empty() {
                reads.push(CellId { of: group.own, cell: WHOLE });
            }
            reads.extend_from_slice(&eval.ids);
            mem.before = reads.len() as u64;
            mem.work.cells = (reads.capacity() * core::mem::size_of::<CellId>()) as u64;
            let (wide, unfrozen) = widen_as(shared, &reads, group, &mut owners, opened.records, w, &mut mem);
            // The row, made now from what it keeps: its reads copied out of the scratch at their
            // length, its one define (rule 11; lane BD3).
            let mut defines: SVec<'a, (Cell, ContentId)> = SVec::with_capacity_in(usize::from(defines_verdict), alloc);
            if defines_verdict {
                defines.push((blaze_ladder::front2::tables::task::VERDICT_CELL, out_id));
            }
            let mut done = Outcome::made(&one.task, out_id, defines, row_of(wide, one.held, &data_reads, shared, &mut mem, sc, alloc), findings, failed);
            saw_data(&mut done.reads, &data_seen);
            // A head it read has no frozen head: the body fails by that read (rule 3), naming the
            // head and quoting why (engine_head_notes.rs), its reads kept so the head's repair
            // enters it again.
            mem.allocating = "the body's retained findings and note words";
            if let Some((task, cell)) = unfrozen {
                if !done.failed {
                    done.failed = true;
                    done.defines.clear();
                    done.findings = done.findings.max(1);
                }
                types_short = true;
                // After the body's own reason when it has one: both are said.
                if !why.is_empty() {
                    why.push_str("; ");
                }
                shared.heads_failed.body_words(task, cell, &mut why);
            }
            // One a body not checked whole (its verdict failed, or a line of it could not be said
            // as a finding): in the request's heap, as the labels are.
            // Reserve destination space without publishing a finding or any message bytes.
            // A panic in either reserve leaves the run's lengths untouched.
            run_out.found.reserve(found.len());
            run_out.messages.reserve(found.iter().map(|finding| finding.message.len()).sum());
            mem.scratch = sc.used() as u64;
            let said = (done.failed || !why.is_empty() || !calls.is_empty()).then(|| {
                let words = leak(g, format_args!("{why}"));
                if types_short {
                    // A body under no call allocates no list: the empty slice is static.
                    let calls: &'static [&'static str] = if calls.is_empty() {
                        &[]
                    } else {
                        let mut kept = Vec::with_capacity_in(calls.len(), g);
                        kept.extend_from_slice(&calls);
                        kept.leak_static()
                    };
                    Said::Unchecked { why: words, calls }
                } else {
                    Said::BorrowsUnchecked(words)
                }
            });
            Ok(BodyResult { done, note: said.map(|said| (said, None)), found, locals, typed, untyped_clean, checked: true, stands: data_stands(Stands::ByReads, data_unnamed) })
        };
        #[cfg(test)]
        let outcome = if shared.plants.check_only_guard {
            let mut gather = gather;
            Ok(gather())
        } else {
            work::unwinding(gather)
        };
        #[cfg(not(test))]
        let outcome = work::unwinding(gather);

        // No body has published anything yet. On unwind discard the entire stretch, including
        // its evaluator, freezer and data-file log: they may hold prefixes of this body's work.
        // The next body starts with reset scratch and newly opened stretch records.
        if let Some(active) = recording {
            if let Some(what) = active.allocating() {
                mem.allocating = what;
            }
            mem.unfinished_reads(&active);
        }
        // A frame refusal still did this work. Use the same entry-wide counts on the
        // stage's reads line and on the named body, including the last unfinished item.
        body_tally.reads.offered = mem.body_offers;
        body_tally.reads.kept = mem.recorded;
        body_tally.reads.memo = mem.asks.memo;
        body_tally.reads.filtered = mem.asks.filtered;
        body_tally.reads.covered = mem.asks.covered;
        body_tally.reads.by_kind = mem.asks.by_kind;
        let outcome = match outcome {
            Ok(Ok(body)) => match frame_limit(mem.allocating) {
                None => Ok(body),
                limit => Err(limit),
            },
            Ok(Err(limit)) => Err(Some(limit)),
            Err(work::Panicked) => Err(frame_limit(mem.allocating)),
        };
        let stopped = outcome.is_err();
        let result = match outcome {
            Ok(result) => {
                tally.add(&body_tally);
                result
            }
            Err(exhaustion) => {
                tally.reads.add(body_tally.reads);
                mem = BodyMem {
                    scratch: sc.peak() as u64,
                    after: 0, whole: 0, names: 0, heads: 0, buckets: 0, other: 0,
                    ..mem
                };
                let (said, done, stands) = match exhaustion {
                    Some((used, reserve, what, stands)) => {
                        let mut said = Said::FrameUsedUp { used: used as u64, reserve: reserve as u64, what, offers: mem.body_offers, kept: mem.recorded, kinds: mem.kinds.top(), stands };
                        let done = if stands == Stands::ByReads {
                            let log = opened.sources.0.data.borrow();
                            // Each data read as the checked body's ([`data_read`]): a computed
                            // include's file followed too, so its `Given` task's first answer
                            // enters the refused body again; one no cell names refused by name.
                            let data = log[data_at..].iter().map(|&(path, _, bytes)| {
                                let read = data_read(shared.reads, path, bytes);
                                match read.cell {
                                    DataCell::Planned => {}
                                    DataCell::Followed => run_out.data_files.push((blaze_own::String::from_str_in(path, alloc), read.seen)),
                                    DataCell::Unnamed => {
                                        if matches!(said, Said::FrameUsedUp { .. }) && let Some(words) = shared.missing_data(one.label, path, read.id) {
                                            said = Said::Refused(words);
                                        }
                                    }
                                }
                                read.id
                            });
                            let done = shared.refused_scope(one, group, data, alloc);
                            mem.count_scope(&done);
                            done
                        } else {
                            Outcome::owed(&one.task, alloc).0
                        };
                        (said, done, stands)
                    }
                    None => {
                        let (done, stands) = Outcome::owed(&one.task, alloc);
                        (Said::Panicked, done, stands)
                    }
                };
                #[cfg(test)]
                let stands = if shared.plants.checked_on_panic && matches!(said, Said::Panicked) { Stands::ByKey } else { stands };
                BodyResult { done, note: Some((said, None)), found: Vec::new_in(sc), locals: Vec::new_in(sc), typed: Vec::new_in(sc), untyped_clean: 0, checked: false, stands }
            }
        };
        opened.sources.0.data.borrow_mut().truncate(data_at);
        opened.sources.0.left.borrow_mut().truncate(left_at);
        let index = ran.len();
        if let Some((said, head_failed)) = result.note {
            run_out.notes.push((index, said, head_failed));
        } else if result.checked && result.done.findings == 0 {
            run_out.clean += 1;
        }
        if result.checked {
            run_out.items += one.items.len() as u64;
            // A slice is counted as its entry's task once, at its first slice.
            run_out.item_tasks += u64::from(!one.items.is_empty() && one.part.is_none_or(|part| part.from == 0));
        }
        for finding in &result.found {
            run_out.messages.extend_from_slice(finding.message);
            run_out.found.push((index, finding.rule, finding.at, run_out.messages.len()));
        }
        run_out.locals.extend(result.locals.iter().map(|key| (index, *key)));
        if result.checked {
            run_out.untyped_clean = run_out.untyped_clean.saturating_add(result.untyped_clean);
            run_out.typed.reserve(result.typed.len());
            for &(check, bytes) in result.typed.iter() {
                let mut kept = SVec::with_capacity_in(bytes.len(), alloc);
                kept.extend_from_slice(bytes);
                run_out.typed.push(TypedPut { check, bytes: kept });
            }
        }
        let (made_now, hits_now) = ids.counts();
        mem.ids_made = made_now - made_before;
        mem.memo_hits = hits_now - hits_before;
        mem.scratch = mem.scratch.max(sc.used() as u64);
        mem.count_done(&result.done);
        run_out.scratch_most = run_out.scratch_most.max(mem.scratch);
        let ns = clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        let bytes = frame.used().saturating_sub(at_start) as u64;
        ran.push(Ran { name, label: one.label, ns, bytes, chain: one.place.chain, note: None, found: FindingRange::default(), file: one.file, stretch });
        out.push(result.done, result.stands);
        shared.record_memory(one, &mem);
        running.end(shared.board.now(clock));
        if stopped {
            break;
        }
    }
    // The stretch's records opened each expansion's origin column once (front2 source.rs
    // `Records::text_marks`): how many, for the stage's line.
    tally.origins_opened = tally.origins_opened.saturating_add(opened.records.opened().origins);
    taken
}

/// A verdict's line as `check` files it (body_types.rs, where a body's findings are filed; the
/// same rule numbers and message pieces): a type error as `TYPES` with its code and words, a
/// typed lint's finding by its lint's number (lints.rs `number_of`; no level kept: rule 81), a
/// move or borrow finding as `BORROWS` (a note while `BORROWS_ARE_ERRORS` is false: print.rs);
/// `None` for a not-checked line, which the body's note says. `written`: the label of a body a
/// macro wrote, or of a block's module, whose findings are at its entry's start, and the words
/// that say why. `item`: the entry's text its bodies were checked from and its edition, which
/// the entry's own findings' ranges are said as tokens of ([`FoundAt`]). `Err`: the line cannot
/// be said as a finding, and why (the body is then not checked whole: rule 3).
fn found_of<'s>(line: &Line<'_>, written: Option<Unplaced<'_>>, item: Option<(&str, blaze_surface::Edition)>, scratch: &'s Frame) -> Result<Option<BodyFinding<'s>>, &'static str> {
    if let SaidWhat::Type { code: SYNTAX_CODE } = &line.what {
        return syntax_found(line, written, item, scratch).map(Some);
    }
    let (rule, code) = match &line.what {
        SaidWhat::Type { code } => (crate::body_types::TYPES, Some(*code)),
        SaidWhat::Borrow { code } => (crate::body_types::BORROWS, Some(*code)),
        SaidWhat::Lint { lint } => match blaze_lint::Rule::from_name(lint) {
            Some(rule) => (crate::lints::number_of(rule), None),
            None => return Err("a typed lint's finding names no rule of blaze-lint"),
        },
        SaidWhat::NotChecked { .. } | SaidWhat::BorrowsNotChecked => return Ok(None),
    };
    let mut message: Vec<u8, &'s Frame> = Vec::new_in(scratch);
    if let Some(code) = code {
        message.extend_from_slice(code.as_bytes());
        message.push(0x1F);
    }
    let at = match written {
        None => {
            let (text, edition) = item.ok_or(ITEM_TEXT_UNREAD)?;
            let range = line.start as usize..line.end as usize;
            FoundAt::Tokens(blaze_file_items::token_place(text, edition, range).map_err(|_| NOT_TOKENS)?)
        }
        Some(unplaced) => {
            message.extend_from_slice(b"in `");
            message.extend_from_slice(unplaced.label.as_bytes());
            message.extend_from_slice(b"`, ");
            message.extend_from_slice(unplaced.words.as_bytes());
            FoundAt::Start
        }
    };
    message.extend_from_slice(line.words.as_str().as_bytes());
    Ok(Some(BodyFinding { rule, at, message: &*message.into_bump_slice() }))
}

// The code of a body's diagnostic that says its text does not parse (front2 project.rs
// `lowering_errors`: a lowering whose findings are all `DiagKind`s, one a `Syntax`): blaze's
// code of a syntax error, the name of `work::SYNTAX`'s wording. One home, the ladder's.
use blaze_ladder::front2::project::SYNTAX_CODE;

/// A body whose text does not parse: ONE error of the body, the parser's first error in its
/// entry's text, as [`work::SYNTAX`] with the parser's own words ("expected expression"), at its
/// tokens (rule 71). The file's outline leaves a body's syntax errors to the body (work.rs
/// `outline_file`), so this is where it is said, once. The lowering keeps no parser message
/// (blaze-lower `Diag`), so the entry's text is parsed again here, bodies and all: only for a
/// body that does not parse. `Err`: no parser error at or after the lowering's place (the two
/// parses disagree), or the body is a macro's (its text is the expansion's, not the entry's).
fn syntax_found<'s>(line: &Line<'_>, written: Option<Unplaced<'_>>, item: Option<(&str, blaze_surface::Edition)>, scratch: &'s Frame) -> Result<BodyFinding<'s>, &'static str> {
    if written.is_some() {
        return Err(SYNTAX_WRITTEN);
    }
    let (text, edition) = item.ok_or(ITEM_TEXT_UNREAD)?;
    let erased = blaze_own::Erased::new(scratch);
    let outline = blaze_file_items::outline_with(text, edition, blaze_file_items::Bodies::Parsed, erased.handle());
    let error = outline.errors().iter().find(|error| error.range.start >= line.start).ok_or(SYNTAX_UNFOUND)?;
    let range = error.range.start as usize..error.range.end as usize;
    let at = FoundAt::Tokens(blaze_file_items::token_place(text, edition, range).map_err(|_| NOT_TOKENS)?);
    let message = scratch.alloc_slice_copy(outline.message(error).as_bytes());
    Ok(BodyFinding { rule: work::SYNTAX, at, message: &*message })
}

/// A body call's read outside its package (D111; job_texts.rs `left_package`) as the body's
/// warning: rule `INCLUDE_OUTSIDE_PACKAGE`, its words names/include_outside.rs's pieces after
/// the place (the call as written in its shape, the file read and the package, each with the
/// texts' `/0/` cut), at the call's path's tokens when the call is in the entry's own text
/// (`at` is a byte of its file, `entry` the entry's range there); else at the entry's start (a
/// call a macro wrote, or a body with no file place: body_left.rs `body_place`).
fn left_found<'s>(
    (from, at, shape, call, path): crate::names::include_outside::Left<'_>,
    package: Option<&str>,
    entry: blaze_surface::Range,
    item: Option<(&str, blaze_surface::Edition)>,
    scratch: &'s Frame,
) -> BodyFinding<'s> {
    fn cut(path: &str) -> &str {
        path.strip_prefix("/0/").unwrap_or(path)
    }
    let mut message: Vec<u8, &'s Frame> = Vec::new_in(scratch);
    let (before, after): (&[u8], &[u8]) = match shape {
        blaze_surface::CallShape::FnLike => (b"", b"!"),
        blaze_surface::CallShape::Attr => (b"#[", b"]"),
        blaze_surface::CallShape::Derive => (b"#[derive(", b")]"),
    };
    message.extend_from_slice(before);
    message.extend_from_slice(call.as_bytes());
    message.extend_from_slice(after);
    message.push(0x1F);
    message.extend_from_slice(cut(path).as_bytes());
    message.push(0x1F);
    message.extend_from_slice(cut(package.unwrap_or("its member's package (not in its crate list)")).as_bytes());
    let placed = match item {
        Some((text, edition)) if !from.is_empty() && entry.start <= at && at < entry.end => {
            let start = (at - entry.start) as usize;
            blaze_file_items::token_place(text, edition, start..start + call.len()).ok()
        }
        _ => None,
    };
    let at = placed.map_or(FoundAt::Start, FoundAt::Tokens);
    BodyFinding { rule: crate::names::include_outside::INCLUDE_OUTSIDE_PACKAGE, at, message: &*message.into_bump_slice() }
}

/// Why a body's syntax error has no place: its text is a macro's expansion.
const SYNTAX_WRITTEN: &str = "a body a macro wrote does not parse: its text is the expansion's, which its entry's text does not hold, so its syntax error has no place (engine_bodies.rs `syntax_found`)";
/// Why a body's syntax error has no words: the parser found none where the lowering did.
const SYNTAX_UNFOUND: &str = "the lowering found a syntax error the parser of the entry's text did not (engine_bodies.rs `syntax_found`: the two parses disagree)";

/// Why a body's finding has no place: its entry's text was not in its crate's records.
const ITEM_TEXT_UNREAD: &str = "its entry's text is not in its crate's records (front2 `Records::file_now`), so its findings cannot be placed by tokens";
/// Why a body's finding has no place: its range is no range of its item's tokens.
const NOT_TOKENS: &str = "a finding's range starts or ends in trivia or cuts a token of its item (blaze_file_items::token_place, rule 71)";

/// The note of the bodies a macro wrote that no entry covers: how many, the first by label.
struct Uncovered<'l> {
    count: u32,
    named: &'l [&'static str],
}

impl core::fmt::Display for Uncovered<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} body(ies) a macro wrote are checked by no task: the walk up from their call ends at no file entry and in no block's module (body_types/covered.rs `written_by`, `in_blocks`)", self.count)?;
        for (at, label) in self.named.iter().enumerate() {
            f.write_str(if at == 0 { ": " } else { ", " })?;
            write!(f, "`{label}`")?;
        }
        let rest = (self.count as usize).saturating_sub(self.named.len());
        if rest > 0 {
            write!(f, ", and {rest} more")?;
        }
        write!(f, "; not checked ({RULE})")
    }
}

/// Why an entry has no body: its unit's cfg is not in its crate's records.
const CFG_ATOMS_UNREAD: &str = "its crate's cfg atoms are not in its records (front2 source.rs `cfg_atoms`), so whether a `cfg` left this entry out cannot be told; not checked";
/// Why an entry has no body: a `cfg` predicate on it does not read (body_types.rs `CFG_UNREAD`).
const CFG_UNREAD: &str = "a `cfg` predicate on it or on an item it is written in does not read (blaze-scope `Cfg::eval`), so whether its crate holds it cannot be told; not checked";
/// Why an entry has no body: its crate holds it under its cfg, yet its records list no body there.
const ENTRY_UNLISTED: &str = "its crate holds it under its cfg, yet its crate's records list no body at this entry and no macro wrote one at it (front2 project_records.rs `index_by_chain`); not checked";
/// Why the root's replaced `fn main` has no body to check: front2's root `main` has no place.
const ROOT_MAIN_UNPLACED: &str = "this is the root's `fn main`, which D104's launcher replaced; the root `main` has no place in the records (body_types/covered.rs `root_of`); not checked";

/// A body entry of a held file at which its unit's records list no body and no macro wrote one
/// ([`BodiesStage::groups`], step 3), as body_types/covered.rs `root_of` and body_types.rs
/// `Checker::holds` decide it (the reference for coverage): `Err(ENTRY_ABSENT)` when a `cfg` on
/// it or on an item it is written in is false under the unit's cfg (not this unit's body: the
/// Reference, "Conditional compilation"); the key of the launcher's `main` when it is the root
/// file's top-level `fn main` (D104: the launcher `fn main() -> Exit { fn main(..) <the written
/// body> .. }`; its local `main`, the body the user wrote, is checked by the task's work list,
/// front2 tables/task.rs); else a refusal naming why. Never a silent absence (rule 3).
fn absent_entry<'n>(
    records: &'n blaze_ladder::front2::source::Records<'n>,
    outline: &blaze_file_items::Outline<'_>,
    item: &blaze_surface::Item,
    text: &str,
    root_file: bool,
    cfg: Option<&blaze_scope::task::Cfg<'_, '_>>,
    g: blaze_own::Alloc<'static>,
) -> Result<ItemKey, &'static str> {
    if planted_silent_absent() {
        return Err(crate::engine_body_plan::ENTRY_ABSENT);
    }
    let Some(cfg) = cfg else { return Err(CFG_ATOMS_UNREAD) };
    // Up the parents, bounded by the outline's length (rule 18).
    let all = outline.items();
    let mut at = Some(item);
    let mut steps = 0usize;
    while let Some(entry) = at {
        let attrs = blaze_scope::task::item_attrs(cfg, outline, entry, text, g);
        if attrs.cfg_unread > 0 {
            return Err(CFG_UNREAD);
        }
        if !attrs.cfg_ok {
            return Err(crate::engine_body_plan::ENTRY_ABSENT);
        }
        steps += 1;
        if steps > all.len() {
            return Err(CFG_UNREAD);
        }
        at = entry.parent.and_then(|p| all.get(p.0 as usize));
    }
    let root_main_entry = root_file && item.parent.is_none() && matches!(item.kind(), blaze_surface::ItemKind::Function) && crate::item_name::shown_name(item, text) == "main";
    if !root_main_entry {
        return Err(ENTRY_UNLISTED);
    }
    let key = blaze_ladder::front2::program::run_entry::root_main(records)
        .map_err(|why| crate::engine_head_notes::leak(g, format_args!("this is the root's `fn main`, which D104's launcher replaced, and the launcher's `main` is not found: {why} (body_types/covered.rs `root_of`); not checked")))?;
    match NamesSource::place_of(records, key).map(|p| p.at) {
        Some(blaze_scope::resolve::At::Expansion(_)) => Ok(key),
        Some(blaze_scope::resolve::At::File(file)) => Err(crate::engine_head_notes::leak(
            g,
            format_args!("this is the root's `fn main`, but the root `main` the records list is written in file {file} and not at this item (body_types/covered.rs `root_of`); not checked"),
        )),
        None => Err(ROOT_MAIN_UNPLACED),
    }
}

/// A member's lent environment copied into `frame` (no allocator of the lending thread is
/// called here, crate_env.rs `EnvLent`), as body_types.rs does for its workers.
fn env_in<'l: 'n, 'n>(lent: Option<EnvLent<'_, 'l>>, frame: &'n Frame) -> Result<Option<&'n blaze_scope::crate_env::CrateEnv<'n>>, blaze_build_script::outputs::RecordError> {
    let Some(lent) = lent else { return Ok(None) };
    let erased: &'n blaze_own::Erased<&'n Frame> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(blaze_own::Erased::new(frame), frame));
    let env = lent.copy_in(erased.bump())?;
    Ok(Some(&*blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(env, frame))))
}

/// The stretch's evaluator appends distinct keys. Place just the new suffix, retaining at most
/// two IDs per key (the question and, for an item, its text). Commit a key only after both
/// placements succeed, so an unplaceable read continues to refuse every row that needs it.
struct EvalReads<'a> {
    placed: usize,
    ids: blaze_own::AVec<'a, CellId>,
}

enum EvalReadError {
    Full(blaze_ladder::front2::tables::recorder::ReadsFull),
    Id(TaskError),
    Text(crate::engine_head_consts::Unplaced),
}

impl EvalReadError {
    fn place<'n>(
        key: TableRead,
        memo: &mut ReadIds<'_, '_, '_>,
        records: &'n blaze_ladder::front2::source::Records<'n>,
        subjects: &crate::engine_heads::Subjects,
    ) -> Result<Option<CellId>, Self> {
        if key.is_source() {
            crate::engine_head_consts::source_read(records, subjects, key).map_err(Self::Text)
        } else {
            memo.id(key).map_err(Self::Id)
        }
    }

    fn words(&self, crates: &[Crate<'_>], g: blaze_own::Alloc<'static>) -> &'static str {
        match self {
            Self::Full(full) => leak(g, format_args!("its constants' evaluation at the seed read too much: {full}")),
            Self::Id(error) => leak(g, format_args!("a names read of its constants' evaluation at the seed: {error}")),
            Self::Text(unplaced) => {
                let shown = crate::engine_head_consts::UnplacedIn { unplaced, crates };
                leak(g, format_args!("its constants' evaluation at the seed read a text no crate of its scope holds: {shown}"))
            }
        }
    }
}

impl EvalReads<'_> {
    fn update<'n>(&mut self, evaluated: &Recorder<'_>, index_reads: u64, memo: &mut ReadIds<'_, '_, '_>,
        records: &'n blaze_ladder::front2::source::Records<'n>, subjects: &crate::engine_heads::Subjects) -> Result<(), EvalReadError> {
        let keys = evaluated.keys().map_err(EvalReadError::Full)?;
        if evaluated.tally().offered == index_reads {
            return Ok(());
        }
        for &key in &keys[self.placed..] {
            self.ids.extend(EvalReadError::place(key, memo, records, subjects)?);
            self.placed += 1;
        }
        Ok(())
    }
}

type DistinctIds<'w> = blaze_hash::HashSet<CellId, blaze_own::Alloc<'w>>;

/// Place each raw read according to the command's scope strategy. Only row_of asks the view
/// for a digest, after the evaluator, the plan and the data-file reads have all joined.
fn widen_as<'w, 'n, 's: 'n, R: ?Sized + CellDigests>(
    shared: &Shared<'_, '_, '_, '_, R>,
    reads: &[CellId],
    group: &Group<'_, '_>,
    owners: &mut crate::engine_heads::Owners<'s>,
    records: &'n blaze_ladder::front2::source::Records<'n>,
    work: blaze_own::Alloc<'w>,
    mem: &mut BodyMem,
) -> (DistinctIds<'w>, Option<(Name, Cell)>) {
    use crate::engine_scope_reads::{Cells, ScopeReadsAs, Whole};
    mem.allocating = "the widened cell ID hash table";
    #[cfg(test)]
    if mem.fault == Some(BodyFault::WidenPanic) {
        panic!("planted panic in body read widening");
    }
    match shared.scope_reads {
        ScopeReadsAs::Whole => widen::<Whole, R>(shared, reads, group, owners, records, work, mem),
        ScopeReadsAs::Cells => widen::<Cells, R>(shared, reads, group, owners, records, work, mem),
        #[cfg(test)]
        ScopeReadsAs::PlantedCells => widen::<crate::engine_scope_reads::PlantedCells, R>(shared, reads, group, owners, records, work, mem),
    }
}

fn widen<'w, 'n, 's: 'n, S: crate::engine_scope_reads::ScopeReads, R: ?Sized + CellDigests>(
    shared: &Shared<'_, '_, '_, '_, R>,
    reads: &[CellId],
    group: &Group<'_, '_>,
    owners: &mut crate::engine_heads::Owners<'s>,
    records: &'n blaze_ladder::front2::source::Records<'n>,
    work: blaze_own::Alloc<'w>,
    mem: &mut BodyMem,
) -> (DistinctIds<'w>, Option<(Name, Cell)>) {
    let mut unfrozen = None;
    let mut wide = DistinctIds::new_in(work);
    #[cfg(test)]
    let own_key = group.crates.own().expect("a group's member is its crate 0").records.key;
    #[cfg(test)]
    let own_tables = blaze_ladder::front2::buckets::impl_tables_name(own_key);
    for &id in reads {
        #[cfg(test)]
        if shared.plants.all_dependencies_settled && match id.of.kind {
            Kind::CrateScope => id.of != group.own,
            Kind::ImplBucket => id.of.subject != ABSENT && id.of != own_tables,
            Kind::Signature => id.of.subject.0[..8] != own_key.prefix(),
            Kind::FileItems => !shared.heads.iter().any(|subject| subject.prefix == own_key.prefix() && subject.slot == id.of),
            _ => false,
        } {
            mem.place(&mut wide, CellId { of: Name::unit_graph(), cell: WHOLE });
            continue;
        }
        let placed = match id.of.kind {
            Kind::CrateScope if !S::keeps(group.own, id) => continue,
            Kind::ImplBucket if id.of.subject == ABSENT => {
                S::incoherent_fact(&group.scope, &mut |of, cell| mem.place(&mut wide, CellId { of, cell }));
                continue;
            }
            Kind::Signature => match owners.place(records, &id, shared.heap.alloc()) {
                Some(crate::engine_heads::HeadAt::Cell { cell, .. }) => {
                    // Earliest failed cell in canonical order, independent of recording order.
                    let at = (cell.of, cell.cell);
                    if unfrozen.is_none_or(|first| at < first) && (shared.reads.row_failed(cell.of) == Some(true) || shared.heads_failed.has(cell.of, cell.cell)) {
                        unfrozen = Some(at);
                    }
                    cell
                }
                Some(crate::engine_heads::HeadAt::Dependency(of) | crate::engine_heads::HeadAt::OfTheBody(of) | crate::engine_heads::HeadAt::Unlisted { scope: of, .. }) => CellId { of, cell: WHOLE },
                None => id,
            },
            _ => id,
        };
        mem.place(&mut wide, shared.grains.place(placed, shared.plants.fine_buckets));
    }
    if let Some(of) = group.script {
        mem.place(&mut wide, CellId { of, cell: WHOLE });
    }
    (wide, unfrozen)
}

fn row_of<'a, R: ?Sized + CellDigests>(mut wide: DistinctIds<'_>, held: &[CellId], data: &[CellId], shared: &Shared<'_, '_, '_, '_, R>, mem: &mut BodyMem, scratch: &Frame, alloc: SharedAlloc<'a>) -> SVec<'a, (CellId, ContentId)> {
    mem.allocating = "the row's cell ID hash table";
    for &id in held.iter().chain(data) {
        mem.place(&mut wide, id);
    }
    mem.placed_all = true;
    mem.allocating = "the row's sorted cell ID list";
    #[cfg(test)]
    if mem.fault == Some(BodyFault::RowFrame) {
        // Leave less than one CellId free in this body's small reservation. The ordinary
        // sorted-ID allocation below must meet the cap, after the checker produced findings.
        let room = scratch.reserved() - scratch.used();
        scratch.alloc_layout(core::alloc::Layout::array::<u8>(room).expect("small test scratch"));
    }
    let mut ids = Vec::with_capacity_in(wide.len(), scratch);
    mem.work.cells += (wide.capacity() * (core::mem::size_of::<CellId>() + 1) + ids.capacity() * core::mem::size_of::<CellId>()) as u64;
    ids.extend(wide);
    ids.sort_unstable();
    let room = if shared.plants.row_at_recorded { (mem.before as usize).max(ids.len()) } else { ids.len() };
    mem.allocating = "the row's cell and content ID list";
    let mut row = SVec::with_capacity_in(room, alloc);
    row.extend(ids.iter().map(|id| (*id, shared.reads.cell(id))));
    mem.count_row(&row);
    row
}

/// Which cell a body's read of an included data file is ([`data_read`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DataCell {
    /// The file's `FileItems` record's `WHOLE`: today's plan lists the file (a literal include,
    /// data_include.rs), its digest the record's out (`data_out`, or `absent_out`).
    Planned,
    /// Its checkout-read `Given` cell (engine_script_reads.rs `file_read`): no `FileItems` task
    /// defines the file (a computed include), so the read is kept as a leaf row, followed by the
    /// sweep and planned as its `Given` task from the next request on, as a build script's
    /// `file:` read of it is. Its digest is the sandbox's answer of what the body was handed.
    Followed,
    /// No cell names it (no checkout-relative path): the row is owed ([`data_stands`]).
    Unnamed,
}

/// One body's read of an included data file: its cell, what it saw there, and which cell it is.
struct DataRead {
    id: CellId,
    seen: ContentId,
    cell: DataCell,
}

/// The cell a body's read of the data file `path` is, given what it was handed (`bytes`, job_texts.rs
/// `read_data`). A plan that lists the file makes it [`DataCell::Planned`]; else the read is the
/// file's `Given` cell, whose leaf the request keeps (`RunOut::data_files`). The digest the row
/// keeps is what the body saw, so a file edited between the read and the next request's sweep
/// enters the body when its `Given` task defines the cell with the new answer: the first
/// request needs no owing, and the next enters no body when nothing moved.
fn data_read<R: ?Sized + CellDigests>(reads: &R, path: &str, bytes: Option<&[u8]>) -> DataRead {
    let file = CellId { of: Name::file(path.as_bytes()), cell: WHOLE };
    let planned = || bytes.map_or_else(crate::data_include::absent_out, crate::data_include::data_out);
    if reads.row_failed(file.of) == Some(false) {
        let seen = planned();
        // Only bind to the `FileItems` task when its `WHOLE` cell IS a `data_include` digest
        // (pure `DATA_FILE`) or its `DATA` cell holds the raw bytes digest (a Rust source file
        // also read via `include_str!` / `include_bytes!`); otherwise fall through to its `Given` cell.
        if reads.cell(&file) == seen {
            return DataRead { id: file, seen, cell: DataCell::Planned };
        }
        let data = CellId { of: file.of, cell: crate::engine_tasks::DATA };
        if reads.cell(&data) == seen {
            return DataRead { id: data, seen, cell: DataCell::Planned };
        }
    }
    match crate::engine_script_reads::file_read(path, bytes) {
        Some((given, answer)) => DataRead { id: CellId { of: given, cell: WHOLE }, seen: answer, cell: DataCell::Followed },
        None => DataRead { id: file, seen: planned(), cell: DataCell::Unnamed },
    }
}

/// How a body's row stands when one of its data reads has no cell at all (`unnamed`): owed to
/// the next request, whatever its reads say, since nothing moves for that file when it is
/// edited (blaze-engine invariants 4 and 13); else as `stands`.
fn data_stands(stands: Stands, unnamed: bool) -> Stands {
    match unnamed {
        true => Stands::OwedNextRequest,
        false => stands,
    }
}

/// Each included data file's read in `row` holds the digest the body saw (job_texts.rs
/// `read_data`), not today's cell: equal while the file's `FileItems` record ran on the same
/// bytes, and the bytes it read when the file moved under the request or no record defines it,
/// so the row is entered again exactly when its cell holds another digest. `row` is sorted by
/// cell ([`row_of`], `Shared::refused_scope`), and each holds every data read: an empty row is
/// a row on no read (a panic), owed whatever it read.
fn saw_data(row: &mut SVec<'_, (CellId, ContentId)>, seen: &[(CellId, ContentId)]) {
    if row.is_empty() {
        return;
    }
    for &(id, digest) in seen {
        // Placed as it is by both (`BodyMem::place` inserts the id itself): a miss is this
        // file's bug, failing the one body (rule 20).
        let at = row.binary_search_by_key(&id, |(cell, _)| *cell)
            .unwrap_or_else(|_| unreachable!("a body's row lost its read of the data file {id:?} (engine_bodies.rs `row_of`)"));
        row[at].1 = digest;
    }
}

/// `more` added into `into`, part by part.
fn add_work(into: &mut WorkBytes, more: &WorkBytes) {
    into.recorder = into.recorder.max(more.recorder);
    into.recorder_floor += more.recorder_floor;
    into.cells += more.cells;
    into.held += more.held;
    into.said += more.said;
    into.verdict = into.verdict.max(more.verdict);
    into.goal_hits += more.goal_hits;
}
