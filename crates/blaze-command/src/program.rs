//! The command's program path: the bodies of the driver's three program methods of
//! [`blaze_keyed::front::Front`] for the real command ([`plan`], [`code`], [`link`]), the
//! walker's reader of the front end's plans ([`Reader`]), and a program's roots ([`roots`]).
//! Each is a free function over what it is handed; `impl Front for Command` calls each in one
//! line, with the policy and front settings the command holds.
//!
//! - [`plan`]: one instance's or static's plan record, over a snapshot of the program's
//!   closure with every crate given from its surface and the bodies the driver gave installed
//!   as the item store (`hir_ty::body_store::install_given`): its describe reads no body's text
//!   and checks nothing again (a body record is the proof). A read of a body not given unwinds
//!   with `Unserved` (`NO_RECORD`), which is caught here and returned as the driver's
//!   `TaskFailed::NoBody` ([`blaze_front::body::mir::failed`]).
//! - [`code`]: one code record: the plan's places from its edges and its callees' plans
//!   ([`code::places`]), compiled by [`code::code`] or [`code::code_static`], filed under the
//!   driver's key, which must be the one [`Reader::code_key`] made (else refused by name).
//! - [`link`]: one link step: the rows that came or moved instantiated
//!   ([`code::instantiated_rows`]), the rows that went or moved removed by the instance key the
//!   kept payload holds for them ([`KEPT_MAGIC`]), then `blaze_link::step` on the linker's
//!   kept state; its patch handed back in the driver's form for the host to apply to a clone.
//! - [`spans`]: one owner's span tables (`Front::spans`), with [`span_file_fact`] the read each
//!   holds of a file and [`printed_name_id`] the one name a file is known by.
//! - [`plan_anchors`]: a plan's own span anchors and their owners, with no database, for the
//!   walker.
//!
//! What the kept payload is: the link's own row table (each row's plan name, what it is to the
//! link, its code key and the symbol it defines), sorted by plan name, then the linker's kept
//! state. The table is there because a row that went has no plan any more (`Front::link` gives
//! plans only of rows that came or moved), and the link removes a function by its instance
//! key and a static by its item: the table answers both, and the names a gone row unbinds.
//!
//! Nothing on the plan, code and link path reads a file's name, text or root (rule 25): a
//! plan's and a code record's keys hold none, and the front end is made naming no file
//! ([`code::front_end`]). Only [`spans`] reads text and names files, and its records read what they
//! name ([`span_file_fact`]).

#![blaze::sans_io]

use core::alloc::Allocator;
use std::{
    alloc::Global,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
};

use base_db::FileId;
use blaze_codegen::{InstanceKey, plan::Policy};
use blaze_core::{FileName, Mapping};
use blaze_format::Aligned;
use blaze_hash::HashSet;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, ItemKey, key::Resolved};
use hir_def::DefWithBodyId;
use blaze_keyed::{
    driver::TaskFailed,
    front::{GivenBody, Linked, Patch, PatchBase as DriverBase, PatchWrite, SpanTables, Universe},
    kind::{
        CodeRecord as CodeKind, CodeRecordInputs, Key, Kind as _, PlanRecord as PlanKind, SpanTable as SpanTableKind,
        SpanTableInputs, ToolId,
    },
    program::{RowChange, SpanAt},
    record::{self, Fact, StableId},
};
use blaze_link::{Change, FileEdit, Output, PatchBase};
use blaze_program::{
    key::Tier,
    walker::{Edge as WalkEdge, Root, Target as WalkTarget},
};
use hir_ty_codegen::v2::{
    Edge, Edges, HashSource, HirFrontend, ItemHashes, PLAN_MAGIC, PlanView, RootKind,
    STATIC_ASK_MAGIC, STATIC_PLAN_MAGIC, StaticPlanView, owner_span_tables, static_ask,
};

use crate::{
    code::{self, CodeError, FrontSettings, Rows},
    front::Command,
    snapshot::{self, Snapshot, Task, refused},
};

/// A plan or code task's given bodies did not install as the item store.
pub const NO_GIVEN_STORE: &str = "a program task: the given body records did not install as the item store (`hir_ty::body_store::install_given`)";
/// A plan payload of neither plan layout.
pub const NOT_A_PLAN: &str = "a program task: a plan payload is neither a function's (`bzplan02`) nor a static's (`bzsplan1`) plan";
/// A code key the driver handed in that is not the one the walker's reader makes.
pub const KEY_NOT_THE_READERS: &str = "a code task: the key it was given is not the code key the walker's reader makes of its plan and callees (`program::Reader::code_key`); rule keys-cover-every-input";
/// A read the plan made that two answers stand for.
pub const TWO_ANSWERS: &str = "a plan task: one fact of its read list has two answers (a given body's MIR fact and a fact its describe read under one id)";
/// The walker asked a tier the code path does not make.
pub const NO_RELEASE_TIER: &str = "a code key: the walker asked the release tier, which the code path does not make yet (only the dev tier: `code::code_key`)";
/// Targets that are not one per instance of the plan's edges.
pub const TARGETS_DIFFER: &str = "a code key: the walker's targets are not one per instance the plan's edges name";
/// An ask past the u32 its length is.
pub const ASK_TOO_LONG: &str = "a plan edge: its ask is past 4 GiB";
/// A plan that fails in its front end (the line said names it).
pub const PLAN_FAILED: &str = "a plan task: the front end could not make or read the plan (`hir_ty_codegen::v2::PlanFailed`)";
/// A compile that failed (the line said names the instance).
pub const CODEGEN_FAILED: &str = "a code task: the describe, prepare or emit failed (`blaze_codegen::CodegenError`)";
/// A code record that is no template.
pub const NOT_A_TEMPLATE: &str = "a link step: a code record's payload is not a template (`blaze_codegen::blob`)";
/// A template that does not instantiate.
pub const NOT_INSTANTIATED: &str = "a link step: a template does not instantiate at its places (`blaze_codegen::template`)";
/// An edge naming a plan the task was not given.
pub const NO_PLAN: &str = "a program task: an edge names a plan the task was not given";
/// Rows that are not closed.
pub const UNLINKED: &str = "a link: a row's callee or static is no row (`code::link_rows`)";
/// A link step whose changes do not fit the kept rows.
pub const KEPT_DIFFERS: &str = "a link step: a change's old code is not the one the kept payload holds for its instance (or the instance is not kept, or is kept and the change says it came)";
/// Changes not in instance order.
pub const CHANGES_UNSORTED: &str = "a link step: its changes are not in instance order, each once";
/// A change with code and no record, or no plan.
pub const NO_CODE: &str = "a link step: a row that came or moved has no code record, or its record does not open under its key";
/// A kept payload of another layout.
pub const NOT_KEPT: &str = "a link step: the kept payload is not `bzlkept1` and its row table";
/// The linker refused the change or its kept state.
pub const LINK_FAILED: &str = "a link step: blaze-link refused the change or its kept state (`blaze_link::StepError`)";
/// A root of the program with no portable ask.
pub const ROOT_NOT_PORTABLE: &str = "a program's roots: a root has no portable ask (`hir_ty_codegen` `put_instance` refused its instance)";
/// A link step's patch past what the driver's `Patch` holds.
pub const PATCH_TOO_LONG: &str = "a link step: its patch writes 4 GiB or more, past the u32 offsets of the driver's `Patch`";
/// A span task's owner that is no body of its unit.
pub const SPAN_NO_OWNER: &str = "a span task: its owner does not resolve to a function, const or static of its unit (`blaze_items::key::resolve`)";
/// A span table that could not be made (the line said names the owner and why).
pub const SPAN_FAILED: &str = "a span task: the owner's span tables could not be made (`hir_ty_codegen::v2::owner_span_tables`)";
/// A file a span table names that is not one of the owner's unit's gathered files.
pub const SPAN_FOREIGN_FILE: &str = "a span task: a table names a file of another crate (a span a macro of another crate made), which the task has no gathered text and no read of";
/// A file named by a span table that the command cannot name as the program sees it.
pub const SPAN_FILE_UNNAMED: &str = "a span task: a table names a file of a unit that is neither std's nor a workspace member's (a registry crate's is named by its absolute path, which no record may hold: rule deterministic-output), or a file outside its unit's root";
/// The roots the front end could not list.
pub const NO_ROOTS: &str = "a program's roots: the front end could not list them (`HirFrontend::roots`)";

/// The first bytes of the kept payload [`link`] writes.
pub const KEPT_MAGIC: &[u8; 8] = b"bzlkept1";

/// Which plan layout `payload` is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Layout {
    Function,
    Static,
}

fn layout_of(payload: &[u8]) -> Result<Layout, TaskFailed> {
    match payload.get(..8) {
        Some(magic) if magic == PLAN_MAGIC => Ok(Layout::Function),
        Some(magic) if magic == STATIC_PLAN_MAGIC => Ok(Layout::Static),
        _ => Err(refused(NOT_A_PLAN)),
    }
}

/// The edges of the plan `payload`, of either layout.
fn edges_of(payload: &[u8]) -> Result<Edges<'_>, TaskFailed> {
    Ok(match layout_of(payload)? {
        Layout::Function => PlanView::read(payload).map_err(refused)?.edges(),
        Layout::Static => StaticPlanView::read(payload).map_err(refused)?.edges(),
    })
}

/// The driver's key of the code record whose template identity is `template`.
pub fn code_record_key(template: &blaze_cas::Key) -> Key<CodeKind> {
    CodeKind::key(&CodeRecordInputs { template: template.0 })
}

/// The rule a code path error is refused by.
fn rule_of<A: Allocator>(error: &CodeError<A>) -> TaskFailed {
    match error {
        CodeError::Task(failed) => *failed,
        CodeError::Plan(_) => refused(PLAN_FAILED),
        CodeError::Codegen(_) => refused(CODEGEN_FAILED),
        CodeError::Blob(_) => refused(NOT_A_TEMPLATE),
        CodeError::Template(_) => refused(NOT_INSTANTIATED),
        CodeError::NoPlan { .. } => refused(NO_PLAN),
        CodeError::Unlinked { .. } | CodeError::UnlinkedStatic { .. } => refused(UNLINKED),
    }
}

/// `error` as the driver's failure, its words told for the summary under `program` (rule 19:
/// the `TaskFailed` carries only the rule), naming the task and its instance ([`Of`]).
fn failed<A: Allocator + Clone, E: Allocator>(
    command: &Command<'_, A>,
    program: u32,
    of: impl fmt::Display,
    error: CodeError<E>,
) -> TaskFailed {
    let rule = rule_of(&error);
    if !matches!(error, CodeError::Task(_)) {
        command.tell(program, format_args!("{of}: {}: {error:?}", rule.what()));
    }
    rule
}

/// Which task a failure is of and the instance it works on, as its told line names them.
/// Printed only on a failure, two ways:
///
/// - after the work returned an error ([`Of`] itself): "the plan task of `<path>` (plan
///   <key>)", the path the instance's (with its type arguments) or a static's
///   (`HirFrontend::printed_ask`), asked of the database, which the work left whole;
/// - after the work unwound ([`Unwound`]): "the plan task of plan <key>", never asking the
///   database, whose read just stopped (a typed stop's reason, told beside it, names the
///   crate and file; asking the name would read the same crate and stop again).
///
/// The key is the record's driver key (the plan's or the code record's), in hex: the id the
/// driver's profile and the program's rows name it by.
struct Of<'f, 'db, 'a> {
    task: &'static str,
    frontend: &'f HirFrontend<'db>,
    ask: Option<&'a [u8]>,
    key: ContentId,
}

impl fmt::Display for Of<'_, '_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The name's own read may stop as the work's did (it reads the item's crate): caught,
        // and the line names the record alone.
        let name = catch_unwind(AssertUnwindSafe(|| self.ask.and_then(|ask| self.frontend.printed_ask(ask))));
        match name {
            Ok(Some(name)) => write!(f, "the {} task of `{name}` (record {})", self.task, Hex(&self.key)),
            Ok(None) => write!(f, "the {} task of record {}, whose ask this snapshot does not read", self.task, Hex(&self.key)),
            Err(_) => write!(f, "the {} task of record {}, whose name stopped to print", self.task, Hex(&self.key)),
        }
    }
}

/// [`Of`] after the work unwound: the task and its record's key alone, with no database.
struct Unwound<'o, 'f, 'db, 'a>(&'o Of<'f, 'db, 'a>);

impl fmt::Display for Unwound<'_, '_, '_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the {} task of record {}", self.0.task, Hex(&self.0.key))
    }
}

/// A content id in lowercase hex, as the driver's rows print one.
struct Hex<'i>(&'i ContentId);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// What a link task's told line is of: it works on the program, not one instance.
const LINK_TASK: &str = "the link task";

/// `work`, an error refused by its rule; a typed stop it unwound with (a body read not given,
/// a file with no text, frozen bytes that do not read) told under `program` and failed by its
/// rule, as a unit task's is (`Command::stop_of`, the one home of those payloads), after a
/// line naming the task and its record ([`Unwound`]); every other unwind told the same way
/// with its message, then passed on (rule 20: the host fails this one job).
fn caught<T, A: Allocator + Clone + Send + Sync>(
    command: &Command<'_, A>,
    program: u32,
    of: &Of<'_, '_, '_>,
    work: impl FnOnce() -> Result<T, CodeError<Global>>,
) -> Result<T, TaskFailed> {
    let payload = match catch_unwind(AssertUnwindSafe(work)) {
        Ok(Ok(made)) => return Ok(made),
        Ok(Err(error)) => return Err(failed(command, program, of, error)),
        Err(payload) => payload,
    };
    let unwound = Unwound(of);
    // Told before the stop's own line, so the summary reads which task, then why.
    match payload.downcast_ref::<String>().map(String::as_str).or_else(|| payload.downcast_ref::<&str>().copied()) {
        Some(text) => command.tell(program, format_args!("{unwound} panicked: {text}")),
        None => command.tell(program, format_args!("{unwound} stopped, typed (its reason on the next line where it is a refusal)")),
    }
    match command.stop_of(program, &*payload) {
        Some(failed) => Err(failed),
        None => resume_unwind(payload),
    }
}

/// The front end of a program task over `snapshot`, naming no file.
fn front_of<'db, A: Allocator>(snapshot: &'db Snapshot<A>, policy: &Policy, settings: FrontSettings) -> HirFrontend<'db> {
    code::front_end(snapshot, policy, settings)
}

/// `Front::plan` for the command: the plan record named `key` of program `program`, whose
/// task is given `ask` (a function's or a static's, by its magic), over `bodies` and
/// `universe`, written with `record::write_read` (the read list sorted by id, each once: every
/// fact the describe read, and each given body's `record::mir_id` with its result).
///
/// Cost against the floor (the instance's describe plus one lookup per fact it reads): the
/// describe and the lookups, plus the lease ([`snapshot::lease_plan`]: on a kept snapshot one
/// table lookup and record head per closure crate for the universe's digest, the pool's lock,
/// a merge per kept snapshot and the given store swapped; on a new one an O(closure crates)
/// open with no texts, find index or trace store), one copy of each given record into the
/// store's holders, one copy of the read list, and one copy of the record into the returned
/// `Aligned`. The per-crate digest work is the part over the floor on a kept snapshot; a whole
/// open still comes for a task whose bodies cover no kept snapshot's.
#[allow(clippy::too_many_arguments)]
pub fn plan<A: Allocator + Clone + Send + Sync>(
    command: &Command<'_, A>,
    program: u32,
    ask: &[u8],
    key: Key<PlanKind>,
    bodies: &[GivenBody<'_>],
    universe: Universe<'_>,
    policy: &Policy,
    settings: FrontSettings,
) -> Result<Aligned, TaskFailed> {
    let leased = snapshot::lease_plan(command, program, &universe, bodies)?;
    let snapshot: &Snapshot<A> = &leased;
    let frontend = front_of(snapshot, policy, settings);
    let name = key.id();
    let of = Of { task: "plan", frontend: &frontend, ask: Some(ask), key: key.id() };
    let made = caught(command, program, &of, || match ask.get(..STATIC_ASK_MAGIC.len()) {
        Some(magic) if magic == STATIC_ASK_MAGIC => {
            code::plan_static(snapshot, &frontend, ask, &name, universe, Global)
        }
        // Any other ask is a function's: `plan` refuses one of another layout by name.
        _ => code::plan(snapshot, &frontend, ask, &name, universe, Global),
    })?;
    let mut reads: Vec<Fact, Global> = Vec::with_capacity_in(made.reads.len() + bodies.len(), Global);
    reads.extend_from_slice(&made.reads);
    reads.extend(bodies.iter().map(|body| Fact { id: record::mir_id(&body.item), digest: body.result }));
    reads.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    if reads.windows(2).any(|pair| pair[0].id == pair[1].id && pair[0].digest != pair[1].digest) {
        return Err(refused(TWO_ANSWERS));
    }
    reads.dedup_by_key(|fact| fact.id);
    let (_, bytes) = record::write_read::<PlanKind>(key, &reads, &[], &made.payload);
    Ok(Aligned::copy_from(&bytes))
}

/// `Front::code` for the command: the code record of plan `plan` (a payload of either layout)
/// at the places its edges resolve to through `callees` (sorted by instance, each once), filed
/// with `record::write_opaque` under `key`, which must be [`code_record_key`] of the
/// template identity the compile made (else [`KEY_NOT_THE_READERS`]). The payload is the
/// template's blob (`blob::encode` or `blob::encode_static`).
///
/// `universe` and `bodies` are the plan's: the compile reads the instance's MIR and layouts
/// from a snapshot, leased from the plan's pool ([`snapshot::lease_plan`]), where its plan's
/// snapshot was put back.
///
/// Cost against the floor (prepare and emit of one function, or one static's value): those,
/// plus the lease (as [`plan`]'s), one pass over the edges for the places, one
/// encode of the template and two copies of it (into the record, into the `Aligned`). The
/// side table (debug info and panic locations) is dropped: no seam files it yet.
#[allow(clippy::too_many_arguments)]
pub fn code<A: Allocator + Clone + Send + Sync>(
    command: &Command<'_, A>,
    program: u32,
    key: Key<CodeKind>,
    plan: &[u8],
    callees: &[(Key<PlanKind>, &[u8])],
    bodies: &[GivenBody<'_>],
    universe: Universe<'_>,
    policy: &Policy,
    settings: FrontSettings,
) -> Result<Aligned, TaskFailed> {
    let leased = snapshot::lease_plan(command, program, &universe, bodies)?;
    let frontend = front_of(&*leased, policy, settings);
    let mut plans = Vec::with_capacity_in(callees.len(), Global);
    plans.extend(callees.iter().map(|(instance, bytes)| (instance.id(), *bytes)));
    let ask = match layout_of(plan)? {
        Layout::Function => PlanView::read(plan).ok().map(|view| view.ask),
        Layout::Static => StaticPlanView::read(plan).ok().map(|view| view.ask),
    };
    let of = Of { task: "code", frontend: &frontend, ask, key: key.id() };
    let (template, bytes) = match layout_of(plan)? {
        Layout::Function => caught(command, program, &of, || {
            let view = PlanView::read(plan)?;
            let places = code::places(policy, view.edges(), &plans, Global)?;
            let made = code::code(&frontend, policy, plan, &places, Global)?;
            Ok((made.key, made.bytes))
        })?,
        Layout::Static => caught(command, program, &of, || {
            let view = StaticPlanView::read(plan)?;
            let places = code::places(policy, view.edges(), &plans, Global)?;
            let made = code::code_static(&frontend, policy, plan, &places, Global)?;
            Ok((made.key, made.bytes))
        })?,
    };
    if code_record_key(&template) != key {
        command.tell(program, format_args!("{KEY_NOT_THE_READERS}: given {key:?}, made {:?}", code_record_key(&template)));
        return Err(refused(KEY_NOT_THE_READERS));
    }
    Ok(Aligned::copy_from(&record::write_opaque(&key, &bytes)))
}

/// The walker's reader of the front end's plans (`blaze_program::walker::Reader`): edges read
/// off a plan payload of either layout, and code keys made by [`code::code_key`] or
/// [`code::static_code_key`], with no database.
#[derive(Clone, Copy)]
pub struct Reader<'p> {
    pub policy: &'p Policy,
    /// The toolchain a span table's key is made under ([`Reader::spans`]).
    pub tool: ToolId,
}

/// The span anchors a plan record's head names as its own, each with the key of the body
/// they are of (the owner's item subject, which `Front::spans` takes), sorted by anchor, each
/// once; empty for a static's plan and for a plan whose template holds its positions literally
/// (no anchors of its own: its code record names them, which is the linker's to read). With no
/// database.
///
/// Cost: one read of the plan head; six pairs at most.
pub fn plan_anchors<B: Allocator>(payload: &[u8], alloc: B) -> Result<Vec<(ContentId, StableId), B>, TaskFailed> {
    let mut out = Vec::new_in(alloc);
    if layout_of(payload)? == Layout::Static {
        return Ok(out);
    }
    let view = PlanView::read(payload).map_err(refused)?;
    if let (Some(anchors), Some(owner)) = (view.head.anchors, view.span_owner) {
        out.reserve(anchors.len());
        out.extend(anchors.iter().map(|anchor| (*anchor, StableId(owner))));
        out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        out.dedup_by_key(|it| it.0);
    }
    Ok(out)
}

impl blaze_program::walker::Reader for Reader<'_> {
    /// The plan's own span anchors ([`plan_anchors`]), each with its owner and the key of its
    /// span table record under this reader's toolchain. Cost: one head read, six keys at most.
    fn spans<E: Extend<SpanAt>>(&self, _instance: Key<PlanKind>, plan: &[u8], spans: &mut E) -> Result<(), TaskFailed> {
        let tool = self.tool;
        for (anchor, owner) in plan_anchors(plan, Global)? {
            let key = SpanTableKind::key(&SpanTableInputs { tool, anchor });
            spans.extend(core::iter::once(SpanAt { anchor, key, owner }));
        }
        Ok(())
    }

    /// Each instance a plan's edges name, once, in the plan's order: a function by its plan's
    /// name and ask, a static by its plan's name and [`static_ask`] of its item. An outside
    /// symbol is no instance. A function's `body` is the body its plan reads the MIR of, as the
    /// edge carries it (hir-ty-codegen's `plan_body`), so the driver gives it on the first run;
    /// a static's is `None` (its value's evaluation reads its body: one `NoBody` run more each,
    /// until the static edge says whether it has one: an `extern` static has none).
    ///
    /// Cost: one pass over the edges and one hash insert per edge (the floor: the pass).
    fn edges<E: Extend<WalkEdge>, B: Extend<u8>>(
        &self,
        _instance: Key<PlanKind>,
        payload: &[u8],
        edges: &mut E,
        asks: &mut B,
    ) -> Result<(), TaskFailed> {
        let mut seen = HashSet::new_in(Global);
        for edge in edges_of(payload)? {
            let (name, kind, ask, body): (ContentId, WalkTarget, &[u8], Option<StableId>) = match edge.map_err(refused)? {
                Edge::Function { name, ask, body } => (name, WalkTarget::Call, ask, body.map(StableId)),
                Edge::Static { name, item, .. } => {
                    if seen.insert(name) {
                        let ask = static_ask(&item);
                        let ask_len = u32::try_from(ask.len()).map_err(|_| refused(ASK_TOO_LONG))?;
                        edges.extend(core::iter::once(WalkEdge {
                            instance: Key::from_id(name),
                            kind: WalkTarget::Static,
                            body: None,
                            ask_len,
                        }));
                        asks.extend(ask.iter().copied());
                    }
                    continue;
                }
                Edge::Extern(_) | Edge::Weak(_) => continue,
            };
            if seen.insert(name) {
                let ask_len = u32::try_from(ask.len()).map_err(|_| refused(ASK_TOO_LONG))?;
                edges.extend(core::iter::once(WalkEdge { instance: Key::from_id(name), kind, body, ask_len }));
                asks.extend(ask.iter().copied());
            }
        }
        Ok(())
    }

    /// The driver's key of the code of `plan` at `tier`: its places from its edges, each
    /// function edge's place read off its target's plan (`targets`, one per instance in
    /// [`Self::edges`]'s order), then [`code::code_key`] or [`code::static_code_key`]. Only
    /// the dev tier is made ([`NO_RELEASE_TIER`]).
    ///
    /// Cost: one pass over the edges, one sort of the function targets by name, and one
    /// binary search per edge (the floor: the pass and one read of each target's head).
    fn code_key(
        &self,
        _instance: Key<PlanKind>,
        tier: Tier,
        plan: &[u8],
        targets: &[&[u8]],
    ) -> Result<Key<CodeKind>, TaskFailed> {
        match tier {
            Tier::Edit => {}
            Tier::Release => return Err(refused(NO_RELEASE_TIER)),
        }
        let mut plans: Vec<(ContentId, &[u8]), Global> = Vec::with_capacity_in(targets.len(), Global);
        let mut seen = HashSet::new_in(Global);
        let mut at = 0usize;
        for edge in edges_of(plan)? {
            match edge.map_err(refused)? {
                Edge::Function { name, .. } => {
                    if seen.insert(name) {
                        let target = targets.get(at).ok_or(refused(TARGETS_DIFFER))?;
                        plans.push((name, *target));
                        at += 1;
                    }
                }
                // A static's place reads nothing of its plan (`code::places`).
                Edge::Static { name, .. } => {
                    if seen.insert(name) {
                        at += 1;
                    }
                }
                Edge::Extern(_) | Edge::Weak(_) => {}
            }
        }
        if at != targets.len() {
            return Err(refused(TARGETS_DIFFER));
        }
        plans.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let template = match layout_of(plan)? {
            Layout::Function => {
                let view = PlanView::read(plan).map_err(refused)?;
                let places = code::places(self.policy, view.edges(), &plans, Global).map_err(|it| rule_of(&it))?;
                code::code_key::<Global>(self.policy, &view.head, &places).map_err(|it| rule_of(&it))?.1
            }
            Layout::Static => {
                let view = StaticPlanView::read(plan).map_err(refused)?;
                let places = code::places(self.policy, view.edges(), &plans, Global).map_err(|it| rule_of(&it))?;
                code::static_code_key::<Global>(self.policy, &view.head, &places).map_err(|it| rule_of(&it))?
            }
        };
        Ok(code_record_key(&template))
    }
}

/// A program's roots as the walker takes them ([`ProgramRoots::roots`]): each root's plan name
/// its ask's place in `asks`, and the body its plan reads.
pub struct ProgramRoots<A: Allocator> {
    instances: Vec<(Key<PlanKind>, u32, u32, Option<StableId>), A>,
    asks: Vec<u8, A>,
}

impl<A: Allocator> ProgramRoots<A> {
    /// The roots, borrowing the asks, for `ProgramWalker::add_program`.
    pub fn roots<B: Allocator>(&self, alloc: B) -> Vec<Root<'_>, B> {
        let mut out = Vec::with_capacity_in(self.instances.len(), alloc);
        out.extend(self.instances.iter().map(|&(instance, at, len, body)| Root {
            instance,
            ask: &self.asks[at as usize..(at + len) as usize],
            body,
        }));
        out
    }
}

/// The roots of program `program` (an executable unit): the front end's roots of its crate
/// (`HirFrontend::roots` with `RootKind::Executable`: the C `main`, the allocator shim, and
/// every symbol a crate of it keeps by name), each function asked for by the program crate's
/// origin, and its kept statics, each by its static plan's ask. Over a snapshot of the
/// program's closure from `universe`. A root with no portable ask is refused
/// ([`ROOT_NOT_PORTABLE`]). `body` is each function root's plan body (`PlanAsk::body`), and
/// `None` for a static, as in [`Reader::edges`].
///
/// Cost against the floor (one plan name per root): one snapshot open (O(closure crates)),
/// the front end's roots (one pass over each linked crate's kept symbols), and one name and ask
/// per root. Not a record yet: the roots are listed again every request (their reads are not
/// kept), against a floor of one lookup when nothing they read moved.
pub fn roots<A: Allocator + Clone + Send + Sync>(
    command: &Command<'_, A>,
    program: u32,
    universe: &Universe<'_>,
    policy: &Policy,
    settings: FrontSettings,
) -> Result<ProgramRoots<A>, TaskFailed> {
    let snapshot = snapshot::open(command, program, universe, Task::Body)?;
    let frontend = front_of(&snapshot, policy, settings);
    let listed = frontend.roots(snapshot.own, RootKind::Executable).map_err(|why| {
        command.tell(program, format_args!("{NO_ROOTS}: {why:?}"));
        refused(NO_ROOTS)
    })?;
    let origin = frontend.origin(&snapshot.db, snapshot.own);
    let alloc = command.alloc.clone();
    let mut out = ProgramRoots {
        instances: Vec::with_capacity_in(listed.roots.len() + listed.statics.len(), alloc.clone()),
        asks: Vec::new_in(alloc),
    };
    let mut push = |name: ContentId, ask: &[u8], body: Option<ContentId>| -> Result<(), TaskFailed> {
        let at = u32::try_from(out.asks.len()).map_err(|_| refused(ASK_TOO_LONG))?;
        let len = u32::try_from(ask.len()).map_err(|_| refused(ASK_TOO_LONG))?;
        out.asks.extend_from_slice(ask);
        out.instances.push((Key::from_id(name), at, len, body.map(StableId)));
        Ok(())
    };
    for root in listed.roots.iter() {
        let asked = frontend.plan_ask(root, &origin, Global).map_err(|why| {
            command.tell(program, format_args!("{ROOT_NOT_PORTABLE}: {why:?}"));
            refused(ROOT_NOT_PORTABLE)
        })?;
        push(asked.name, &asked.ask, asked.body)?;
    }
    for value in listed.statics.iter() {
        let asked = frontend.static_plan_ask(value, Global).map_err(|why| {
            command.tell(program, format_args!("{ROOT_NOT_PORTABLE}: {why:?}"));
            refused(ROOT_NOT_PORTABLE)
        })?;
        push(asked.name, &asked.ask, asked.body)?;
    }
    Ok(out)
}

/// The id of the name the program prints for the file at absolute `path` of unit `unit`
/// ([`printed_name`]'s pieces, whole, through `record::printed_name`); `None` for a file it
/// cannot name. The one function both the key task (`Keyed::names`: `ZERO` for `None`) and the
/// span task ([`span_file_fact`]) name a file by, so the driver's answer of a
/// `FactKind::SpanFile` fact and a span table's read of it cannot disagree.
///
/// Cost: one copy of the name (two pieces joined) and one hash.
pub fn printed_name_id<A: Allocator + Clone>(command: &Command<'_, A>, unit: u32, path: &[u8]) -> Option<ContentId> {
    let (head, rest) = printed_name(command, unit, path)?;
    let mut name = Vec::with_capacity_in(head.len() + rest.len(), Global);
    name.extend_from_slice(head.as_bytes());
    name.extend_from_slice(rest.as_bytes());
    Some(record::printed_name(&name))
}

/// The `FactKind::SpanFile` fact a span table reads of file `file` (an index into the gathered
/// files) of unit `unit`, as the driver answers it: id `record::span_file_id` of the unit's
/// owner and [`printed_name_id`], digest `record::span_file_digest` of the file's image key.
/// `None` for a file not gathered or one the command cannot name ([`SPAN_FILE_UNNAMED`]).
pub fn span_file_fact<A: Allocator + Clone>(command: &Command<'_, A>, unit: u32, file: u32) -> Option<Fact> {
    let gathered = command.text(unit)?.files.get(file as usize)?;
    let name = printed_name_id(command, unit, &gathered.path)?;
    let owner = command.graph.units.get(unit as usize)?.owner;
    Some(Fact { id: record::span_file_id(owner, &name), digest: record::span_file_digest(&gathered.image) })
}

/// The name the program prints for the file at absolute `path` of unit `unit` (`file!()`,
/// a panic's location), in two pieces to be read as one (`head` then `rest`), borrowing the
/// graph and `path`, no allocation:
/// - a std unit's file: `/rustc/<commit>/library` then its path below the library
///   ([`crate::graph::Graph::std_name`]), as rustc's remap prints it;
/// - a workspace member's: its path from the checkout, as rustc prints one cargo ran it on
///   (`head` empty).
///
/// `None` for any other unit's file (a registry crate's absolute path is the machine's, which
/// no record may hold) or a path outside its root ([`SPAN_FILE_UNNAMED`]).
fn printed_name<'g, 'p, A: Allocator + Clone>(command: &'g Command<'_, A>, unit: u32, path: &'p [u8]) -> Option<(&'g str, &'p str)> {
    let path = core::str::from_utf8(path).ok()?;
    if let Some(name) = command.graph.std_name(unit, path) {
        return Some((name.library, name.rest));
    }
    if !command.graph.units.get(unit as usize)?.member {
        return None;
    }
    Some(("", path.strip_prefix(&*command.graph.checkout)?.strip_prefix('/')?))
}

/// `Front::spans` for the command: the span tables of `owner` (a function, const or static of
/// unit `unit`, by its item's subject, which is its item key), over a surface snapshot of the
/// unit (its own crate built from its text, every file with its id: `Task::Surface`) and
/// `universe`, the unit's. Each file a table names is one of the unit's gathered files with a
/// printed name, else refused by name ([`SPAN_FOREIGN_FILE`], [`SPAN_FILE_UNNAMED`]); its read
/// is [`span_file_fact`], the driver's own answer. Both lists sorted, each entry once.
///
/// What a comment added above an item then costs: no plan (a plan reads no text and keys by
/// item), no code (a code key is the template's identity), this task once per owner of the
/// edited file whose read failed, and a link step of no row that rewrites the `Location`
/// records of the tables whose bases moved; the tables above the comment come out byte-equal.
///
/// Cost against the floor (the owner's spans, once): a snapshot open of the unit (O(closure
/// crates) plus its own crate's text), the owner's body lowered from text, and its tables made
/// ([`hir_ty_codegen::v2::owner_span_tables`]) then copied once into `frame`; one name copied
/// per gathered file of the unit. No pool yet: a comment edit above many items opens one
/// snapshot per owner, against a floor of one per unit.
pub fn spans<A: Allocator + Clone + Send + Sync, F: Allocator + Clone>(
    command: &Command<'_, A>,
    unit: u32,
    owner: StableId,
    universe: &Universe<'_>,
    frame: F,
) -> Result<SpanTables<F>, TaskFailed> {
    let owner = ItemKey(owner.0);
    let snapshot = snapshot::open(command, unit, universe, Task::Surface)?;
    let text = command.text(unit).ok_or(refused(snapshot::NOT_GATHERED))?;
    // The printed name of each own file, by file id, sorted: what the front end asks.
    let mut names: Vec<(FileId, Box<str, Global>), Global> = Vec::with_capacity_in(text.files.len(), Global);
    for (file, gathered) in snapshot.own_files.iter().zip(text.files.iter()) {
        if let (Some(file), Some((head, rest))) = (file, printed_name(command, unit, &gathered.path)) {
            let mut name = String::with_capacity(head.len() + rest.len());
            name.push_str(head);
            name.push_str(rest);
            names.push((*file, name.into_boxed_str()));
        }
    }
    names.sort_unstable_by_key(|(file, _)| file.index());
    let hashes = HashSource::Items(ItemHashes::new(Box::new(move |file: FileId| {
        let at = names.binary_search_by_key(&file.index(), |(it, _)| it.index()).ok()?;
        Some(String::from(&*names[at].1))
    })));
    let def = match blaze_items::key::resolve(&snapshot.db, owner) {
        Some(Resolved::Item(ItemId::Function(it))) => DefWithBodyId::FunctionId(it),
        Some(Resolved::Item(ItemId::Const(it))) => DefWithBodyId::ConstId(it),
        Some(Resolved::Item(ItemId::Static(it))) => DefWithBodyId::StaticId(it),
        _ => {
            command.tell(unit, format_args!("{SPAN_NO_OWNER}: {owner:?}"));
            return Err(refused(SPAN_NO_OWNER));
        }
    };
    let made = owner_span_tables(&snapshot.db, &hashes, def).map_err(|why| {
        command.tell(unit, format_args!("{SPAN_FAILED}: {owner:?}: {why}"));
        refused(SPAN_FAILED)
    })?;
    let mut reads = Vec::with_capacity_in(made.files.len(), frame.clone());
    for file in made.files.iter() {
        let at = snapshot.own_files.iter().position(|it| *it == Some(*file)).ok_or_else(|| {
            command.tell(unit, format_args!("{SPAN_FOREIGN_FILE}: {owner:?} names {file:?}"));
            refused(SPAN_FOREIGN_FILE)
        })?;
        let fact = span_file_fact(command, unit, at as u32).ok_or_else(|| {
            command.tell(unit, format_args!("{SPAN_FILE_UNNAMED}: {owner:?}"));
            refused(SPAN_FILE_UNNAMED)
        })?;
        reads.push(fact);
    }
    reads.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    reads.dedup_by_key(|fact| fact.id);
    let mut tables = Vec::with_capacity_in(made.tables.len(), frame.clone());
    for (anchor, bytes) in made.tables.iter() {
        let mut copied = Vec::with_capacity_in(bytes.len(), frame.clone());
        copied.extend_from_slice(bytes);
        tables.push((*anchor, copied));
    }
    Ok(SpanTables { reads, tables })
}

/// What a row is to the link, as the kept table writes it.
const ROW_FUNCTION: u8 = 0;
const ROW_STATIC: u8 = 1;

/// One row of the kept table: its plan name, what it is (`ROW_FUNCTION` with its instance key
/// in `key`, or `ROW_STATIC` with its item in `key` and its instance), the key of its code
/// record, and the symbol it defines.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct KeptRow<'k> {
    name: ContentId,
    kind: u8,
    key: ContentId,
    instance: ContentId,
    code: ContentId,
    symbol: Option<&'k str>,
}

fn take<'b>(bytes: &mut &'b [u8], len: usize) -> Result<&'b [u8], TaskFailed> {
    if bytes.len() < len {
        return Err(refused(NOT_KEPT));
    }
    let (head, rest) = bytes.split_at(len);
    *bytes = rest;
    Ok(head)
}

fn take_id(bytes: &mut &[u8]) -> Result<ContentId, TaskFailed> {
    let mut id = [0u8; 32];
    id.copy_from_slice(take(bytes, 32)?);
    Ok(ContentId(id))
}

fn take_u32(bytes: &mut &[u8]) -> Result<u32, TaskFailed> {
    let mut word = [0u8; 4];
    word.copy_from_slice(take(bytes, 4)?);
    Ok(u32::from_le_bytes(word))
}

/// The kept table and the linker's kept state of `payload` (empty: a first link).
fn kept_of<B: Allocator>(payload: &[u8], alloc: B) -> Result<(Vec<KeptRow<'_>, B>, &[u8]), TaskFailed> {
    let mut rows = Vec::new_in(alloc);
    if payload.is_empty() {
        return Ok((rows, &[]));
    }
    let mut bytes = payload;
    if take(&mut bytes, KEPT_MAGIC.len())? != KEPT_MAGIC {
        return Err(refused(NOT_KEPT));
    }
    let count = take_u32(&mut bytes)? as usize;
    rows.reserve(count.min(bytes.len() / 133));
    for _ in 0..count {
        let name = take_id(&mut bytes)?;
        let kind = take(&mut bytes, 1)?[0];
        let key = take_id(&mut bytes)?;
        let instance = take_id(&mut bytes)?;
        let code = take_id(&mut bytes)?;
        let len = take_u32(&mut bytes)? as usize;
        let symbol = match len {
            0 => None,
            len => Some(core::str::from_utf8(take(&mut bytes, len)?).map_err(|_| refused(NOT_KEPT))?),
        };
        if kind > ROW_STATIC || rows.last().is_some_and(|it: &KeptRow<'_>| it.name >= name) {
            return Err(refused(NOT_KEPT));
        }
        rows.push(KeptRow { name, kind, key, instance, code, symbol });
    }
    Ok((rows, bytes))
}

fn put_row<A: Allocator>(out: &mut Vec<u8, A>, row: &KeptRow<'_>) -> Result<(), TaskFailed> {
    out.extend_from_slice(&row.name.0);
    out.push(row.kind);
    out.extend_from_slice(&row.key.0);
    out.extend_from_slice(&row.instance.0);
    out.extend_from_slice(&row.code.0);
    let symbol = row.symbol.unwrap_or("");
    out.extend_from_slice(&u32::try_from(symbol.len()).map_err(|_| refused(NOT_KEPT))?.to_le_bytes());
    out.extend_from_slice(symbol.as_bytes());
    Ok(())
}

/// `Front::link` for the command: one link step of the image `name` laid out for `to` with
/// `output` (both the caller's; nothing is written there), from `changes` (the rows that came,
/// went or moved, in instance order), `records` (`records[i]` the code record of
/// `changes[i].now`, the driver's `code`), `plans` (every row that came or moved and each of
/// their callees, sorted by instance), `spans` (span record values by anchor, sorted, each
/// once: [`spans`]) and the kept `payload` (empty for a first link). A row that went or moved
/// is removed by the instance key or item the kept table holds for it, and the names it
/// defined that its new plan does not are unbound; a row that came or moved is instantiated at
/// its places ([`code::instantiated_rows`]). Returns the next kept payload and the patch from
/// the old state's image to the new one's ([`patch_of`]); the host applies it to a clone.
///
/// `spans` is the driver's to choose (the shape it needs is in [`spans`]): the tables of every
/// anchor a row that came or moved names and the kept state lacks, and of every anchor whose
/// span record was remade to other bytes. The step may carry no row at all (`changes` empty):
/// a comment added above an item is that, and the linker then rewrites the `Location` records
/// of the moved tables alone. A table equal to the kept one is skipped by the linker (one
/// compare of its bytes).
///
/// Cost against the floor (a patch in proportion to `changes`, plus the linker's read and
/// write of its state): one decode and instantiation per changed row; one merge of the kept
/// table with the changes and a rewrite of the whole table, O(rows) at 133 bytes and a
/// symbol each, against a floor of the changed rows' bytes (the linker's own open and keep
/// are O(state) too, so the table is not what bounds the step); one copy of the linker's kept
/// state into a `Mapping` (`step` takes one; the payload is borrowed), one copy of the new
/// state into the payload; the names copied into `String`s, which blaze-link's `Change` holds;
/// and the patch's bytes copied once ([`patch_of`]).
#[allow(clippy::too_many_arguments)]
pub fn link<A: Allocator + Clone, F: Allocator + Clone>(
    command: &Command<'_, A>,
    program: u32,
    policy: &Policy,
    name: &str,
    to: FileName,
    output: Output,
    changes: &[RowChange],
    records: &[&[u8]],
    plans: &[(Key<PlanKind>, &[u8])],
    spans: &[(ContentId, &[u8])],
    payload: &[u8],
    frame: F,
) -> Result<Linked<F>, TaskFailed> {
    let (old, linker) = kept_of(payload, frame.clone())?;
    let mut fn_plans: Vec<(ContentId, &[u8]), Global> = Vec::with_capacity_in(plans.len(), Global);
    let mut static_plans: Vec<(ContentId, &[u8]), Global> = Vec::new_in(Global);
    for &(instance, bytes) in plans {
        match layout_of(bytes)? {
            Layout::Function => fn_plans.push((instance.id(), bytes)),
            Layout::Static => static_plans.push((instance.id(), bytes)),
        }
    }
    // A function, not a closure: the bytes found outlive the borrow of the list, which a
    // closure's inferred signature cannot say.
    fn find<'b>(plans: &[(ContentId, &'b [u8])], name: &ContentId) -> Option<&'b [u8]> {
        plans.binary_search_by(|(it, _)| it.cmp(name)).ok().map(|at| plans[at].1)
    }

    let mut fn_rows: Vec<(ContentId, &[u8]), Global> = Vec::new_in(Global);
    let mut static_rows: Vec<(ContentId, &[u8]), Global> = Vec::new_in(Global);
    let mut next: Vec<KeptRow<'_>, Global> = Vec::with_capacity_in(old.len() + changes.len(), Global);
    let mut removed: Vec<InstanceKey, Global> = Vec::new_in(Global);
    let mut removed_statics: Vec<(ItemKey, ContentId), Global> = Vec::new_in(Global);
    let mut unnamed: Vec<&str, Global> = Vec::new_in(Global);
    let mut kept_at = 0usize;
    let mut last: Option<ContentId> = None;
    for (at, change) in changes.iter().enumerate() {
        let instance = change.instance.id();
        if last.is_some_and(|it| it >= instance) {
            return Err(refused(CHANGES_UNSORTED));
        }
        last = Some(instance);
        while kept_at < old.len() && old[kept_at].name < instance {
            next.push(old[kept_at]);
            kept_at += 1;
        }
        let was = match old.get(kept_at) {
            Some(row) if row.name == instance => {
                kept_at += 1;
                Some(*row)
            }
            _ => None,
        };
        match (change.was, was) {
            (Some(was_key), Some(row)) if row.code == was_key.id() => {}
            (None, None) => {}
            _ => return Err(refused(KEPT_DIFFERS)),
        }
        let now = match change.now {
            Some(code_key) => {
                let bytes = records.get(at).copied().ok_or(refused(NO_CODE))?;
                let root = record::open::<CodeKind>(bytes, &code_key).map_err(|_| refused(NO_CODE))?;
                let template = root.payload.follow(bytes).map_err(|_| refused(NO_CODE))?;
                let row = if let Some(plan) = find(&fn_plans, &instance) {
                    let view = PlanView::read(plan).map_err(refused)?;
                    let key = code::instance_key::<Global>(policy, &view.head)
                        .map_err(|it| failed(command, program, LINK_TASK, it))?;
                    fn_rows.push((instance, template));
                    KeptRow {
                        name: instance,
                        kind: ROW_FUNCTION,
                        key: key.0,
                        instance: ContentId::default(),
                        code: code_key.id(),
                        symbol: view.head.symbol,
                    }
                } else if let Some(plan) = find(&static_plans, &instance) {
                    let view = StaticPlanView::read(plan).map_err(refused)?;
                    static_rows.push((instance, template));
                    KeptRow {
                        name: instance,
                        kind: ROW_STATIC,
                        key: view.head.item.0,
                        instance: view.head.instance,
                        code: code_key.id(),
                        symbol: view.head.symbol,
                    }
                } else {
                    return Err(failed(command, program, LINK_TASK, CodeError::<Global>::NoPlan { name: instance }));
                };
                Some(row)
            }
            None => None,
        };
        if let Some(row) = was {
            match row.kind {
                ROW_FUNCTION => removed.push(InstanceKey(row.key)),
                _ => removed_statics.push((ItemKey(row.key), row.instance)),
            }
            if let Some(symbol) = row.symbol
                && now.and_then(|it| it.symbol) != Some(symbol)
            {
                unnamed.push(symbol);
            }
        }
        next.extend(now);
    }
    next.extend_from_slice(&old[kept_at..]);

    let rows = code::instantiated_rows(policy, Rows { functions: &fn_rows, statics: &static_rows }, &fn_plans, &static_plans, Global)
        .map_err(|it| failed(command, program, LINK_TASK, it))?;
    let mut table = Vec::new_in(frame.clone());
    table.extend_from_slice(KEPT_MAGIC);
    table.extend_from_slice(&u32::try_from(next.len()).map_err(|_| refused(NOT_KEPT))?.to_le_bytes());
    for row in next.iter() {
        put_row(&mut table, row)?;
    }
    let given = Hasher::new("blaze-command program image rows 1").update(&table).finish();

    let mut change = Change::default();
    change.added.extend(rows.functions.iter());
    change.statics.extend(rows.statics.iter());
    change.removed.extend_from_slice(&removed);
    change.removed_statics.extend_from_slice(&removed_statics);
    change.names.extend(rows.names.iter().map(|(symbol, target)| (String::from(*symbol), target.clone())));
    change.weak_names.extend(rows.weak_names.iter().map(|symbol| String::from(*symbol)));
    change.unnamed.extend(unnamed.iter().map(|symbol| String::from(*symbol)));
    change.debug_names.extend(rows.debug_names.iter().map(|(key, path)| (*key, String::from(*path))));
    change.cold.extend_from_slice(&rows.cold);
    change.spans.extend_from_slice(spans);
    change.given = Some(given);
    let kept = match linker.is_empty() {
        true => None,
        false => Some(Mapping::copy(linker)),
    };
    let stepped = blaze_link::step(name, to, output, kept, change, frame.clone()).map_err(|why| {
        command.tell(program, format_args!("{LINK_FAILED}: {why}"));
        refused(LINK_FAILED)
    })?;
    table.extend_from_slice(&stepped.kept);
    let patch = patch_of(&stepped.patch, frame)?;
    Ok(Linked { payload: table, patch })
}

/// The driver's patch of blaze-link's: what it starts from (an empty file for a whole relink,
/// else a clone of the old image), its length, and each write's bytes copied once into `bytes`
/// by offset.
///
/// Cost against the floor (the bytes that changed, written once): one copy of
/// [`blaze_link::Patch::written`] bytes.
fn patch_of<F: Allocator + Clone>(stepped: &blaze_link::Patch, frame: F) -> Result<Patch<F>, TaskFailed> {
    let base = match stepped.applies_to() {
        PatchBase::Empty => DriverBase::Empty,
        PatchBase::Clone => DriverBase::Clone,
    };
    let mut writes = Vec::with_capacity_in(stepped.writes.len(), frame.clone());
    let written = usize::try_from(stepped.written()).ok().filter(|it| *it <= u32::MAX as usize).ok_or(refused(PATCH_TOO_LONG))?;
    let mut bytes = Vec::with_capacity_in(written, frame);
    let mut len = 0u64;
    for edit in stepped.edits() {
        match edit {
            FileEdit::SetLen(it) => len = it,
            FileEdit::Write { at, bytes: data } => {
                // Both fit: the sum of the writes is at most `u32::MAX`, checked above.
                let from = bytes.len() as u32;
                bytes.extend_from_slice(data);
                writes.push(PatchWrite { at, from, len: data.len() as u32 });
            }
        }
    }
    Ok(Patch { base, len, writes, bytes })
}
