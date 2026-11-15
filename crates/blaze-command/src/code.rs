//! The code path's three steps (docs/v2/plan/design/command.md, "Code"), each a pure function
//! over records that the keyed driver's plan and code tasks and the link call:
//!
//! - [`plan`], [`plan_static`]: one function instance's or static's plan, run over a snapshot
//!   of the asking program's closure ([`crate::snapshot`]): its name (the key), its read list
//!   and its record (the payload). The driver files it with `blaze_keyed::record::write_read`
//!   under the name, at the address of what it read, as a body record is.
//! - [`code`], [`code_static`]: a plan's code record. A function's is the dev-tier template
//!   ([`blaze_codegen::blob`]) under the identity `blaze_codegen::kernel` defines
//!   (`driver::dev_template_key`), compiled by the driver's own per-instance entry
//!   (`blaze_codegen::driver::{prepare_one, emit_one, dev_template_of}`), so there is one
//!   compile path. A static's is its template under `driver::static_shape_key`, the key the
//!   driver's build files a static's template under: its value's content, flags, section and
//!   references by kind.
//! - [`link_rows`]: what a program's rows give blaze-link's `Change`: each function its
//!   template instantiated with this build's places, each static likewise, and the statics a
//!   program had and no longer has ([`removed_statics`]).
//!
//! [`places`] is the one rule that turns a plan's edges into the places its code is made and
//! instantiated against, read with no database: a callee by its plan's key (its code, its
//! arguments, its settings from its origin), through its slot (`Target::Function`, as a
//! launched build's code calls every function); a function or static defined weakly by its
//! name.
//!
//! What a plan's read list is (rule 21): every read the describe made, classified by the item
//! store's one classifier (`blaze_item_hash::reads::fact_of_in`, the one a body record's steps
//! go through), asked by the instance's home crate and covering no body: a plan's key holds no
//! text, so nothing is covered by it, and every row is a fact by key (a body's MIR is that
//! body's `StepMir` fact, the record a body task makes; decision 3). Each fact is asked through
//! [`blaze_front::body::reads::Asks`] on the task's universe. An instance with no body (drop
//! glue, a shim, the C `main`) is planned the same way, its reads the facts of the types and
//! items it is made from (hir-ty-codegen's `plan` module doc, "An instance with no body"). A
//! read no fact stands for fails the plan by its name; a fact no table answers fails it by the
//! rule the universe broke.
//!
//! Invariants:
//! - a plan's payload is a function of its name and the answers of its reads: two builders of
//!   one name over one universe write equal bytes (hir-ty-codegen's `plan::tests`);
//! - a code record is a function of its key: the key holds the template code, the settings,
//!   the places by kind and the toolchain, never a callee's key (so an edit of a callee renames
//!   a relocation at link and compiles nothing of its callers: `tests`);
//! - a program's rows are closed: every function and static an edge of a row resolves to is a
//!   row itself, or the link is refused naming it ([`CodeError::Unlinked`]), before any
//!   template is read;
//! - nothing here is native code from Cranelift (rule 10): a native unit is the stencil
//!   backend's, a Cranelift unit only a wasm one; LLVM's is no dev tier and is refused.
//!
//! Floors, and the cost here against them:
//! - plan: one describe of the instance (its MIR monomorphized, its references walked) and one
//!   binary search per read; here also one port of each callee's ask, and one copy of the
//!   record and of the read list into the caller's allocator;
//! - code: the backend's prepare and emit of the one function (or the static's value); here
//!   also one encode of its template (what the store keeps) and one copy into the caller's
//!   allocator;
//! - link rows: one decode and one instantiation per row, one binary search per edge, and one
//!   sort of the rows' keys for the closure check; nothing per program but the output.

#![blaze::sans_io]

use core::alloc::Allocator;

use base_db::FileId;
use blaze_codegen::{
    Arch, Backend, CodegenError, CompiledFunction, Instance, InstanceKey, Static, Target, Unit,
    backend::Prepare as _,
    blob::{self, BlobError},
    driver::{DevCompiled, dev_template_key, dev_template_of, emit_one, prepare_one, static_shape_key},
    plan::{Origin, Policy},
    settings::Spec,
    template::{StaticTemplate, TemplateError},
};
use blaze_front::body::reads::Asks;
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use blaze_keyed::{driver::TaskFailed, front::Universe, record::Fact};
use hir_ty::body_store::{Classified, Reads, capture};
use hir_ty_codegen::v2::{
    Backends, BuildTime, Databases, Edge, HashSource, Head, HirFrontend, ItemHashes, PlanFailed,
    PlanView, StaticHead, StaticPlanView,
};

use crate::snapshot::Snapshot;

/// A snapshot with no item store: its reads are not recorded, so no plan can be filed.
pub const NO_STORE: &str = "a plan task: the snapshot has no item store installed, so the describe's reads are not recorded (`hir_ty::body_store::capture`)";
/// A snapshot whose memo keeps no read lists.
pub const NO_READS: &str = "a plan task: the snapshot's memo keeps no read lists (`blaze_memo::Reads::Kept`)";
/// An instance with no home (an `asm!` function, the allocator shim) whose describe read the
/// database: no crate to ask its facts by.
pub const NO_HOME: &str = "a plan task: the instance has no home crate (an `asm!` function or the allocator shim) and its describe read the database, so no crate's view classifies the reads (`blaze_item_hash::reads::fact_of_in`)";
/// A read the classifier took as covered when asked to cover nothing.
pub const COVERED_BY_NOTHING: &str = "a plan task: the classifier took a read as covered by a body when it was asked to cover none (`blaze_item_hash::reads::fact_of_in` with no cover)";
/// A plan whose describe names no origin.
pub const NO_ORIGIN: &str = "a plan record: its describe holds no origin (a plan always writes one)";
/// A policy that gives the instance no settings.
pub const NO_CHOICE: &str = "a code task: the build's policy gives the plan's origin no settings (`Policy::choose`)";
/// A unit at LLVM: no dev tier.
pub const NOT_DEV: &str = "a code task: the unit is LLVM's, which is no dev tier (dev code is the stencil backend's, or Cranelift's for wasm)";
/// A native unit at Cranelift (rule 10).
pub const NATIVE_CRANELIFT: &str = "a code task: a native unit at Cranelift (rule proc-macros-on-wasm: Cranelift never makes native code)";
/// A dev compile that built a body in.
pub const INLINED: &str = "a code task: the dev compile built another body in, which its template's key does not hold";
/// Places that are not one per reference of the describe.
pub const PLACES_DIFFER: &str = "a code task: the places given are not one per reference of the plan's describe";
/// A static compiled as another than its plan names.
pub const STATIC_MOVED: &str = "a static code task: the value compiled is of another static than its plan names";
/// A static compiled with other flags than its plan's.
pub const STATIC_FLAGS_MOVED: &str = "a static code task: the value compiled has another mutability, thread-locality or section than its plan's describe";

/// Why a step failed. Each names the rule and, through the codegen error, the fact or the
/// plan, the item. Its words are in the caller's allocator.
pub enum CodeError<A: Allocator> {
    /// The task refused, by the rule.
    Task(TaskFailed),
    /// The plan could not be made or read in this database.
    Plan(PlanFailed<A>),
    /// The describe, prepare or emit failed, naming the instance.
    Codegen(CodegenError),
    /// A code record that is not a template.
    Blob(BlobError),
    /// A template that does not instantiate at the places given.
    Template(TemplateError),
    /// An edge names a plan the caller gave none of.
    NoPlan { name: ContentId },
    /// A program's rows that are not closed: the function `callee` (its plan's name and its
    /// path) an edge of the row `by` (a plan's name) resolves to has no row, so the link would
    /// leave its relocation unbound.
    Unlinked { callee: ContentId, path: Box<str, A>, by: ContentId },
    /// As [`Self::Unlinked`], for a static by its item key.
    UnlinkedStatic { item: ItemKey, by: ContentId },
}

impl<A: Allocator> std::fmt::Debug for CodeError<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodeError::Task(failed) => f.debug_tuple("Task").field(failed).finish(),
            CodeError::Plan(failed) => f.debug_tuple("Plan").field(failed).finish(),
            CodeError::Codegen(error) => f.debug_tuple("Codegen").field(error).finish(),
            CodeError::Blob(error) => f.debug_tuple("Blob").field(error).finish(),
            CodeError::Template(error) => f.debug_tuple("Template").field(error).finish(),
            CodeError::NoPlan { name } => f.debug_struct("NoPlan").field("name", name).finish(),
            CodeError::Unlinked { callee, path, by } => (f.debug_struct("Unlinked"))
                .field("callee", callee)
                .field("path", &&**path)
                .field("by", by)
                .finish(),
            CodeError::UnlinkedStatic { item, by } => {
                f.debug_struct("UnlinkedStatic").field("item", item).field("by", by).finish()
            }
        }
    }
}

fn refused<A: Allocator>(what: &'static str) -> CodeError<A> {
    CodeError::Task(TaskFailed::Refused { what })
}

impl<A: Allocator> From<TaskFailed> for CodeError<A> {
    fn from(failed: TaskFailed) -> CodeError<A> {
        CodeError::Task(failed)
    }
}

impl<A: Allocator> From<&'static str> for CodeError<A> {
    fn from(what: &'static str) -> CodeError<A> {
        refused(what)
    }
}

/// What a describe needs of the build beside the database: whether the build links a
/// dependency image, and whether it is of build-time code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrontSettings {
    pub image: bool,
    pub build_time: bool,
}

/// The front end a plan or code task describes and prepares with, over `snapshot`'s database:
/// the fields one describe reads and no other. The database; blaze items' hashes, naming no
/// file (neither a plan's nor a code record's key holds a file's name, rule 25; hir-ty-codegen's
/// plan module doc), so a describe that asks is a bug, refused by the name it asked for; build time; whether the build links an image; and the
/// policy's units and stage (`with_policy`, which also sets the table of what the database read
/// from describe records). Not set, as one describe reads none of them: kept walks (no build
/// is kept between tasks, rule 70), the body checks (a body record is the check), the stop and
/// give-way flags (a task is not cancelled from inside), and the program's dyn facts (a dev
/// unit inlines nothing, so it is described against no world).
///
/// `with_policy` reads every crate's origin once (`Units::of`, one pass over the crates, no
/// program walk); nothing here walks the program.
pub fn front_end<'db, S: Allocator>(
    snapshot: &'db Snapshot<S>,
    policy: &Policy,
    settings: FrontSettings,
) -> HirFrontend<'db> {
    let names = ItemHashes::new(Box::new(|_: FileId| None));
    HirFrontend::new(
        Databases::new(&snapshot.db),
        HashSource::Items(names),
        BuildTime(settings.build_time),
    )
    .with_policy(policy)
    .with_image(settings.image)
}

/// A plan made: what the driver files.
pub struct PlanRecord<A: Allocator> {
    /// What the plan is keyed by: hir-ty-codegen's `plan` module doc, "The name".
    pub name: ContentId,
    /// Every fact the describe read, with its answer, sorted by id, each once.
    pub reads: Vec<Fact, A>,
    /// The plan record's bytes (`PlanView::read` reads them).
    pub payload: Vec<u8, A>,
}

/// The plan of the instance `ask` names, keyed `name` (both an edge of the asking plan, or of
/// the program's root through `HirFrontend::plan_ask`), over `snapshot` with `frontend` made
/// over it ([`front_end`]), its facts answered by `universe`.
///
/// Refused by name: a snapshot with no item store or whose memo keeps no reads; an instance
/// with no home whose describe read the database ([`NO_HOME`]); a name the front end does not
/// give the ask (another stage); a read no fact stands for; a fact the universe cannot answer.
pub fn plan<S: Allocator, A: Allocator + Clone>(
    snapshot: &Snapshot<S>,
    frontend: &HirFrontend<'_>,
    ask: &[u8],
    name: &ContentId,
    universe: Universe<'_>,
    alloc: A,
) -> Result<PlanRecord<A>, CodeError<A>> {
    if snapshot.installed.is_none() {
        return Err(refused(NO_STORE));
    }
    let db = &snapshot.db;
    let home = frontend.plan_home(ask, alloc.clone()).map_err(CodeError::Plan)?;
    let (made, reads) = capture(db, || frontend.plan(ask, name, alloc.clone()));
    let made = made.map_err(CodeError::Plan)?;
    let reads = reads.ok_or(refused(NO_READS))?;
    let facts = read_list(db, home, &reads, universe, alloc)?;
    Ok(PlanRecord { name: made.name, reads: facts, payload: made.bytes })
}

/// The plan of the static `ask` names (`HirFrontend::static_plan_ask`), keyed `name`, as
/// [`plan`] makes a function's: its reads asked by the static's crate, covering no body.
pub fn plan_static<S: Allocator, A: Allocator + Clone>(
    snapshot: &Snapshot<S>,
    frontend: &HirFrontend<'_>,
    ask: &[u8],
    name: &ContentId,
    universe: Universe<'_>,
    alloc: A,
) -> Result<PlanRecord<A>, CodeError<A>> {
    if snapshot.installed.is_none() {
        return Err(refused(NO_STORE));
    }
    let db = &snapshot.db;
    let home = frontend.static_plan_home(ask)?;
    let (made, reads) = capture(db, || frontend.static_plan(ask, name, alloc.clone()));
    let made = made.map_err(CodeError::Plan)?;
    let reads = reads.ok_or(refused(NO_READS))?;
    let facts = read_list(db, Some(home), &reads, universe, alloc)?;
    Ok(PlanRecord { name: made.name, reads: facts, payload: made.bytes })
}

/// The read list of a plan whose describe read `reads`, asked by `home` and covering no body
/// (this module's doc): each read classified, its fact asked of `universe`, sorted and each
/// once, kept in `alloc`.
fn read_list<A: Allocator + Clone>(
    db: &base_db::Db,
    home: Option<base_db::Crate>,
    reads: &Reads,
    universe: Universe<'_>,
    alloc: A,
) -> Result<Vec<Fact, A>, CodeError<A>> {
    let mut asks = Asks::new(universe, alloc.clone());
    let mut rows = reads.iter().peekable();
    let home = match (home, rows.peek()) {
        (Some(home), _) => home,
        (None, None) => {
            return Ok(Vec::new_in(alloc));
        }
        (None, Some(_)) => return Err(refused(NO_HOME)),
    };
    for &read in rows {
        let fact = match blaze_item_hash::reads::fact_of_in(db, home, None, read) {
            Classified::Fact(fact) | Classified::Scan(fact, _) => fact,
            Classified::Covered => return Err(refused(COVERED_BY_NOTHING)),
            Classified::Refused(what) => return Err(refused(what)),
        };
        let id = blaze_front::facts::id_of(&fact).map_err(|error| CodeError::Task(error.failed()))?;
        asks.ask(&id)?;
    }
    Ok(asks.finish())
}

/// A code record: the dev template's store key and bytes, with the function's side table.
pub struct CodeRecord<A: Allocator> {
    /// What the instance is compiled under: its code, arguments and settings.
    pub instance: InstanceKey,
    /// `blaze_codegen::driver::dev_template_key`: the kernel's dev template identity.
    pub key: blaze_cas::Key,
    /// `blob::encode` of the template.
    pub bytes: Vec<u8, A>,
    /// Its debug info and panic locations, under their key (`Kind::Debug`), where the backend
    /// made one, copied into the caller's allocator.
    pub side: Option<(blaze_cas::Key, Vec<u8, A>)>,
}

/// The dev settings of code compiled for `origin` under `policy`: refused where its unit is no
/// dev tier or is native code at Cranelift.
fn dev_spec<A: Allocator>(
    policy: &Policy,
    origin: &Origin,
    overflow_checks: Option<bool>,
) -> Result<Spec, CodeError<A>> {
    let choice = policy.choose(origin, overflow_checks).map_err(|_| refused(NO_CHOICE))?;
    let spec = choice.first;
    match spec.backend {
        Backend::Stencil => {}
        Backend::Cranelift { .. } if policy.toolchain.arch.is_wasm() => {}
        Backend::Cranelift { .. } => return Err(refused(NATIVE_CRANELIFT)),
        Backend::Llvm { .. } => return Err(refused(NOT_DEV)),
    }
    Ok(spec)
}

/// The unit and settings of the plan whose head is `head` under `policy`: its key's
/// instance, refused where the unit is no dev tier or is native code at Cranelift.
pub fn unit_of<A: Allocator>(policy: &Policy, head: &Head<'_>) -> Result<(Unit, Spec), CodeError<A>> {
    let origin = head.origin.ok_or(refused(NO_ORIGIN))?;
    let spec = dev_spec(policy, &origin, head.overflow_checks)?;
    let arch: Arch = policy.toolchain.arch;
    let instance = Instance { code: head.code, args: head.args, settings: spec.settings() };
    Ok((Unit { instance, backend: spec.backend, arch, item: head.item }, spec))
}

/// The code record of the plan `plan` (its record's bytes) at `places` ([`places`] of its
/// edges), with `frontend` made over the code task's snapshot ([`front_end`]): the instance
/// read back from the plan, prepared and emitted by the driver's own per-instance entry, its
/// template keyed by the kernel's dev identity. The describe is the plan's, never made again;
/// preparing reads the instance's MIR and layouts from the snapshot.
pub fn code<A: Allocator + Clone>(
    frontend: &HirFrontend<'_>,
    policy: &Policy,
    plan: &[u8],
    places: &[Target],
    alloc: A,
) -> Result<CodeRecord<A>, CodeError<A>> {
    let view = PlanView::read(plan)?;
    let (unit, spec) = unit_of(policy, &view.head)?;
    let (instance, _, described) = frontend.planned(&view, alloc.clone()).map_err(CodeError::Plan)?;
    if places.len() != described.refs.len() {
        return Err(refused(PLACES_DIFFER));
    }
    let backend = Backends::default();
    let anchors = described.anchors.as_ref();
    let (_, prepared) =
        prepare_one(&backend, frontend, &unit, &spec, &instance, places, anchors.is_some())
            .map_err(CodeError::Codegen)?;
    let emitted = emit_one(&backend, &unit, &spec, prepared, places, &described.name)
        .map_err(CodeError::Codegen)?;
    // A dev unit inlines nothing: code built in would be code its key does not hold.
    if !emitted.function.inlined.is_empty() {
        return Err(refused(INLINED));
    }
    let DevCompiled { template, side } =
        dev_template_of(&unit, emitted, places, anchors).map_err(CodeError::Codegen)?;
    let (key, at) = code_key(policy, &view.head, places)?;
    let bytes = copied(&blob::encode(&template), alloc.clone());
    let side = side.map(|(key, bytes)| (key, copied(&bytes, alloc)));
    Ok(CodeRecord { instance: key, key: at, bytes, side })
}

/// What a plan's code is filed and linked under at `places`, read off its head with no
/// database: its instance key, and its dev template's store key (`dev_template_key` of its
/// template code, settings and places by kind, never a callee's key). [`code`] files under it;
/// a driver looks it up before compiling, so a plan whose key is unchanged compiles nothing.
pub fn code_key<A: Allocator>(
    policy: &Policy,
    head: &Head<'_>,
    places: &[Target],
) -> Result<(InstanceKey, blaze_cas::Key), CodeError<A>> {
    let (unit, spec) = unit_of(policy, head)?;
    let key = unit.instance.key();
    let kept = Instance { code: head.template_code, ..unit.instance };
    Ok((key, dev_template_key(&kept, key, &[], places, &spec.toolchain.id)))
}

/// `bytes` copied into `alloc`.
fn copied<A: Allocator>(bytes: &[u8], alloc: A) -> Vec<u8, A> {
    let mut out = Vec::with_capacity_in(bytes.len(), alloc);
    out.extend_from_slice(bytes);
    out
}

/// A static's code record: its template's store key and bytes.
pub struct StaticCodeRecord<A: Allocator> {
    /// Which static it is: its item and instance, as the link places it.
    pub item: ItemKey,
    pub instance: ContentId,
    /// `blaze_codegen::driver::static_shape_key` of its plan's head at its places: the key the
    /// driver's build files a static's template under.
    pub key: blaze_cas::Key,
    /// `blob::encode_static` of the template.
    pub bytes: Vec<u8, A>,
}

/// The code record of the static plan `plan` at `places` ([`places`] of its edges): its value
/// compiled by the dev backend (`Backend::compile_static`, which reads its value from the
/// snapshot), checked to be the static and flags its plan names, its template keyed by its
/// shape and references by kind.
pub fn code_static<A: Allocator + Clone>(
    frontend: &HirFrontend<'_>,
    policy: &Policy,
    plan: &[u8],
    places: &[Target],
    alloc: A,
) -> Result<StaticCodeRecord<A>, CodeError<A>> {
    let view = StaticPlanView::read(plan)?;
    let head = &view.head;
    let spec = dev_spec(policy, &head.origin, None)?;
    if places.len() != view.len() {
        return Err(refused(PLACES_DIFFER));
    }
    let value = frontend.static_planned(&view)?;
    let backend = Backends::default();
    let made = backend.compile_static(frontend, &spec, &value, places).map_err(CodeError::Codegen)?;
    if (made.item, made.instance) != (head.item, head.instance) {
        return Err(refused(STATIC_MOVED));
    }
    if (made.mutable, made.thread_local, made.section.as_deref())
        != (head.mutable, head.thread_local, head.section)
    {
        return Err(refused(STATIC_FLAGS_MOVED));
    }
    let template = StaticTemplate::of(&made, places).map_err(CodeError::Template)?;
    let key = static_code_key(policy, head, places)?;
    let bytes = copied(&blob::encode_static(&template), alloc);
    Ok(StaticCodeRecord { item: head.item, instance: head.instance, key, bytes })
}

/// What a static plan's code is filed and linked under at `places`, read off its head with no
/// database: `driver::static_shape_key` of its value's shape, its places by kind and its dev
/// settings, never which static it is. [`code_static`] files under it; a driver's walk keys
/// the static's code task by it (`program::Reader`).
pub fn static_code_key<A: Allocator>(
    policy: &Policy,
    head: &StaticHead<'_>,
    places: &[Target],
) -> Result<blaze_cas::Key, CodeError<A>> {
    let spec = dev_spec(policy, &head.origin, None)?;
    Ok(static_shape_key(head.shape(), places, &spec.settings()))
}

/// The key a plan's function is compiled and linked under: its code, arguments and settings.
pub fn instance_key<A: Allocator>(policy: &Policy, head: &Head<'_>) -> Result<InstanceKey, CodeError<A>> {
    let origin = head.origin.ok_or(refused(NO_ORIGIN))?;
    let choice = policy.choose(&origin, head.overflow_checks).map_err(|_| refused(NO_CHOICE))?;
    Ok(Instance { code: head.code, args: head.args, settings: choice.first.settings() }.key())
}

/// The plan named `name` among `plans`, sorted by name, each once.
fn plan_named<'p, A: Allocator>(
    plans: &[(ContentId, &'p [u8])],
    name: &ContentId,
) -> Result<PlanView<'p>, CodeError<A>> {
    let at = plans.binary_search_by(|(it, _)| it.cmp(name)).map_err(|_| CodeError::NoPlan { name: *name })?;
    Ok(PlanView::read(plans[at].1)?)
}

/// The places of a plan's `edges`, in order: a function by its plan's key (found in `plans`,
/// sorted by name, each once) through its slot, or by its symbol where it defines it weakly;
/// a static by item and instance, or by its symbol where it defines it weakly; an outside
/// symbol by name. Read with no database. A function plan's edges are `PlanView::edges`, a
/// static plan's `StaticPlanView::edges`.
pub fn places<'r, A: Allocator + Clone>(
    policy: &Policy,
    edges: hir_ty_codegen::v2::Edges<'r>,
    plans: &[(ContentId, &[u8])],
    alloc: A,
) -> Result<Vec<Target, A>, CodeError<A>> {
    let mut out = Vec::with_capacity_in(edges.size_hint().0, alloc);
    // A place's own name, as `Target` holds it: a `String`, so on the global heap until
    // `Target` takes its names' allocator.
    let owned = |name: &str| Box::<str, std::alloc::Global>::from(name).into_string();
    for edge in edges {
        out.push(match edge? {
            Edge::Function { name, .. } => {
                let callee = plan_named(plans, &name)?;
                match (callee.head.linkage(), callee.head.symbol) {
                    (blaze_codegen::Linkage::Weak, Some(symbol)) => Target::Extern(owned(symbol)),
                    _ => Target::Function(instance_key(policy, &callee.head)?),
                }
            }
            Edge::Static { weak: Some(symbol), .. } => Target::Extern(owned(symbol)),
            Edge::Static { item, instance, weak: None, .. } => Target::Static { item, instance },
            Edge::Extern(name) => Target::Extern(owned(name)),
            Edge::Weak(name) => Target::Weak(owned(name)),
        });
    }
    Ok(out)
}

/// What a program's rows give blaze-link's `Change`: its `added` (each function borrowed from
/// `functions`), `statics` (each borrowed from `statics`), `names`, `weak_names`,
/// `debug_names` and `cold`. Names borrow the plans.
pub struct LinkRows<'p, A: Allocator> {
    /// One per function row, in the rows' order.
    pub functions: Vec<CompiledFunction, A>,
    /// One per static row, in the rows' order.
    pub statics: Vec<Static, A>,
    /// The symbols the rows define, each at its function's slot or its static.
    pub names: Vec<(&'p str, Target), A>,
    /// Of `names`, those defined weakly.
    pub weak_names: Vec<&'p str, A>,
    pub debug_names: Vec<(InstanceKey, &'p str), A>,
    pub cold: Vec<InstanceKey, A>,
}

/// A program's rows: each `(plan name, code record bytes)`, functions and statics apart.
#[derive(Clone, Copy, Debug)]
pub struct Rows<'r> {
    pub functions: &'r [(ContentId, &'r [u8])],
    pub statics: &'r [(ContentId, &'r [u8])],
}

/// What a program's `rows` give the link, each plan found in `plans` (function plans) or
/// `static_plans` (both sorted by name, each once; every callee's plan too): first the rows'
/// closure is checked ([`CodeError::Unlinked`], [`CodeError::UnlinkedStatic`]), then each code
/// record is decoded and instantiated at its edges' places, with the names the link binds.
pub fn link_rows<'p, A: Allocator + Clone>(
    policy: &Policy,
    rows: Rows<'_>,
    plans: &[(ContentId, &'p [u8])],
    static_plans: &[(ContentId, &'p [u8])],
    alloc: A,
) -> Result<LinkRows<'p, A>, CodeError<A>> {
    closed(policy, rows, plans, static_plans, alloc.clone())?;
    instantiated_rows(policy, rows, plans, static_plans, alloc)
}

/// [`link_rows`] without its closure check: what a link step's rows that came or moved give
/// the link (`program::link`), whose callees not among them are already in the kept image.
/// That the image is closed is the walk's: it reaches every edge of every row, so a row's
/// callee is a row of the kept manifest or of this change. Each plan is found in `plans` or
/// `static_plans` as [`link_rows`] finds it (every callee's plan too, for its place).
///
/// Cost: one decode and one instantiation per row, one binary search per edge; nothing per
/// row of the image that did not move.
pub fn instantiated_rows<'p, A: Allocator + Clone>(
    policy: &Policy,
    rows: Rows<'_>,
    plans: &[(ContentId, &'p [u8])],
    static_plans: &[(ContentId, &'p [u8])],
    alloc: A,
) -> Result<LinkRows<'p, A>, CodeError<A>> {
    let mut out = LinkRows {
        functions: Vec::with_capacity_in(rows.functions.len(), alloc.clone()),
        statics: Vec::with_capacity_in(rows.statics.len(), alloc.clone()),
        names: Vec::new_in(alloc.clone()),
        weak_names: Vec::new_in(alloc.clone()),
        debug_names: Vec::with_capacity_in(rows.functions.len(), alloc.clone()),
        cold: Vec::new_in(alloc.clone()),
    };
    for (name, code) in rows.functions {
        let plan = plan_named(plans, name)?;
        let key = instance_key(policy, &plan.head)?;
        let at = places(policy, plan.edges(), plans, alloc.clone())?;
        let template = blob::decode(code).map_err(CodeError::Blob)?;
        let function = template
            .instantiate_at(key, &at, plan.head.anchors.as_ref())
            .map_err(CodeError::Template)?;
        out.functions.push(function);
        if let Some(symbol) = plan.head.symbol {
            out.names.push((symbol, Target::Function(key)));
            if matches!(plan.head.linkage(), blaze_codegen::Linkage::Weak) {
                out.weak_names.push(symbol);
            }
        }
        out.debug_names.push((key, plan.head.name));
        if plan.head.cold() {
            out.cold.push(key);
        }
    }
    for (name, code) in rows.statics {
        let plan = static_plan_named(static_plans, name)?;
        let head = plan.head;
        let at = places(policy, plan.edges(), plans, alloc.clone())?;
        let template = blob::decode_static(code).map_err(CodeError::Blob)?;
        let value = template.instantiate(head.item, head.instance, &at).map_err(CodeError::Template)?;
        out.statics.push(value);
        if let Some(symbol) = head.symbol {
            out.names.push((symbol, Target::Static { item: head.item, instance: head.instance }));
            if matches!(head.linkage, blaze_codegen::Linkage::Weak) {
                out.weak_names.push(symbol);
            }
        }
    }
    Ok(out)
}

/// The static plan named `name` among `plans`, sorted by name, each once.
fn static_plan_named<'p, A: Allocator>(
    plans: &[(ContentId, &'p [u8])],
    name: &ContentId,
) -> Result<StaticPlanView<'p>, CodeError<A>> {
    let at = plans.binary_search_by(|(it, _)| it.cmp(name)).map_err(|_| CodeError::NoPlan { name: *name })?;
    Ok(StaticPlanView::read(plans[at].1)?)
}

/// Whether `rows` are closed: every function an edge of a row resolves to by its slot is a
/// function row, and every static an edge resolves to by item is a static row. A function or
/// static defined weakly is reached by its symbol, which any row or none may define, and is
/// not checked; nor is an outside symbol. Read with no database, before any template.
fn closed<A: Allocator + Clone>(
    policy: &Policy,
    rows: Rows<'_>,
    plans: &[(ContentId, &[u8])],
    static_plans: &[(ContentId, &[u8])],
    alloc: A,
) -> Result<(), CodeError<A>> {
    let mut functions = Vec::with_capacity_in(rows.functions.len(), alloc.clone());
    for (name, _) in rows.functions {
        functions.push(instance_key(policy, &plan_named(plans, name)?.head)?);
    }
    functions.sort_unstable();
    let mut statics = Vec::with_capacity_in(rows.statics.len(), alloc.clone());
    for (name, _) in rows.statics {
        let head = static_plan_named(static_plans, name)?.head;
        statics.push((head.item, head.instance));
    }
    statics.sort_unstable();
    let check = |by: &ContentId, edges: hir_ty_codegen::v2::Edges<'_>| -> Result<(), CodeError<A>> {
        for edge in edges {
            match edge? {
                Edge::Function { name, .. } => {
                    let callee = plan_named(plans, &name)?;
                    if matches!(callee.head.linkage(), blaze_codegen::Linkage::Weak)
                        && callee.head.symbol.is_some()
                    {
                        continue;
                    }
                    let key = instance_key(policy, &callee.head)?;
                    if functions.binary_search(&key).is_err() {
                        let path = blaze_mem::String::from_str_in(callee.head.name, alloc.clone());
                        return Err(CodeError::Unlinked { callee: name, path: path.into_boxed_str(), by: *by });
                    }
                }
                Edge::Static { item, instance, weak: None, .. } => {
                    if statics.binary_search(&(item, instance)).is_err() {
                        return Err(CodeError::UnlinkedStatic { item, by: *by });
                    }
                }
                Edge::Static { weak: Some(_), .. } | Edge::Extern(_) | Edge::Weak(_) => {}
            }
        }
        Ok(())
    };
    for (name, _) in rows.functions {
        check(name, plan_named(plans, name)?.edges())?;
    }
    for (name, _) in rows.statics {
        check(name, static_plan_named(static_plans, name)?.edges())?;
    }
    Ok(())
}

/// The statics a program's link had at `old` and has no longer at `new` (each the static rows'
/// `(item, instance)`, sorted, each once): `Change.removed_statics`, in `old`'s order.
pub fn removed_statics<A: Allocator>(
    old: &[(ItemKey, ContentId)],
    new: &[(ItemKey, ContentId)],
    alloc: A,
) -> Vec<(ItemKey, ContentId), A> {
    let mut out = Vec::new_in(alloc);
    out.extend(old.iter().filter(|it| new.binary_search(it).is_err()).copied());
    out
}

#[cfg(test)]
mod tests;
