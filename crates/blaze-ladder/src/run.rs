//! The straight line: source files to a status, each stage the shape of the task the request
//! machine will ask for (docs/v2/plan/design/bottom.md section 9): lower the files, check each
//! body, build it to LIR, emit it, place every body behind its slot, call the entry.
//!
//! Every stage's refusal is a [`RunError`] naming the function it is about (rule 19); nothing
//! outside a stage's subset is skipped (rule 3).

use core::ffi::c_void;
use core::ptr::NonNull;
use std::alloc::Global;

use blaze_build::data::{DataAlloc, DataError, DataRecord, StaticImage};
use blaze_build::tls::{self, TlsError, TlsLayout};
use blaze_build::layout::{LayoutCx, LayoutFacts, Layouts, Missing, ReprAttrs, ValidRange};
use blaze_build::vtable::{Shape, Vtable, VtableRef, VtableTarget, Word};
use blaze_consteval::{
    Answer, Codes, ConstError, ConstRecord, ConstWorld, FnKind as ConstFnKind, Limits, Lits, Need, NeedKind, RecordKey, Settle,
    Typed, value_of,
};
use blaze_ids::ContentId;
use blaze_typeck::{Checked, ConstHolds, Fetch, PTys};
use blaze_build::{Abi, Adts, BuildError, Instance, InstanceKey, Intrinsic, StaticItem, Subst, instance_key};
use blaze_emit::{Code, EmitError, Isa, Os, Via};
use blaze_own::Alloc;
use blaze_typeck::{
    Adt, CheckError, DiagCode, ExprId, InferCtx, ItemKey, LangItem, List, Mutbl, Needs, Outcome, PNode, PTy, PatId, PortableTys,
    ResolveError, resolve_instance, select_impl,
};

use crate::front::{At, BlazeAttrs, FnKind, FnLang, Front, HOSTED_START};
use crate::crates::{CratesError, DirShelf, Inputs, Toolchain, UserCrate};
use crate::front2::program::{Front2Program, Front2Refusal};
use crate::seam::FrontEnd;
use edition::PackageEdition;
use crate::refusal::{NOT_YET, Refusal, RefusalRule, SourceFile};
use crate::place::{self, Body, Entry, PlaceError, Target, VtableImage, VtableWord};

/// A function a stage refused: its path, for the words, and where its item is, for the finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub path: Box<str, Global>,
    pub at: At,
}

impl core::fmt::Display for Named {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.path)
    }
}

/// Why a program did not run.
#[derive(Debug)]
pub enum RunError {
    /// The front end refused the program: each refusal as a finding (file, byte range, rule,
    /// words). `shown` is their plain rendering, one line each and [`RUNS_TODAY`] last, until
    /// the CLI's `print.rs` draws `refusals` through blaze-report; then it goes.
    Lower { refusals: std::vec::Vec<Refusal, Global>, shown: Box<str, Global> },
    /// More instances than a slot index holds.
    TooManyInstances,
    /// The checker stopped on a malformed body or a bound.
    Check { function: Named, error: CheckError },
    /// The checker asked for a question the lowered program's world holds back: the world's bug.
    Need { function: Named },
    /// The body has type errors: the first one's code, and how many.
    Types { function: Named, first: DiagCode, count: u32 },
    /// The build refused the function; `range` is where in its item, when the error names an
    /// expression or a pattern of its body.
    Build { function: Named, error: BuildError, range: Option<blaze_typeck::Range> },
    /// The build met a type naming ADT `adt`, which the seam's `Adts` has no row for; `path`
    /// is its name where the front end can give it.
    NoAdt { function: Named, adt: ItemKey, path: Option<Box<str>> },
    Emit { function: Named, error: EmitError },
    /// A function the program calls that is neither a body nor an outside symbol.
    Callee { function: Named, callee: ItemKey },
    /// A call of a trait's fn found no impl to run.
    Resolve { function: Named, callee: ItemKey, error: ResolveError },
    /// A vtable the function takes the address of could not be made.
    Vtable { function: Named, principal: ItemKey, why: VtableFail },
    Place(PlaceError),
    /// The program has no `main` at its crate root.
    NoMain,
    /// A lang item the back half asks for that the front end refused to answer.
    Lang(crate::seam::LangRefusal),
    /// `main`'s signature is not one the skeleton calls.
    MainShape { main: Named, what: &'static str },
    /// The program starts at bstd's hosted entry and the caller gave no host table.
    NoHost { start: Named },
    /// The constant evaluator refused `const` item `constant` (or a constant it reads).
    Const { constant: Named, error: ConstError<'static> },
    /// The statics do not form an image.
    Statics { error: DataError },
    /// The thread-locals do not form a template.
    Tls { error: TlsError },
    /// The new front end refused the program, by what it cannot do yet.
    Front2(Front2Refusal),
    /// The new front end's crates: the toolchain or a pack.
    Crates(CratesError),
    /// The package's edition is none the names library knows.
    Edition { written: Box<str, Global> },
    /// The names library refused the program's crates (its error, as shown).
    Names { shown: Box<str, Global> },
    /// The rule `entry-takes-roots` (D59): `main`'s shape has no entry module, or a finding
    /// inside the appended `__blaze_entry`, reported at `main`'s parameters in the root file
    /// `file` (the user never wrote the appended text).
    Entry { file: u32, range: blaze_surface::Range, why: Box<str> },
}

impl core::fmt::Display for RunError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RunError::Lower { shown, .. } => write!(f, "{shown}"),
            RunError::TooManyInstances => write!(f, "more than 2^32 instances"),
            RunError::Check { function, error } => write!(f, "`{function}`: the checker stopped: {error:?}"),
            RunError::Need { function } => {
                write!(f, "`{function}`: the checker needed a fact the program's world holds back")
            }
            RunError::Types { function, first, count } => {
                write!(f, "`{function}`: {count} type errors, the first {first:?}")
            }
            RunError::Build { function, error, .. } => write!(f, "`{function}`: {error}"),
            RunError::NoAdt { function, adt, path } => match path {
                Some(path) => write!(f, "`{function}`: a type names the ADT `{path}` ({:#x}), which the build's `Adts` has no row for", adt.0),
                None => write!(f, "`{function}`: a type names ADT {:#x}, which the build's `Adts` has no row for", adt.0),
            },
            RunError::Emit { function, error } => write!(f, "`{function}`: {error}"),
            RunError::Callee { function, callee } => {
                write!(f, "`{function}` calls item {:#x}, which is neither a body nor an outside symbol", callee.0)
            }
            RunError::Resolve { function, callee, error } => {
                write!(f, "`{function}` calls trait fn {:#x}: {error}", callee.0)
            }
            RunError::Vtable { function, principal, why } => {
                write!(f, "`{function}` takes a vtable of trait {:#x}: ", principal.0)?;
                match why {
                    VtableFail::Shape(error) => write!(f, "{error}"),
                    VtableFail::Layout(error) => write!(f, "the concrete type's layout: {error}"),
                    VtableFail::Build(error) => write!(f, "{error}"),
                    VtableFail::NoDropGlue => write!(f, "the program has no `drop_in_place` lang item for the drop word"),
                }
            }
            RunError::Place(error) => write!(f, "{error}"),
            RunError::NoMain => write!(f, "the crate root has no `main`"),
            RunError::Lang(l) => write!(f, "the lang item {:?}: {} (rule no-fallback: no build without it)", l.item, l.why),
            RunError::MainShape { main, what } => write!(f, "`{main}`: {what}"),
            RunError::NoHost { start } => {
                write!(f, "`{start}` is bstd's hosted entry, and this run was given no host table to start it with")
            }
            RunError::Const { constant, error } => write!(f, "`{constant}`: {error}"),
            RunError::Statics { error } => write!(f, "the program's statics: {error}"),
            RunError::Tls { error } => write!(f, "the program's thread-locals: {error}"),
            RunError::Front2(refusal) => write!(f, "{refusal}"),
            RunError::Crates(error) => write!(f, "the new front end's crates: {error}"),
            RunError::Edition { written } => write!(f, "the edition `{written}` is none the names library knows"),
            RunError::Names { shown } => write!(f, "the names library: {shown}"),
            RunError::Entry { why, .. } => write!(f, "{why}"),
        }
    }
}

impl core::error::Error for RunError {}

impl RunError {
    /// This error as findings: the front end's refusals as they are; a later stage's as one
    /// refusal at the item of the function it names, under `ladder:program` (the build, its
    /// checker or its backend refused the program); none for a failure of placing or running.
    pub fn refusals(self) -> std::vec::Vec<Refusal, Global> {
        let mut out = std::vec::Vec::new_in(Global);
        // A construct the build does not take yet, at its place: the same finding the front end
        // gives for one it does not take (`front::Why::Unsupported`).
        if let RunError::Build {
            ref function,
            error:
                BuildError::Unsupported { what, .. }
                | BuildError::UnsupportedPat { what, .. }
                | BuildError::UnsupportedType { what, .. },
            range: Some(r),
        } = self
        {
            out.push(Refusal {
                file: Some(function.at.file),
                rule: RefusalRule::Unsupported,
                place: function.at.place,
                start: r.start,
                end: r.end,
                secondary: None,
                construct: Some(what),
                message: std::format!("{what} {NOT_YET}").into_boxed_str(),
            });
            return out;
        }
        let function = match self {
            RunError::Lower { refusals, .. } => return refusals,
            RunError::Place(_) => return out,
            RunError::Entry { file, range, ref why } => {
                out.push(Refusal {
                    file: Some(file),
                    rule: RefusalRule::Program,
                    place: 0,
                    start: range.start,
                    end: range.end,
                    secondary: None,
                    construct: None,
                    message: why.clone(),
                });
                return out;
            }
            ref other => other.function_at(),
        };
        out.push(Refusal {
            file: function.map(|at| at.file),
            rule: RefusalRule::Program,
            place: function.map_or(0, |at| at.place),
            start: 0,
            end: 0,
            secondary: None,
            construct: None,
            message: std::format!("{self}").into_boxed_str(),
        });
        out
    }

    /// Where the function this error is about is written, when it names one.
    fn function_at(&self) -> Option<At> {
        match *self {
            RunError::Lower { .. } | RunError::Place(_) | RunError::Entry { .. } => None,
            RunError::Check { ref function, .. }
            | RunError::Need { ref function }
            | RunError::Types { ref function, .. }
            | RunError::Build { ref function, .. }
            | RunError::NoAdt { ref function, .. }
            | RunError::Emit { ref function, .. }
            | RunError::Callee { ref function, .. }
            | RunError::Resolve { ref function, .. }
            | RunError::Vtable { ref function, .. }
            | RunError::MainShape { main: ref function, .. }
            | RunError::NoHost { start: ref function }
            | RunError::Const { constant: ref function, .. } => Some(function.at),
            RunError::TooManyInstances
            | RunError::NoMain
            | RunError::Lang(_)
            | RunError::Statics { .. }
            | RunError::Tls { .. }
            | RunError::Front2(_)
            | RunError::Crates(_)
            | RunError::Edition { .. }
            | RunError::Names { .. } => None,
        }
    }
}

/// The build's questions answered from the front end: ADTs, literals, lang fns, `Drop` impls,
/// ABIs and intrinsics; and each `const` item's evaluated value, by item.
struct FrontAdts<'a, P>(&'a P, usize, &'a [(ItemKey, DataRecord<Global>)], &'a BackLangs);

/// The lang ADTs the builder asks for ([`blaze_build::Adts::lang_adt`]), asked of the front end
/// once before the build: a lookup the front end refuses stops the build by the item's name
/// ([`RunError::Lang`]), never a quiet `None` at the builder's question.
struct BackLangs {
    asked: [(LangItem, Option<ItemKey>); 2],
}

impl BackLangs {
    const ITEMS: [LangItem; 2] = [LangItem::ManuallyDrop, LangItem::OwnedBox];

    fn of<P: FrontEnd>(program: &P) -> Result<BackLangs, RunError> {
        let mut asked = [(LangItem::ManuallyDrop, None); 2];
        for (slot, item) in asked.iter_mut().zip(Self::ITEMS) {
            *slot = (item, program.lang_item(item).map_err(RunError::Lang)?);
        }
        Ok(BackLangs { asked })
    }
}

/// The constant evaluator's world: the front end's items and the checked bodies, and the value
/// records made so far (each read back by the constants that read it).
struct FrontConsts<'a, P> {
    program: &'a P,
    checked: &'a [Option<Checked<'static>>],
    store: Vec<ConstRecord<Alloc<'static>>, Global>,
}

/// As [`FrontAdts`]'s facts: the front end's ADTs and reprs.
impl<P: FrontEnd> LayoutFacts for FrontConsts<'_, P> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.program.adt(key)
    }

    fn repr(&self, key: ItemKey) -> ReprAttrs {
        self.program.repr(key)
    }

    /// The front end's answer (the seam's `discriminant`).
    fn discriminant(&self, adt: ItemKey, variant: u32) -> Result<Option<i128>, Missing> {
        Ok(self.program.discriminant(adt, variant))
    }

    fn valid_range(&self, _: ItemKey) -> ValidRange {
        ValidRange::default()
    }
}

impl<P: FrontEnd> ConstWorld for FrontConsts<'_, P> {
    fn body(&self, item: ItemKey) -> Fetch<Option<Typed<'_>>> {
        let Some(i) = self.program.fn_index(item) else { return Ok(None) };
        let function = self.program.function(i);
        let Some((body, l)) = function.body else { return Ok(None) };
        // Not checked yet: the evaluator stops with `NeedKind::Body` (`condition_holds`).
        let Some(checked) = self.checked[i].as_ref() else { return Err(blaze_typeck::Missing) };
        let lits = Lits { text: &l.text, exprs: (&l.exprs, &l.expr_ranges), pats: (&l.pats, &l.pat_ranges) };
        let view = body.view(self.program.types());
        Ok(Some(Typed::new(view, checked, function.text, lits, self.program.code_of(i))))
    }

    /// A tuple constructor builds its value and a `const fn` is interpreted; every other fn is
    /// refused by the evaluator as not `const`.
    fn fn_kind(&self, item: ItemKey) -> ConstFnKind {
        let Some(i) = self.program.fn_index(item) else { return ConstFnKind::NotConst };
        let function = self.program.function(i);
        match function.kind {
            FnKind::Ctor => {
                self.program.ctor_of(item).map_or(ConstFnKind::NotConst, |(adt, variant)| ConstFnKind::Ctor { adt, variant })
            }
            _ if function.const_fn => ConstFnKind::Const,
            _ => ConstFnKind::NotConst,
        }
    }

    fn code_id(&self, item: ItemKey) -> ContentId {
        match self.program.fn_index(item) {
            Some(i) => self.program.code_of(i),
            None => blaze_ids::Hasher::new("blaze-ladder item identity 1").part(&item.0.to_le_bytes()).finish(),
        }
    }

    fn record(&self, key: RecordKey) -> Fetch<&ConstRecord<Alloc<'_>>> {
        self.store.iter().find(|r| r.key == key).ok_or(blaze_typeck::Missing)
    }

    fn explicit_discriminant(&self, _: ItemKey, _: u32) -> Option<ItemKey> {
        None
    }
}

/// Every `const` item's value, evaluated once each with what it reads settled first, as the
/// build reads it: the record's bytes in the build's allocator.
fn eval_consts<P: FrontEnd>(program: &P, checked: &[Option<Checked<'static>>]) -> Result<Vec<(ItemKey, DataRecord<Global>), Global>, RunError> {
    let alloc = Alloc::global();
    let mut world = FrontConsts { program, checked, store: Vec::new_in(Global) };
    let mut ground = PortableTys::new_in(alloc);
    let mut layout = Layouts::new_in(alloc);
    let mut codes = Codes::new_in(alloc);
    let mut out = Vec::new_in(Global);
    for i in 0..program.fn_count() {
        let function = program.function(i);
        // A `where const` condition is evaluated at each impl selection's arguments
        // (`condition_holds`), never once on its own.
        if !matches!(function.kind, FnKind::Const | FnKind::Static) || function.body.is_none() || function.condition {
            continue;
        }
        let mut settle = Settle::new(Need { kind: NeedKind::Value, item: function.key, args: List::EMPTY }, 64, alloc);
        let mut last = None;
        while let Some(n) = settle.next() {
            let step = match value_of(&world, &mut layout, &mut codes, n.item, n.args, &mut ground, Limits::DEFAULT, alloc) {
                Ok(Answer::Done(record)) => {
                    last = Some(record.key);
                    if !world.store.iter().any(|r| r.key == record.key) {
                        world.store.push(record);
                    }
                    settle.done();
                    Ok(())
                }
                Ok(Answer::Need(need)) => settle.need(need),
                Err(error) => Err(error),
            };
            step.map_err(|error| RunError::Const { constant: function.named(), error })?;
        }
        let Some(record) = last.and_then(|key| world.store.iter().find(|r| r.key == key)) else {
            return Err(RunError::Build {
                function: function.named(),
                error: BuildError::Malformed { what: "a settled constant left no record" },
                range: None,
            });
        };
        let mut allocs = Vec::with_capacity_in(record.data.allocs.len(), Global);
        for a in record.data.allocs.iter() {
            let mut bytes = Vec::with_capacity_in(a.bytes.len(), Global);
            bytes.extend_from_slice(&a.bytes);
            let mut relocs = Vec::with_capacity_in(a.relocs.len(), Global);
            relocs.extend_from_slice(&a.relocs);
            allocs.push(DataAlloc { bytes, align: a.align, mutable: a.mutable, relocs });
        }
        out.push((function.key, DataRecord { allocs }));
    }
    Ok(out)
}

/// Evaluations a condition's `Settle` loop runs at most, bodies checked included (rule 18).
const CONDITION_STEPS: u32 = 4096;

/// `where const` condition `cond`'s value at ground `args` (of `types`): the condition's body
/// and each body it calls checked on demand, then evaluated in a [`Settle`] loop as
/// [`eval_consts`] does. An evaluation error, a body that does not check, a value that is no
/// `bool`, and a condition read while checking a body for another (`Front::nested_world`) are
/// all `Error`: never true.
pub(crate) fn condition_holds(program: &Front<'_>, cond: ItemKey, args: &[PTy], types: PTys<'_>) -> ConstHolds {
    match settle_condition(program, cond, args, types) {
        Some(true) => ConstHolds::True,
        Some(false) => ConstHolds::False,
        None => ConstHolds::Error,
    }
}

fn settle_condition(program: &Front<'_>, cond: ItemKey, args: &[PTy], types: PTys<'_>) -> Option<bool> {
    let alloc = Alloc::global();
    let mut ground = PortableTys::new_in(alloc);
    let args = ground_list(&mut ground, args, types)?;
    let mut checked = Vec::with_capacity_in(program.fns.len(), Global);
    checked.resize_with(program.fns.len(), || None);
    let mut store = Vec::new_in(Global);
    let mut layout = Layouts::new_in(alloc);
    let mut codes = Codes::new_in(alloc);
    let mut settle = Settle::new(Need { kind: NeedKind::Value, item: cond, args }, 64, alloc);
    let mut last = None;
    let mut steps = 0;
    while let Some(n) = settle.next() {
        steps += 1;
        if steps > CONDITION_STEPS {
            return None;
        }
        let world = FrontConsts { program, checked: &checked, store: core::mem::replace(&mut store, Vec::new_in(Global)) };
        let answer = value_of(&world, &mut layout, &mut codes, n.item, n.args, &mut ground, Limits::DEFAULT, alloc);
        store = world.store;
        match answer.ok()? {
            Answer::Done(record) => {
                last = Some(record.key);
                if !store.iter().any(|r| r.key == record.key) {
                    store.push(record);
                }
                settle.done();
            }
            Answer::Need(need) if need.kind == NeedKind::Body => {
                let j = program.fn_index(need.item)?;
                if checked[j].is_some() {
                    return None;
                }
                checked[j] = Some(check_for_condition(program, j)?);
            }
            Answer::Need(need) => settle.need(need).ok()?,
        }
    }
    let record = last.and_then(|key| store.iter().find(|r| r.key == key))?;
    match record.data.allocs.first().map(|a| a.bytes.as_slice()) {
        Some([0]) => Some(false),
        Some([1]) => Some(true),
        _ => None,
    }
}

/// Fn `j`'s body checked in [`Front::nested_world`]; `None` when it does not check clean.
fn check_for_condition(program: &Front<'_>, j: usize) -> Option<Checked<'static>> {
    let body = program.fns[j].body.as_ref()?;
    let view = body.body.view(program.tys.view());
    let world = program.nested_world(j);
    let mut cx = InferCtx::new_in(Alloc::global());
    match blaze_typeck::check(&view, &world, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(done)) if done.diags.is_empty() => Some(done),
        _ => None,
    }
}

/// `args` of table `types` in `ground`: the table copied node by node (each node's children
/// come before it, as `PortableTys::intern` requires), then the list.
fn ground_list(ground: &mut PortableTys<'_>, args: &[PTy], types: PTys<'_>) -> Option<List> {
    let mut map = Vec::with_capacity_in(types.nodes.len(), Global);
    for &n in types.nodes {
        let node = match n {
            PNode::Tuple(l) => PNode::Tuple(ground_of_list(ground, types, &map, l)?),
            PNode::Ref(m, t) => PNode::Ref(m, *map.get(t.0 as usize)?),
            PNode::Ptr(m, t) => PNode::Ptr(m, *map.get(t.0 as usize)?),
            PNode::Array(t, len) => PNode::Array(*map.get(t.0 as usize)?, *map.get(len.0 as usize)?),
            PNode::Slice(t) => PNode::Slice(*map.get(t.0 as usize)?),
            PNode::Pat(t, l) => PNode::Pat(*map.get(t.0 as usize)?, ground_of_list(ground, types, &map, l)?),
            PNode::Adt(k, l) => PNode::Adt(k, ground_of_list(ground, types, &map, l)?),
            PNode::FnDef(k, l) => PNode::FnDef(k, ground_of_list(ground, types, &map, l)?),
            PNode::FnPtr(l, h) => PNode::FnPtr(ground_of_list(ground, types, &map, l)?, h),
            PNode::Closure { owner, ordinal, args } => PNode::Closure { owner, ordinal, args: ground_of_list(ground, types, &map, args)? },
            PNode::Future(e, o, i) => PNode::Future(e, *map.get(o.0 as usize)?, *map.get(i.0 as usize)?),
            PNode::Alias { trait_, name, args } => PNode::Alias { trait_, name, args: ground_of_list(ground, types, &map, args)? },
            PNode::Opaque(k, l) => PNode::Opaque(k, ground_of_list(ground, types, &map, l)?),
            PNode::Dynamic(l) => PNode::Dynamic(ground_of_list(ground, types, &map, l)?),
            PNode::ExTrait(k, l) => PNode::ExTrait(k, ground_of_list(ground, types, &map, l)?),
            PNode::ExProj { trait_, name, args, term } => PNode::ExProj {
                trait_,
                name,
                args: ground_of_list(ground, types, &map, args)?,
                term: *map.get(term.0 as usize)?,
            },
            leaf @ PNode::ExAuto(_) => leaf,
            PNode::ConstValue { ty, bits } => PNode::ConstValue { ty: *map.get(ty.0 as usize)?, bits },
            leaf @ (PNode::Bool
            | PNode::ConstParam(_)
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::Param(_)
            | PNode::Infer
            | PNode::Error) => leaf,
        };
        map.push(ground.intern(node).ok()?);
    }
    let mut out = Vec::with_capacity_in(args.len(), Global);
    for a in args {
        out.push(*map.get(a.0 as usize)?);
    }
    ground.intern_list(&out).ok()
}

fn ground_of_list(ground: &mut PortableTys<'_>, types: PTys<'_>, map: &[PTy], l: List) -> Option<List> {
    let mut out = Vec::with_capacity_in(l.len as usize, Global);
    for t in types.list(l).ok()? {
        out.push(*map.get(t.0 as usize)?);
    }
    ground.intern_list(&out).ok()
}

/// The layout's facts from the front end. Every answer but `adt` is the default because the
/// front refuses what would change it.
impl<P: FrontEnd> LayoutFacts for FrontAdts<'_, P> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.0.adt(key)
    }

    /// As front.rs read the ADT's `#[repr(..)]`.
    fn repr(&self, key: ItemKey) -> ReprAttrs {
        self.0.repr(key)
    }

    /// The front end's answer (the seam's `discriminant`).
    fn discriminant(&self, adt: ItemKey, variant: u32) -> Result<Option<i128>, Missing> {
        Ok(self.0.discriminant(adt, variant))
    }

    /// The type's own range: the front reads no `rustc_layout_scalar_valid_range_*`.
    fn valid_range(&self, _: ItemKey) -> ValidRange {
        ValidRange::default()
    }
}

impl<P: FrontEnd> Adts for FrontAdts<'_, P> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.0.adt(key)
    }

    fn facts(&self) -> &dyn LayoutFacts {
        self
    }

    fn int_lit(&self, owner: ItemKey, expr: ExprId) -> Option<u128> {
        self.0.int_lit(owner, expr)
    }

    fn int_pat(&self, owner: ItemKey, pat: PatId) -> Option<u128> {
        self.0.int_pat(owner, pat)
    }

    fn byte_str(&self, owner: ItemKey, expr: ExprId) -> Option<&[u8]> {
        self.0.string(owner, expr)
    }

    fn str_lit(&self, owner: ItemKey, expr: ExprId) -> Option<&str> {
        core::str::from_utf8(self.0.string(owner, expr)?).ok()
    }

    fn lang_fn(&self, name: &str) -> Option<ItemKey> {
        self.0.fn_lang(FnLang::named(name)?)
    }

    /// `ty`'s `Drop` impl, selected by the checker's solver against the `drop` lang trait; its
    /// `drop` fn as the trait's, which the instance loop resolves to the impl's. Impls are
    /// found by the trait, never by the module: any body's world sees them all.
    fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Result<Option<ItemKey>, BuildError> {
        let Some((trait_, f)) = self.0.drop_trait() else { return Ok(None) };
        if !self.0.selects(self.1, trait_, ty, ground).map_err(BuildError::Select)? {
            return Ok(None);
        }
        f.map(Some).ok_or(BuildError::Malformed { what: "the `drop` lang trait has no `drop` fn" })
    }

    fn deref_fn(&self, mutbl: Mutbl) -> Option<ItemKey> {
        match mutbl {
            Mutbl::Not => self.0.lang_trait_fn(LangItem::Deref, "deref"),
            Mutbl::Mut => self.0.lang_trait_fn(LangItem::DerefMut, "deref_mut"),
        }
    }

    /// From [`BackLangs`]; an item outside it is one the builder never asks (its doc's list).
    fn lang_adt(&self, item: LangItem) -> Option<ItemKey> {
        self.3.asked.iter().find(|(i, _)| *i == item).and_then(|(_, k)| *k)
    }

    fn object_trait(&self, t: ItemKey) -> Option<blaze_build::vtable::ObjectTrait<'_>> {
        self.0.object_trait(t)
    }

    fn abi(&self, item: ItemKey) -> Option<Abi> {
        let i = self.0.fn_index(item)?;
        (i < self.0.fn_count()).then(|| self.0.function(i).abi)
    }

    fn static_item(&self, item: ItemKey) -> Option<StaticItem> {
        self.0.static_item(item)
    }

    /// A `const` item's value as `eval_consts` made it; none at generic arguments yet.
    fn const_value(&self, item: ItemKey, args: &[PTy]) -> Option<&DataRecord<Global>> {
        if !args.is_empty() {
            return None;
        }
        self.2.iter().find(|(k, _)| *k == item).map(|(_, record)| record)
    }

    fn intrinsic(&self, item: ItemKey) -> Option<Intrinsic> {
        self.0.intrinsic(item)
    }
}

/// What this build of blaze runs today: the closing line of a refusal.
pub const RUNS_TODAY: &str = "this build of blaze runs a package with no dependencies and no standard library \
     (`#![no_core]`), in the subset its ladder covers (crates/blaze-cli/tests/ladder/README.md)";

/// Refusals as plain lines, `path:line:col: words` each (positions become lines only here, at
/// print), then [`RUNS_TODAY`]. The stand-in for the renderer's drawing.
pub fn plain(files: &[SourceFile<'_>], refusals: &[Refusal]) -> std::string::String {
    use core::fmt::Write;
    let mut text = std::string::String::new();
    for r in refusals {
        if let Some(sf) = r.file.and_then(|f| files.get(f as usize)) {
            let before = sf.text.get(..(r.place + r.start) as usize).unwrap_or(sf.text);
            let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
            let col = before.len() - before.rfind('\n').map_or(0, |i| i + 1) + 1;
            let _ = write!(text, "{}:{line}:{col}: ", sf.path);
        }
        let _ = writeln!(text, "{}", r.message);
    }
    let _ = write!(text, "{RUNS_TODAY}");
    text
}

/// How `main` is called, from its signature: `fn main()` or `extern "C" fn main() -> i32`.
fn entry_shape<P: FrontEnd>(program: &P, main: usize) -> Result<Start, RunError> {
    let sig = program.sig(main);
    let row = program.function(main);
    let shape = |what| RunError::MainShape { main: row.named(), what };
    let node = |t: PTy| sig.types.nodes.get(t.0 as usize);
    let returns_i32 = matches!(node(sig.output), Some(PNode::Int(blaze_typeck::IntTy::I32)));
    if row.export == Some(HOSTED_START) {
        let one_pointer = matches!(sig.inputs, [p] if matches!(node(*p), Some(PNode::Ptr(Mutbl::Not, _))));
        return match (row.abi, one_pointer, returns_i32) {
            (Abi::C, true, true) => Ok(Start::Hosted),
            _ => Err(shape("bstd's hosted entry is `extern \"C\" fn(*const HostTable) -> i32`")),
        };
    }
    if !sig.inputs.is_empty() {
        return Err(shape("it takes parameters (argc and argv are not passed yet)"));
    }
    match node(sig.output) {
        Some(PNode::Tuple(list)) if list.len == 0 => Ok(Start::Plain(Entry::Unit)),
        Some(PNode::Int(blaze_typeck::IntTy::I32)) if row.abi == Abi::C => Ok(Start::Plain(Entry::I32)),
        Some(PNode::Int(blaze_typeck::IntTy::I32)) => {
            Err(shape("it returns `i32` with the Rust ABI; write `extern \"C\"` and `#![no_main]`"))
        }
        _ => Err(shape("it returns neither `()` nor `i32`")),
    }
}

/// How the program starts: a plain entry, or bstd's hosted entry, given the host's table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Start {
    Plain(Entry),
    Hosted,
}

/// A program built and ready to place: each reached instance's code, the entry's first.
pub struct Built {
    bodies: Vec<Body, Global>,
    /// Every vtable a body names, once each (`Body::vtables` index it).
    vtables: Vec<VtableImage, Global>,
    start: Start,
    start_fn: Named,
    /// Every static's first bytes, the thread-locals' template and descriptor among them.
    image: Option<StaticImage<Global>>,
    /// With thread-locals: the getter's slot.
    getter: Option<u32>,
}

/// The machine the code is emitted for. Every Apple M-series core implements FEAT_LSE
/// (Armv8.1); the code runs in this process, so its system is this one's.
fn isa() -> Isa {
    Isa {
        lse: true,
        os: match cfg!(target_os = "macos") {
            true => Os::MacOs,
            false => Os::Linux,
        },
    }
}

/// The thread-locals' template static and descriptor static: not items of the program, so
/// keys of their own, fixed, in the image beside the program's statics.
const TLS_TEMPLATE: ItemKey = ItemKey(u128::from_be_bytes(*b"blaze-ladder tls"));
const TLS_DESC: ItemKey = ItemKey(u128::from_be_bytes(*b"blaze-ladder dsc"));

/// The program's statics as one image, and where each thread-local is in a thread's block:
/// each plain static's evaluated record, the thread-locals' template and its descriptor.
fn static_image<P: FrontEnd>(
    program: &P,
    values: &[(ItemKey, DataRecord<Global>)],
) -> Result<(Option<StaticImage<Global>>, Option<TlsLayout<Global>>), RunError> {
    let mut tls = Vec::new_in(Global);
    for (key, record) in values.iter() {
        if program.static_item(*key).is_some_and(|s| s.thread_local) {
            tls.push((*key, record));
        }
    }
    let template = tls::template(&tls, Global).map_err(|error| RunError::Tls { error })?;
    let desc = template.as_ref().map(|(_, layout)| tls::descriptor(TLS_TEMPLATE, layout, Global));
    let mut plain = Vec::new_in(Global);
    for (key, record) in values.iter() {
        if program.static_item(*key).is_some_and(|s| !s.thread_local) {
            plain.push((*key, record));
        }
    }
    if let (Some((record, _)), Some(desc)) = (&template, &desc) {
        plain.push((TLS_TEMPLATE, record));
        plain.push((TLS_DESC, desc));
    }
    if plain.is_empty() {
        return Ok((None, None));
    }
    // The image copies each record's bytes; the layout then moves out of the template.
    let image = StaticImage::new(&plain, Global).map_err(|error| RunError::Statics { error })?;
    drop(plain);
    Ok((Some(image), template.map(|(_, layout)| layout)))
}

/// Runs the program of `files`, whose crate root is `root`, in this process: its status.
/// `host` is the host's table (bstd's `wire::HostTable`, built by bstd's `Hosting`): a program
/// that starts at bstd's hosted entry is called with it, and refused without one; any other
/// program does not read it.
pub fn run(files: &[SourceFile<'_>], root: &str, host: Option<NonNull<c_void>>) -> Result<u8, RunError> {
    run_with(files, root, host, FrontChoice::Old)
}

/// Which front end answers the back half (crate::seam): one explicit input of the runner, the
/// old one by default. The new one never hands a program to the old one: what it cannot do is
/// a [`RunError::Front2`] naming it.
#[derive(Clone, Copy, Debug, Default)]
pub enum FrontChoice<'a> {
    #[default]
    Old,
    New(NewFront<'a>),
}

/// What the new front end needs that the old one does not: the package as the names library
/// takes it.
#[derive(Clone, Copy, Debug)]
pub struct NewFront<'a> {
    /// The package's name: its crate's name.
    pub package: &'a str,
    /// Its `Cargo.toml`'s edition, as written (`2024`, `blaze-2026.11`).
    pub edition: &'a str,
    /// The toolchain's shelf; `None` resolves over no toolchain crate (a `#![no_core]` program).
    pub shelf: Option<&'a str>,
}

/// [`run`] through the front end `choice` names.
pub fn run_with(files: &[SourceFile<'_>], root: &str, host: Option<NonNull<c_void>>, choice: FrontChoice<'_>) -> Result<u8, RunError> {
    let built = match choice {
        FrontChoice::Old => build(files, root)?,
        FrontChoice::New(new) => build_new(files, root, new)?,
    };
    let entry = match (built.start, host) {
        (Start::Plain(entry), _) => entry,
        (Start::Hosted, Some(table)) => Entry::Hosted(table),
        (Start::Hosted, None) => return Err(RunError::NoHost { start: built.start_fn }),
    };
    let image = match &built.image {
        Some(image) => Some((image, place::load_statics(image).map_err(RunError::Place)?)),
        None => None,
    };
    let tls = match (built.getter, image) {
        (Some(getter), Some((image, base))) => {
            let at = image.static_at(TLS_DESC).ok_or(RunError::Place(PlaceError::NoDescriptor))?;
            let desc = base + u64::from(at);
            place::create_tls_key(desc).map_err(RunError::Place)?;
            Some(place::Tls { desc, getter })
        }
        _ => None,
    };
    let placed = place::place(&built.bodies, &built.vtables, image, tls).map_err(RunError::Place)?;
    // The entry's instance was the first reached.
    placed.call(0, entry).map_err(RunError::Place)
}

/// What `blaze run` refuses of the program of `files`, every stage up to placing: empty when
/// it would place and call `main`. blaze-cli's findings path calls this, and [`run`] stops at
/// the same refusals, so the two cannot disagree (rule 30). Each stage stops at its first.
pub fn refusals(files: &[SourceFile<'_>], root: &str) -> Vec<Refusal, Global> {
    findings(files, root).0
}

/// [`refusals`], and the `blaze::` attributes the front end read: the advice about unknown
/// names and the count nothing checks ([`BlazeAttrs::note`]). `blaze check`'s call.
pub fn findings(files: &[SourceFile<'_>], root: &str) -> (Vec<Refusal, Global>, BlazeAttrs) {
    let mut blaze = BlazeAttrs::default();
    let refusals = match build_noting(files, root, &mut blaze) {
        Ok(_) => Vec::new_in(Global),
        Err(error) => error.refusals(),
    };
    (refusals, blaze)
}

/// Every stage of [`run`] before placing: read, check, build and emit.
///
/// Every body is checked once, generically. Instances are then built from `main` forward:
/// slot `n` is the `n`th instance reached, and only reached instances are built (a generic fn
/// once per list of ground arguments, an unreached fn never).
pub fn build(files: &[SourceFile<'_>], root: &str) -> Result<Built, RunError> {
    build_noting(files, root, &mut BlazeAttrs::default())
}

/// [`build`] through the new front end: the program's names resolved by the names library over
/// the toolchain `new` names, then front2's answers to the back half.
pub fn build_new(files: &[SourceFile<'_>], root: &str, new: NewFront<'_>) -> Result<Built, RunError> {
    let tc = match new.shelf {
        Some(shelf) => Toolchain::open(&mut DirShelf::new(shelf.into())).map_err(RunError::Crates)?,
        None => Toolchain::none(),
    };
    let dir = std::format!("/{}/", new.package);
    let mut owned: std::vec::Vec<(std::string::String, std::string::String)> =
        files.iter().map(|f| (std::format!("{dir}{}", f.path), f.text.into())).collect();
    let root_at = std::format!("{dir}{root}");
    let edition = PackageEdition::of_manifest(new.edition).map_err(|_| RunError::Edition { written: new.edition.into() })?;
    // D59's interim: the entry module is appended to the root's text here, before names
    // resolve; the names library reads (and keys) the root from `owned` (crates.rs `Texts`).
    let root_file = files.iter().position(|f| f.path == root).ok_or(RunError::NoMain)?;
    let appended = crate::front2::entry::append(&mut owned[root_file].1, crate::crates::syntax_of(edition), edition.blaze.is_some(), &tc.program_cfg())
        .map_err(|e| RunError::Entry { file: root_file as u32, range: e.range(), why: std::format!("{e}").into_boxed_str() })?;
    let user = UserCrate { name: new.package, dir: &dir, root: &root_at, files: &owned, edition };
    let inputs = Inputs::new(&user, &tc).map_err(RunError::Crates)?;
    let crates = inputs.crates();
    let names = inputs.resolve(&crates).map_err(|e| RunError::Names { shown: std::format!("{e}").into_boxed_str() })?;
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).map_err(|e| RunError::Names { shown: std::format!("the command's frame: {e}").into_boxed_str() })?;
    let mut program = Front2Program::new(&names, &frame, files).map_err(RunError::Front2)?;
    build_with(&mut program).map_err(|error| match (appended, error.function_at()) {
        // A finding inside text the user never wrote is reported at `main`'s parameters.
        (Some(a), Some(at)) if at.file as usize == root_file && at.place >= a.user_len => RunError::Entry {
            file: root_file as u32,
            range: a.params,
            why: std::format!("the entry module cannot call `main` with its roots ({}): {error}", crate::front2::entry::RULE).into_boxed_str(),
        },
        _ => error,
    })
}

/// [`build`], the `blaze::` attributes the front end read put in `blaze`.
fn build_noting(files: &[SourceFile<'_>], root: &str, blaze: &mut BlazeAttrs) -> Result<Built, RunError> {
    let mut program = Front::new(files, root, blaze).map_err(|refusal| {
        let mut refusals = Vec::with_capacity_in(1, Global);
        refusals.push(refusal);
        let shown = plain(files, &refusals).into_boxed_str();
        RunError::Lower { refusals, shown }
    })?;
    build_with(&mut program)
}

/// The back half over any front end (crate::seam): its bodies checked, consts and statics
/// evaluated, instances built from the entry forward, emitted.
pub fn build_with<P: FrontEnd>(program: &mut P) -> Result<Built, RunError> {
    // Only a survey goes on with no entry: a run's `None` is no `main`.
    build_from(program, None)?.ok_or(RunError::NoMain)
}

/// What [`build_survey`] tried: instances built clean, instances tried, and the generic fns no
/// built body called at a ground instance (never built: "no instance"), by name.
pub struct SurveyCounts {
    pub built: usize,
    pub tried: usize,
    pub no_instance: Vec<Box<str>, Global>,
}

/// What the back half builds of a program, never run: EVERY clean fn, not only what the entry
/// reaches: the entry, then each non-generic fn with a checked body as an instance of its own
/// (in key order), then each generic fn at every ground instance a built body calls it at. Each
/// is built as the run builds it, but a refusal (the builder's, a callee's resolution or
/// absence, the emitter's) is filed in `refused` (the fn, the refusal's typed name) and the
/// walk goes on, so one refusal hides nothing behind it but the callees of the refused fn.
pub fn build_survey<P: FrontEnd>(program: &mut P, refused: &mut Vec<(Box<str>, Box<str>), Global>) -> Result<SurveyCounts, RunError> {
    let mut counts = SurveyCounts { built: 0, tried: 0, no_instance: Vec::new_in(Global) };
    build_from(program, Some((refused, &mut counts)))?;
    Ok(counts)
}

/// In a survey, `e` filed by its fn and typed name (a callee's prefixed `callee:`, the
/// emitter's `emit:`) and the walk goes on; in a run, `e` stops it.
fn filed(survey: &mut Option<&mut Vec<(Box<str>, Box<str>), Global>>, e: RunError) -> Result<(), RunError> {
    let Some(out) = survey.as_deref_mut() else { return Err(e) };
    let (function, class) = match &e {
        // The builder's refusal with its own words (a `Malformed`'s `what`), cut to one line.
        RunError::Build { function, error, .. } => {
            let said = format!("{}: {error}", class_of(&format!("{error:?}")));
            (function, Box::from(said.char_indices().nth(200).map_or(&*said, |(i, _)| &said[..i])))
        }
        RunError::NoAdt { function, adt, path } => {
            let shown = path.as_deref().map_or_else(|| format!("{:#x}", adt.0), |p| format!("`{p}`"));
            (function, format!("NoAdt: no row for the ADT {shown}").into_boxed_str())
        }
        RunError::Emit { function, error } => (function, format!("emit: {}", class_of(&format!("{error:?}"))).into_boxed_str()),
        RunError::Callee { function, .. } => (function, Box::from("callee: neither a tabled body nor an outside symbol")),
        RunError::Resolve { function, error, .. } => (function, format!("callee: resolve {}", class_of(&format!("{error:?}"))).into_boxed_str()),
        RunError::Vtable { function, why, .. } => (function, format!("vtable: {}", class_of(&format!("{why:?}"))).into_boxed_str()),
        _ => return Err(e),
    };
    out.push((format!("{function}").into_boxed_str(), class));
    Ok(())
}

/// Why a vtable was not made, named.
#[derive(Debug)]
pub enum VtableFail {
    /// The object trait's shape: no facts for a trait on its supertrait tree, or too large.
    Shape(blaze_build::vtable::VtableError),
    /// The concrete type's size and alignment.
    Layout(blaze_build::layout::LayoutError),
    Build(BuildError),
    /// The drop word needs `drop_in_place::<T>` and the program has none.
    NoDropGlue,
}

/// The program's vtables, each made once: `refs[n]` is what `images[n]` is the table of.
struct Tables {
    refs: Vec<VtableRef, Global>,
    images: Vec<VtableImage, Global>,
}

/// The reached instances: slot `n` is `instances[n]`, of fn `tabled[n]`, told apart by `keys[n]`.
struct Reached {
    instances: Vec<Instance, Global>,
    tabled: Vec<usize, Global>,
    keys: Vec<InstanceKey, Global>,
}

impl Reached {
    /// `callee`'s slot (of fn `c`, key `key`): the one reached before under that key, else a new
    /// one at the end, built in its turn.
    fn slot(&mut self, callee: Instance, c: usize, key: InstanceKey) -> Result<u32, RunError> {
        let slot = match self.keys.iter().position(|&known| known == key) {
            Some(slot) => slot,
            None => {
                self.instances.push(callee);
                self.tabled.push(c);
                self.keys.push(key);
                self.instances.len() - 1
            }
        };
        u32::try_from(slot).map_err(|_| RunError::TooManyInstances)
    }
}

/// A refusal's typed name: its `Debug` form up to its first field.
fn class_of(debug: &str) -> Box<str> {
    Box::from(&debug[..debug.find(['(', ' ', '{']).unwrap_or(debug.len())])
}

/// `None` only in a survey whose program has no entry (its `main` did not check): every clean
/// fn is still built, and no image is made.
fn build_from<P: FrontEnd>(program: &mut P, surveyed: Option<(&mut Vec<(Box<str>, Box<str>), Global>, &mut SurveyCounts)>) -> Result<Option<Built>, RunError> {
    let (mut survey, mut counts) = match surveyed {
        Some((refused, counts)) => (Some(refused), Some(counts)),
        None => (None, None),
    };
    let entry = match (program.entry(), &survey) {
        (Some(main), _) => Some(main),
        (None, Some(_)) => None,
        (None, None) => return Err(RunError::NoMain),
    };
    // A survey builds what the entry reaches whatever its shape (his `main` takes `cx, io`).
    let start = match (&survey, entry) {
        (None, Some(main)) => entry_shape(&*program, main)?,
        _ => Start::Plain(Entry::Unit),
    };
    if let Some(main) = entry
        && program.function(main).body.is_none()
    {
        return Err(RunError::MainShape { main: program.function(main).named(), what: "it has no body" });
    }
    let checked = program.check_all()?;
    let program = &*program;
    let types = program.types();
    let consts = eval_consts(program, &checked)?;
    let langs = BackLangs::of(program)?;
    let (image, tls_layout) = static_image(program, &consts)?;
    let mut ground = PortableTys::new_in(Alloc::global());
    // An item's code identity, until the item layer's records give one (shortcuts 2 and 12): its
    // key and its item's text. Never stale; coarse only in that a signature edit changes it too.
    let key_of = |c: usize, args: List, ground: &PortableTys<'_>| {
        instance_key(program.code_of(c), args, ground.view(), Alloc::global())
            .map_err(|error| RunError::Build { function: program.function(c).named(), error, range: None })
    };
    // Slot `n` is the `n`th instance reached; `keys[n]` its key, by which instances are told apart.
    let mut reached = Reached {
        instances: Vec::with_capacity_in(program.fn_count(), Global),
        tabled: Vec::with_capacity_in(program.fn_count(), Global),
        keys: Vec::with_capacity_in(program.fn_count(), Global),
    };
    let mut tables = Tables { refs: Vec::new_in(Global), images: Vec::new_in(Global) };
    let mut layouts = Layouts::new_in(Alloc::global());
    if let Some(main) = entry {
        reached.instances.push(Instance { item: program.function(main).key, args: List::EMPTY });
        reached.tabled.push(main);
        reached.keys.push(key_of(main, List::EMPTY, &ground)?);
    }
    // A survey seeds every non-generic fn with a checked body, by key (the program's order,
    // never the asking's); a generic one is met at the instances built bodies call it at.
    let generic = |c: usize| {
        let sig = program.sig(c);
        sig.parent_count != 0 || sig.own_count != 0
    };
    if counts.is_some() {
        let mut seeds: Vec<(ItemKey, usize), Global> = Vec::new_in(Global);
        for c in 0..program.fn_count() {
            if Some(c) != entry && program.function(c).body.is_some() && checked[c].is_some() && !generic(c) {
                seeds.push((program.function(c).key, c));
            }
        }
        seeds.sort_unstable_by_key(|s| s.0);
        for (item, c) in seeds {
            reached.instances.push(Instance { item, args: List::EMPTY });
            reached.tabled.push(c);
            reached.keys.push(key_of(c, List::EMPTY, &ground)?);
        }
    }
    let mut bodies = Vec::with_capacity_in(program.fn_count(), Global);
    let mut args = Vec::new_in(Global);
    let mut at = 0;
    while let Some(&instance) = reached.instances.get(at) {
        at += 1;
        // Each instance is pushed with its fn's index (`tabled`), so none is looked up again.
        let i = reached.tabled[at - 1];
        let function = program.function(i);
        let name = || function.named();
        let (Some((body, _)), Some(done)) = (function.body, checked[i].as_ref()) else {
            return Err(RunError::Callee { function: name(), callee: instance.item });
        };
        args.clear();
        let malformed = |_| RunError::Build { function: name(), error: BuildError::Malformed { what: "an instance's arguments past the ground table" }, range: None };
        args.extend_from_slice(ground.view().list(instance.args).map_err(malformed)?);
        let view = body.view(types);
        let adts = FrontAdts(program, i, &consts, &langs);
        let mut subst = Subst { args: &args, ground: &mut ground, scratch: Alloc::global() };
        // `drop_in_place::<T>` is built as `T`'s drop glue, never from its written body.
        let built = if Some(function.key) == program.fn_lang(FnLang::DropInPlace) {
            blaze_build::build_drop_glue(&view, done, &program.sig(i), &mut subst, &adts, Global)
        } else {
            blaze_build::build(&view, done, &program.sig(i), &mut subst, &adts, Global)
        }
        .map_err(|error| {
            let ranges = |all: &[blaze_typeck::Range], i: u32| all.get(i as usize).copied();
            let range = match error {
                BuildError::Unsupported { expr, .. } | BuildError::UnsupportedType { expr, .. } => {
                    ranges(&body.expr_ranges, expr.0)
                }
                BuildError::UnsupportedPat { pat, .. } => ranges(&body.pat_ranges, pat.0),
                BuildError::NoAdt { adt } => return RunError::NoAdt { function: name(), adt, path: program.item_named(adt) },
                _ => None,
            };
            RunError::Build { function: name(), error, range }
        });
        let built = match built {
            Ok(b) => b,
            Err(e) => {
                filed(&mut survey, e)?;
                continue;
            }
        };
        let mut via = Vec::with_capacity_in(built.refs.len(), Global);
        let mut targets = Vec::with_capacity_in(built.refs.len(), Global);
        // In a survey, a callee refused leaves this instance unemitted (its targets short).
        let mut broken = false;
        for &called in built.refs.iter() {
            // A trait's fn runs as the selected impl's.
            let (item, resolved) = match program.resolve(i, called.item, called.args, &mut ground) {
                Ok(r) => r,
                Err(error) => {
                    filed(&mut survey, RunError::Resolve { function: name(), callee: called.item, error })?;
                    broken = true;
                    continue;
                }
            };
            let callee = Instance { item, args: resolved };
            let Some(c) = program.fn_index(callee.item) else {
                filed(&mut survey, RunError::Callee { function: name(), callee: callee.item })?;
                broken = true;
                continue;
            };
            let target = program.function(c);
            if target.body.is_some() {
                let key = key_of(c, callee.args, &ground)?;
                via.push(Via::Slot);
                targets.push(Target::Slot(reached.slot(callee, c, key)?));
            } else if target.kind == FnKind::Foreign {
                via.push(Via::Address);
                targets.push(Target::Symbol(target.symbol.into()));
            } else {
                filed(&mut survey, RunError::Callee { function: name(), callee: callee.item })?;
                broken = true;
            }
        }
        if broken {
            continue;
        }
        // Each vtable the body names, made once for the program (`Shape::of`, `Vtable::build`):
        // the tables of its supertraits off the first chain first, by a stack; each method word
        // the impl's fn, resolved and reached as a call is; the drop word `drop_in_place::<T>`.
        let mut table_of = Vec::with_capacity_in(built.vtables.len(), Global);
        for &want in built.vtables.iter() {
            let made = (|| -> Result<u32, RunError> {
                let fail = |why| RunError::Vtable { function: name(), principal: want.principal, why };
                let full = || fail(VtableFail::Build(BuildError::Malformed { what: "the ground table is full" }));
                let mut stack = Vec::new_in(Global);
                stack.push(want);
                while let Some(&t) = stack.last() {
                    if tables.refs.contains(&t) {
                        stack.pop();
                        continue;
                    }
                    let shape = Shape::of(t.principal, |k| program.object_trait(k), Global).map_err(|e| fail(VtableFail::Shape(e)))?;
                    let above = |s: ItemKey| VtableRef { concrete: t.concrete, principal: s, args: List::EMPTY };
                    let missing = shape.words.iter().find_map(|w| match *w {
                        Word::Super(s) => (!tables.refs.contains(&above(s))).then(|| above(s)),
                        Word::Method { .. } => None,
                    });
                    if let Some(r) = missing {
                        stack.push(r);
                        continue;
                    }
                    let mut cx = LayoutCx { ground: &mut ground, facts: &adts, scratch: Alloc::global() };
                    let id = layouts.of(t.concrete, &mut cx).map_err(|e| fail(VtableFail::Layout(e)))?;
                    let (size, align) = (layouts.get(id).size, layouts.get(id).align_bytes());
                    // Word 0: `drop_in_place::<T>` (a no-op glue when `T` needs none). With no
                    // glue fn AND no `Drop` trait, no type of the program can need drop: word 0
                    // is 0, which the built object drop skips (walk.rs `dyn_glue`).
                    let glue = match program.fn_lang(FnLang::DropInPlace) {
                        Some(item) => Some(Instance { item, args: ground.intern_list(&[t.concrete]).map_err(|_| full())? }),
                        None => match program.lang_item(LangItem::Drop).map_err(RunError::Lang)? {
                            None => None,
                            Some(_) => return Err(fail(VtableFail::NoDropGlue)),
                        },
                    };
                    let glue_item = glue.map(|g| g.item);
                    let mut targs = Vec::new_in(Global);
                    targs.extend_from_slice(ground.view().list(t.args).map_err(|_| full())?);
                    let table = Vtable::build(&shape, t.concrete, &targs, size, align, glue, &mut ground, Global)
                        .map_err(|e| fail(VtableFail::Build(e)))?;
                    let mut words = Vec::with_capacity_in(table.relocs.len(), Global);
                    for &(offset, to) in table.relocs.iter() {
                        let word = match to {
                            VtableTarget::Fn(f) => {
                                // A trait method runs as the selected impl's; the glue is itself.
                                let (item, args) = match Some(f.item) == glue_item {
                                    true => (f.item, f.args),
                                    false => program
                                        .resolve(i, f.item, f.args, &mut ground)
                                        .map_err(|error| RunError::Resolve { function: name(), callee: f.item, error })?,
                                };
                                let c = program.fn_index(item).filter(|&c| program.function(c).body.is_some());
                                let Some(c) = c else {
                                    return Err(RunError::Callee { function: name(), callee: item });
                                };
                                VtableWord::Slot(reached.slot(Instance { item, args }, c, key_of(c, args, &ground)?)?)
                            }
                            VtableTarget::Super(s) => match tables.refs.iter().position(|r| *r == above(s)) {
                                Some(n) => VtableWord::Table(u32::try_from(n).map_err(|_| RunError::TooManyInstances)?),
                                None => return Err(fail(VtableFail::Shape(blaze_build::vtable::VtableError::NoTrait(s)))),
                            },
                        };
                        words.push((offset, word));
                    }
                    tables.refs.push(t);
                    tables.images.push(VtableImage { bytes: table.bytes, words });
                    stack.pop();
                }
                match tables.refs.iter().position(|r| *r == want) {
                    Some(n) => u32::try_from(n).map_err(|_| RunError::TooManyInstances),
                    None => Err(fail(VtableFail::Shape(blaze_build::vtable::VtableError::NoTrait(want.principal)))),
                }
            })();
            match made {
                Ok(n) => table_of.push(n),
                Err(e) => {
                    filed(&mut survey, e)?;
                    broken = true;
                }
            }
        }
        if broken {
            continue;
        }
        let mut code = Code::new_in(Global);
        if let Err(error) = blaze_emit::emit(built.func.view(), &via, isa(), &mut code) {
            filed(&mut survey, RunError::Emit { function: name(), error })?;
            continue;
        }
        let mut tls_at = Vec::with_capacity_in(built.thread_locals.len(), Global);
        for &key in built.thread_locals.iter() {
            let Some(offset) = tls_layout.as_ref().and_then(|l| l.offset(key)) else {
                return Err(RunError::Build {
                    function: name(),
                    error: BuildError::Malformed { what: "a thread-local the program's template does not hold" },
                    range: None,
                });
            };
            tls_at.push(offset);
        }
        bodies.push(Body { code, targets, statics: built.statics, tls: tls_at, vtables: table_of });
    }
    if let Some(counts) = counts.as_deref_mut() {
        counts.built = bodies.len();
        counts.tried = reached.instances.len();
        let mut met: blaze_hash::HashMap<ItemKey, (), Global> = blaze_hash::HashMap::new_in(Global);
        met.extend(reached.instances.iter().map(|i| (i.item, ())));
        for c in 0..program.fn_count() {
            let f = program.function(c);
            if f.body.is_some() && checked[c].is_some() && generic(c) && !met.contains_key(&f.key) {
                counts.no_instance.push(format!("{}", f.named()).into_boxed_str());
            }
        }
        counts.no_instance.sort_unstable();
    }
    // A survey with no entry makes no image: its counts are the answer.
    let Some(main) = entry else { return Ok(None) };
    // The thread-locals' getter, once, in the slot after every instance; it calls its imports
    // directly.
    let getter = match tls_layout {
        None => None,
        Some(_) => {
            let named = || program.function(main).named();
            let func = tls::getter(Global)
                .map_err(|error| RunError::Build { function: named(), error: BuildError::Lir(error), range: None })?;
            let mut via = Vec::with_capacity_in(tls::IMPORTS.len(), Global);
            let mut targets = Vec::with_capacity_in(tls::IMPORTS.len(), Global);
            for name in tls::IMPORTS {
                via.push(Via::Address);
                targets.push(Target::Symbol(name.into()));
            }
            let mut code = Code::new_in(Global);
            blaze_emit::emit(func.view(), &via, isa(), &mut code).map_err(|error| RunError::Emit { function: named(), error })?;
            bodies.push(Body { code, targets, statics: Vec::new_in(Global), tls: Vec::new_in(Global), vtables: Vec::new_in(Global) });
            Some(u32::try_from(bodies.len() - 1).map_err(|_| RunError::TooManyInstances)?)
        }
    };
    Ok(Some(Built { bodies, vtables: tables.images, start, start_fn: program.function(main).named(), image, getter }))
}
