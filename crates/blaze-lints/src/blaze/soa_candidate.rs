//! `soa_candidate` (in `blaze::pedantic_perf`, allowed by default): a struct of the crate held
//! in a `Vec`, slice or array whose loops read or write only a small part of each element. A
//! loop over `Vec<T>` pulls whole elements through the cache, so a loop that needs one `u32` of
//! a 64-byte `T` uses a sixteenth of every line it loads. Storing the collection as columns (a
//! struct of arrays), or moving the hot fields into a struct of their own, makes those loops
//! read only what they need.
//!
//! Jai showed struct-of-arrays as a language feature in its early demos and later took it out
//! of the language; Zig has it as a library type, `std.MultiArrayList`, built on comptime
//! reflection. Rust's derives do the same as Zig's library (`soa-rs`, `soa_derive`), so blaze
//! offers a lint that finds where it pays, not a language feature.
//!
//! A pass is one walk over a collection of `T`, read from each body's MIR ([`soa_passes`]):
//!
//! - a `for` loop whose element is `T`, `&T` or `&mut T` and whose iterable holds `T`s (a `Vec`,
//!   `VecDeque`, slice or array, or std's iterator over one): the element is the `Some` payload
//!   of the loop's `Iterator::next` call;
//! - the closures given to iterator methods (core's `Iterator`, rayon's) along one chain
//!   (`v.iter().filter(..).map(..)`), whose parameter is such an element; a `for` loop over a
//!   chain is the same pass as its closures;
//! - inside a loop or iterator closure, an index into a collection of `T` (`v[i]`, through
//!   `Index::index` or a slice's own index).
//!
//! What a pass touches is every field of the element its places project to: `p.a` read,
//! `p.a += 1` written, through copies and reborrows of the element (`let q = p;`, `&*p`). An
//! element used whole (handed to a function or method, copied into an aggregate, captured,
//! returned) touches all of it. Found: a struct of at least [`MIN_SIZE`] bytes, outside test
//! code and macro output, whose counted passes together touch at most `1 / SHARE` of its bytes.
//! With a profile configured ([`crate::profile`]) the counted passes are those in functions the
//! profile shows hot, and one is enough; without one every pass counts, and it takes
//! [`PASSES_WITHOUT_PROFILE`].
//!
//! Narrowed: only the crate that defines the struct is read (another crate's run can't report
//! on it); a method called on the element counts as using it whole, as its body is not
//! followed; a closure parameter that is a tuple (`zip`, `enumerate`) is not taken as an
//! element; drops are not counted. A body whose MIR can't be built is named in a note: its
//! loops are not counted. No fix: every use of the collection changes.

use std::alloc::Global;

use base_db::Db;
use blaze_hash::HashMap;
use hir_def::{
    AdtId, AttrDefId, DefWithBodyId, FunctionId, ItemContainerId, Lookup as _, StructId, TraitId,
    attrs::AttrFlags,
    expr_store::Body,
    hir::{Expr, ExprId, LoopSource, MatchArm, Pat},
    lang_item::lang_items,
};
use hir_ty::{
    InferBodyId,
    db::HirDatabase as _,
    mir::{
        LocalId, MirBody, MirSpan, Operand, OperandKind, PlaceElem, PlaceTy, ProjectionElem,
        Rvalue, StatementKind, StoredPlace, TerminatorKind, return_slot,
    },
    next_solver::{
        DbInterner, ParamEnv, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt as _, InferCtxt},
    },
};
use rustc_type_ir::inherent::IntoKind as _;
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{
    Lint,
    alloc::written_callee,
    facts::{self, Parents},
    frozen_collection, packable,
};
use crate::{check::Cx, diag, profile, query};

const LINT: Lint = Lint::SoaCandidate;

/// The smallest struct found: below it two elements share a cache line and a column saves
/// little.
const MIN_SIZE: u64 = 32;

/// The passes may need at most `1 / SHARE` of the struct's bytes.
const SHARE: u64 = 4;

/// How many passes a finding needs when no profile says which are hot.
const PASSES_WITHOUT_PROFILE: usize = 2;

/// How many references an element's type is peeled of (`&&T` in a `filter` closure).
const REFS: usize = 4;

/// How many expressions up a use is followed to its loop.
const UP: usize = 64;

/// How many method calls down an iterator chain is followed to the collection.
const CHAIN: usize = 16;

/// How many closure bodies of one body are read.
const BODIES: usize = 64;

/// How many rounds copies of an element are followed (`let q = p;`, `&*p`).
const ROUNDS: usize = 8;

/// How a pass walks the collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Walk {
    For,
    Chain,
    Indexed,
}

impl Walk {
    fn say(self) -> &'static str {
        match self {
            Walk::For => "the `for` loop",
            Walk::Chain => "the iterator chain",
            Walk::Indexed => "the loop indexing it",
        }
    }
}

/// One walk over a collection of a struct, and what it touches of each element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pass {
    pub(crate) def: DefWithBodyId,
    /// Where it is: the `for` expression, the collection an iterator chain starts from, or the
    /// loop or closure that indexes.
    pub(crate) at: ExprId,
    pub(crate) walk: Walk,
    /// The fields it reads or writes, by index, sorted.
    pub(crate) fields: Vec<u32>,
    /// It uses an element whole: all of it.
    pub(crate) whole: bool,
}

/// The passes over collections of the crate's structs, once per crate.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Passes {
    by_struct: HashMap<StructId, Vec<Pass>, Global>,
    /// The bodies whose MIR (or a closure's in them) can't be built, by the structs their
    /// expressions name: their passes are not counted, and a finding says so.
    unread: HashMap<StructId, Vec<DefWithBodyId>, Global>,
}

/// The traits a pass is recognized by.
struct Traits {
    iterator: Option<TraitId>,
    index: Option<TraitId>,
    index_mut: Option<TraitId>,
}

/// Every pass over a collection of a struct of `krate`, in the order of the crate's bodies.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn soa_passes(db: &Db, krate: base_db::Crate) -> Passes {
    let mut out = Passes::default();
    let lang = lang_items(db, krate);
    let traits = Traits { iterator: lang.Iterator, index: lang.Index, index_mut: lang.IndexMut };
    for def in frozen_collection::crate_bodies(db, krate) {
        if is_test(db, def) {
            continue;
        }
        read_body(db, krate, &traits, def, &mut out);
    }
    out
}

fn is_test(db: &Db, def: DefWithBodyId) -> bool {
    let DefWithBodyId::FunctionId(f) = def else { return false };
    AttrFlags::query(db, AttrDefId::FunctionId(f)).contains(AttrFlags::IS_TEST)
}

/// A pass being read: its struct and what it has touched so far.
struct Open {
    strukt: StructId,
    at: ExprId,
    walk: Walk,
    fields: Vec<u32>,
    whole: bool,
}

/// One step of a source's prefix: an exact projection, or any variant's downcast (the
/// `Some` of `Option`).
#[derive(Clone, Copy)]
enum Step {
    Is(PlaceElem),
    AnyDowncast,
}

/// A place that holds an element (or a reference to one) of a pass: `local` projected by
/// `prefix`.
struct Source {
    local: LocalId,
    prefix: Vec<Step>,
    pass: usize,
}

/// What a body's walk reads from.
struct Walker<'a, 'db> {
    db: &'db Db,
    krate: base_db::Crate,
    traits: &'a Traits,
    analysis: &'a hir::BodyAnalysis<'db>,
    parents: &'a Parents,
    infcx: InferCtxt<'db>,
    env: ParamEnv<'db>,
}

fn read_body(
    db: &Db,
    krate: base_db::Crate,
    traits: &Traits,
    def: DefWithBodyId,
    out: &mut Passes,
) {
    let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(def));
    let body = analysis.body();
    if !body.exprs().any(|(_, it)| matches!(it, Expr::Loop { .. } | Expr::Closure { .. })) {
        return;
    }
    let Ok(mir) = db.mir_body(InferBodyId::DefWithBodyId(def)) else {
        unread(db, krate, &analysis, def, out);
        return;
    };
    // alloc: the MIR bodies of one function and its closures, a few
    let mut bodies: Vec<(&MirBody<'_>, Option<ExprId>)> = vec![(mir, None)];
    let mut next = 0;
    let mut missing = false;
    while next < bodies.len() && bodies.len() < BODIES {
        let current: &MirBody<'_> = bodies[next].0;
        for &closure in &current.closures {
            let expr = closure.loc(db).expr;
            match db.mir_body_for_closure(closure) {
                Ok(it) => bodies.push((it, Some(expr))),
                Err(_) => missing = true,
            }
        }
        next += 1;
    }
    if missing {
        unread(db, krate, &analysis, def, out);
    }
    let parents = Parents::of(body);
    let walker = Walker {
        db,
        krate,
        traits,
        analysis: &analysis,
        parents: &parents,
        infcx: DbInterner::new_with(db, krate)
            .with_regions()
            .infer_ctxt()
            .build(TypingMode::non_body_analysis()),
        env: db.trait_environment(def.generic_def(db)),
    };
    // alloc: the passes of one body, a few
    let mut passes: Vec<Open> = Vec::new();
    // The `Iterator::next` call of each `for` loop over a collection of a struct, and the
    // closure parameters that are an element.
    // alloc: the seeds of one body, a few
    let mut for_seeds: Vec<(ExprId, usize)> = Vec::new();
    // alloc: the seeds of one body, a few
    let mut closure_seeds: Vec<(ExprId, usize, usize)> = Vec::new();
    for (id, expr) in body.exprs() {
        match expr {
            Expr::Match { expr: into_iter, arms: [arm] } => {
                if let Some(seed) = walker.for_loop(&mut passes, id, into_iter, arm) {
                    for_seeds.push(seed);
                }
            }
            Expr::Closure { args, .. } => {
                let Some(call) = parents.of_expr(id) else { continue };
                let Some(receiver) = walker.iterator_receiver(call, id) else { continue };
                let receiver_ty = analysis.ty(analysis.infer().expr_ty(receiver));
                for (index, &pat) in args.iter().enumerate() {
                    let Some(strukt) = struct_of(db, krate, analysis.infer().pat_ty(pat)) else {
                        continue;
                    };
                    if !holds(db, &receiver_ty, strukt) {
                        continue;
                    }
                    let pass = open(&mut passes, strukt, chain_base(body, receiver), Walk::Chain);
                    closure_seeds.push((id, index, pass));
                }
            }
            _ => {}
        }
    }
    for &(mir, closure) in &bodies {
        // alloc: the element places of one MIR body, a few
        let mut sources: Vec<Source> = Vec::new();
        if let Some(closure) = closure {
            for &(_, index, pass) in closure_seeds.iter().filter(|it| it.0 == closure) {
                // The closure itself is the first parameter.
                if let Some(&local) = mir.param_locals.get(1 + index) {
                    sources.push(Source { local, prefix: Vec::new(), pass }); // alloc: an empty prefix
                }
            }
        }
        for (_, block) in mir.basic_blocks.iter() {
            let Some(terminator) = &block.terminator else { continue };
            let TerminatorKind::Call { func, args, destination, .. } = &terminator.kind else {
                continue;
            };
            let MirSpan::ExprId(span) = terminator.span else { continue };
            if let Some(&(_, pass)) = for_seeds.iter().find(|it| it.0 == span) {
                let mut prefix: Vec<Step> =
                    destination.projection.as_slice().iter().map(|&it| Step::Is(it)).collect(); // alloc: one prefix
                prefix.extend([
                    Step::AnyDowncast,
                    Step::Is(ProjectionElem::Field(hir_ty::mir::FieldIndex(0))),
                ]);
                sources.push(Source { local: destination.local, prefix, pass });
            } else if destination.projection.is_empty()
                && walker.is_index(func)
                && let Some(strukt) =
                    struct_of(db, krate, mir.locals[destination.local].ty.as_ref())
                && args.first().and_then(place_of).is_some_and(|it| {
                    holds(
                        db,
                        &analysis.ty(walker.place_ty(mir, it.local, it.projection.as_slice())),
                        strukt,
                    )
                })
                && let Some(at) = walker.enclosing(span)
            {
                let pass = open(&mut passes, strukt, at, Walk::Indexed);
                sources.push(Source { local: destination.local, prefix: Vec::new(), pass }); // alloc: an empty prefix
            }
        }
        walker.scan(mir, &mut sources, &mut passes);
    }
    for pass in passes {
        if pass.fields.is_empty() && !pass.whole {
            continue;
        }
        let mut fields = pass.fields;
        fields.sort_unstable();
        fields.dedup();
        let found = Pass { def, at: pass.at, walk: pass.walk, fields, whole: pass.whole };
        out.by_struct.entry(pass.strukt).or_default().push(found); // alloc: one struct's passes
    }
}

/// Marks the structs of `krate` that `def`'s expressions name as having a body that can't be
/// read.
fn unread(
    db: &Db,
    krate: base_db::Crate,
    analysis: &hir::BodyAnalysis<'_>,
    def: DefWithBodyId,
    out: &mut Passes,
) {
    let infer = analysis.infer();
    for (id, _) in analysis.body().exprs() {
        analysis.ty(infer.expr_ty(id)).walk(db, |inner| {
            if let Some(hir::Adt::Struct(it)) = inner.as_adt()
                && it.module(db).krate(db).base() == krate
            {
                let defs = out.unread.entry(StructId::from(it)).or_default(); // alloc: one struct's unread bodies
                if !defs.contains(&def) {
                    defs.push(def);
                }
            }
        });
    }
}

/// The pass of `strukt` at `at`, opened if it is new.
fn open(passes: &mut Vec<Open>, strukt: StructId, at: ExprId, walk: Walk) -> usize {
    if let Some(found) = passes.iter().position(|it| it.strukt == strukt && it.at == at) {
        return found;
    }
    passes.push(Open { strukt, at, walk, fields: Vec::new(), whole: false }); // alloc: one pass's fields
    passes.len() - 1
}

/// The struct of `krate` that `ty` is, its references peeled.
fn struct_of(db: &Db, krate: base_db::Crate, ty: Ty<'_>) -> Option<StructId> {
    let mut ty = ty;
    for _ in 0..REFS {
        match ty.kind() {
            TyKind::Ref(_, inner, _) => ty = inner,
            _ => break,
        }
    }
    let TyKind::Adt(adt, _) = ty.kind() else { return None };
    let AdtId::StructId(id) = adt.def_id() else { return None };
    (hir::Struct::from(id).module(db).krate(db).base() == krate).then_some(id)
}

/// Whether `ty`, or a type inside it, holds `strukt`s in a row: a slice or array of it, or a
/// `Vec`, `VecDeque` or std's iterator over one of those.
fn holds(db: &Db, ty: &hir::Type<'_>, strukt: StructId) -> bool {
    let is = |it: &hir::Type<'_>| matches!(it.as_adt(), Some(hir::Adt::Struct(s)) if StructId::from(s) == strukt);
    let mut found = false;
    ty.walk(db, |inner| {
        found |= inner.as_slice().is_some_and(|it| is(&it))
            || inner.as_array_element(db).is_some_and(|it| is(&it))
            || inner.as_adt().is_some_and(|adt| {
                sequence(db, adt) && inner.type_arguments().next().is_some_and(|it| is(&it))
            });
    });
    found
}

/// Whether `adt` is std's row of elements: `Vec`, `VecDeque`, or an iterator of the `slice`,
/// `vec`, `array` or `vec_deque` modules.
fn sequence(db: &Db, adt: hir::Adt) -> bool {
    let module = adt.module(db);
    if !["core", "alloc", "std"].iter().any(|it| facts::crate_is(db, module.krate(db), it)) {
        return false;
    }
    let name = adt.name(db);
    if matches!(name.as_str(), "Vec" | "VecDeque") {
        return true;
    }
    let within = module
        .name(db)
        .is_some_and(|it| matches!(it.as_str(), "slice" | "vec" | "array" | "vec_deque"));
    within && matches!(name.as_str(), "Iter" | "IterMut" | "IntoIter" | "Drain")
}

/// The expression an iterator chain starts from: `v` in `v.iter().filter(..).map(..)`.
fn chain_base(body: &Body, mut at: ExprId) -> ExprId {
    for _ in 0..CHAIN {
        match body.expr(at) {
            Expr::MethodCall { receiver, .. } => at = receiver,
            _ => break,
        }
    }
    at
}

/// The place `operand` reads.
fn place_of(operand: &Operand) -> Option<&StoredPlace> {
    match &operand.kind {
        OperandKind::Copy(it) | OperandKind::Move(it) => Some(it),
        _ => None,
    }
}

/// The place `value` copies, moves or borrows whole: what a copy of an element is made from.
fn aliased(value: &Rvalue) -> Option<&StoredPlace> {
    match value {
        Rvalue::Use(it) => place_of(it),
        Rvalue::Ref(_, it) | Rvalue::CopyForDeref(it) => Some(it),
        _ => None,
    }
}

fn all_derefs(rest: &[PlaceElem]) -> bool {
    rest.iter().all(|it| matches!(it, ProjectionElem::Deref))
}

/// Takes in a use of an element whose projection past the element is `rest`.
fn touch(passes: &mut [Open], pass: usize, rest: &[PlaceElem]) {
    let Some(open) = passes.get_mut(pass) else { return };
    match rest.iter().find(|it| !matches!(it, ProjectionElem::Deref)) {
        Some(ProjectionElem::Field(field)) => {
            if !open.fields.contains(&field.0) {
                open.fields.push(field.0);
            }
        }
        _ => open.whole = true,
    }
}

impl<'db> Walker<'_, 'db> {
    /// The pass of the `for` loop whose desugaring's outer `match` is `id`, and its
    /// `Iterator::next` call, when it walks a collection of a struct.
    fn for_loop(
        &self,
        passes: &mut Vec<Open>,
        id: ExprId,
        into_iter: ExprId,
        arm: &MatchArm,
    ) -> Option<(ExprId, usize)> {
        let body = self.analysis.body();
        let infer = self.analysis.infer();
        let Expr::Loop { body: inner, source: LoopSource::ForLoop, .. } = body.expr(arm.expr)
        else {
            return None;
        };
        let Expr::Call { args: [head], .. } = body.expr(into_iter) else { return None };
        let Expr::Block { tail: Some(tail), .. } = body.expr(inner) else { return None };
        let Expr::Match { expr: next_call, arms: [_, some] } = body.expr(tail) else { return None };
        let Pat::TupleStruct { args: [element], .. } = body.pat(some.pat) else { return None };
        let strukt = struct_of(self.db, self.krate, infer.pat_ty(*element))?;
        if !holds(self.db, &self.analysis.ty(infer.expr_ty(*head)), strukt) {
            return None;
        }
        Some((next_call, open(passes, strukt, self.for_key(id), Walk::For)))
    }

    /// Where the pass of the `for` loop whose outer `match` is `outer` is keyed: the base of
    /// the iterator chain it walks, so its closures are the same pass, or the loop itself.
    fn for_key(&self, outer: ExprId) -> ExprId {
        let body = self.analysis.body();
        let Expr::Match { expr: into_iter, .. } = body.expr(outer) else { return outer };
        match body.expr(into_iter) {
            Expr::Call { args: [head], .. }
                if matches!(body.expr(*head), Expr::MethodCall { .. }) =>
            {
                chain_base(body, *head)
            }
            _ => outer,
        }
    }

    /// The receiver of the call `call` of an iterator method that `closure` is an argument of.
    fn iterator_receiver(&self, call: ExprId, closure: ExprId) -> Option<ExprId> {
        let body = self.analysis.body();
        let (receiver, args) = match body.expr(call) {
            Expr::MethodCall { receiver, args, .. } => (receiver, args),
            Expr::Call { args: [receiver, rest @ ..], .. } => (*receiver, rest),
            _ => return None,
        };
        if !args.contains(&closure) {
            return None;
        }
        let (f, _) = written_callee(self.analysis.infer(), body, call)?;
        let trait_ = self.trait_of(f)?;
        let parallel = || {
            let it = hir::Trait::from(trait_);
            matches!(it.name(self.db).as_str(), "ParallelIterator" | "IndexedParallelIterator")
                && facts::crate_is(self.db, it.module(self.db).krate(self.db), "rayon")
        };
        (Some(trait_) == self.traits.iterator || parallel()).then_some(receiver)
    }

    /// The trait `f` is a method of, or implements a method of.
    fn trait_of(&self, f: FunctionId) -> Option<TraitId> {
        match f.lookup(self.db).container {
            ItemContainerId::TraitId(it) => Some(it),
            ItemContainerId::ImplId(it) => {
                self.db.impl_trait(it).map(|it| it.skip_binder().def_id.0)
            }
            _ => None,
        }
    }

    /// Whether `func` is `Index::index` or `IndexMut::index_mut`.
    fn is_index(&self, func: &Operand) -> bool {
        let Some((f, _)) = func.fn_item() else { return false };
        let Some(trait_) = self.trait_of(f) else { return false };
        [self.traits.index, self.traits.index_mut].contains(&Some(trait_))
            && matches!(hir::Function::from(f).name(self.db).as_str(), "index" | "index_mut")
    }

    /// The pass `expr` indexes in: the innermost loop or iterator closure it runs once per
    /// round of, as its pass is keyed. A closure given to anything else ends the walk up.
    fn enclosing(&self, expr: ExprId) -> Option<ExprId> {
        let body = self.analysis.body();
        for parent in self.parents.up(expr, UP) {
            match body.expr(parent) {
                // The `for` itself is the desugaring's outer `match` above its `loop`.
                Expr::Loop { source: LoopSource::ForLoop, .. } => {
                    return Some(
                        self.parents.of_expr(parent).map_or(parent, |it| self.for_key(it)),
                    );
                }
                Expr::Loop { .. } => return Some(parent),
                Expr::Closure { .. } => {
                    let call = self.parents.of_expr(parent)?;
                    let receiver = self.iterator_receiver(call, parent)?;
                    return Some(chain_base(body, receiver));
                }
                _ => {}
            }
        }
        None
    }

    fn place_ty(&self, mir: &MirBody<'db>, local: LocalId, projection: &[PlaceElem]) -> Ty<'db> {
        PlaceTy::from_ty(mir.locals[local].ty.as_ref())
            .multi_projection_ty(&self.infcx, self.env, projection)
            .ty
    }

    /// The pass whose element `place` is of, and its projection past the element: a source's,
    /// or an index into a slice or array of a struct inside a loop (`s[i].f`).
    fn element_of<'p>(
        &self,
        mir: &MirBody<'db>,
        sources: &[Source],
        passes: &mut Vec<Open>,
        place: &'p StoredPlace,
        span: MirSpan,
    ) -> Option<(usize, &'p [PlaceElem])> {
        let projection = place.projection.as_slice();
        for source in sources.iter().filter(|it| it.local == place.local) {
            let matches = projection.len() >= source.prefix.len()
                && source.prefix.iter().zip(projection).all(|(step, elem)| match step {
                    Step::Is(it) => it == elem,
                    Step::AnyDowncast => matches!(elem, ProjectionElem::Downcast(_)),
                });
            if matches {
                return Some((source.pass, &projection[source.prefix.len()..]));
            }
        }
        let MirSpan::ExprId(expr) = span else { return None };
        for (at, elem) in projection.iter().enumerate() {
            if !matches!(elem, ProjectionElem::Index(_) | ProjectionElem::ConstantIndex { .. }) {
                continue;
            }
            let Some(strukt) =
                struct_of(self.db, self.krate, self.place_ty(mir, place.local, &projection[..=at]))
            else {
                continue;
            };
            let key = self.enclosing(expr)?;
            return Some((open(passes, strukt, key, Walk::Indexed), &projection[at + 1..]));
        }
        None
    }

    /// Follows the copies of each element, then takes in every use of one.
    fn scan(&self, mir: &MirBody<'db>, sources: &mut Vec<Source>, passes: &mut Vec<Open>) {
        // Copies and reborrows of an element into a local: `let q = p;`, `&*p`, a binding of
        // the loop's `Some(p)`.
        for _ in 0..ROUNDS {
            let mut grew = false;
            for (_, block) in mir.basic_blocks.iter() {
                for statement in &block.statements {
                    let StatementKind::Assign(to, value) = &statement.kind else { continue };
                    if !to.projection.is_empty()
                        || to.local == return_slot()
                        || sources.iter().any(|it| it.local == to.local)
                    {
                        continue;
                    }
                    let Some(from) = aliased(value) else { continue };
                    let Some((pass, rest)) =
                        self.element_of(mir, sources, passes, from, statement.span)
                    else {
                        continue;
                    };
                    if all_derefs(rest) {
                        sources.push(Source { local: to.local, prefix: Vec::new(), pass }); // alloc: an empty prefix
                        grew = true;
                    }
                }
            }
            if !grew {
                break;
            }
        }
        let sources: &[Source] = sources;
        let is_source = |place: &StoredPlace| {
            place.projection.is_empty() && sources.iter().any(|it| it.local == place.local)
        };
        // Takes in a place an element's field may be read or written through, where it is
        // visited: a visitor's place lives only for its call.
        let mut take = |place: &StoredPlace, span: MirSpan| {
            if let Some((pass, rest)) = self.element_of(mir, sources, passes, place, span) {
                touch(passes, pass, rest);
            }
        };
        for (_, block) in mir.basic_blocks.iter() {
            for statement in &block.statements {
                let span = statement.span;
                match &statement.kind {
                    StatementKind::Assign(to, value) => {
                        // The copy that makes a source is no use of the element.
                        if is_source(to) && aliased(value).is_some() {
                            continue;
                        }
                        if !is_source(to) {
                            take(to, span);
                        }
                        value.for_each_operand(&mut |it| {
                            if let Some(place) = place_of(it) {
                                take(place, span);
                            }
                        });
                        if let Rvalue::Ref(_, it)
                        | Rvalue::Len(it)
                        | Rvalue::Discriminant(it)
                        | Rvalue::CopyForDeref(it) = value
                        {
                            take(it, span);
                        }
                    }
                    StatementKind::InlineAsm(_) => {
                        MirBody::visit_statement_places(&statement.kind, &mut |it| take(it, span));
                    }
                    // A pattern's fake read, a scope's storage and a drop are not reads of the
                    // element's fields.
                    StatementKind::FakeRead(_)
                    | StatementKind::Deinit(_)
                    | StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Nop => {}
                }
            }
            let Some(terminator) = &block.terminator else { continue };
            let span = terminator.span;
            match &terminator.kind {
                TerminatorKind::Call { destination, .. } if is_source(destination) => {
                    terminator.kind.for_each_operand(&mut |it| {
                        if let Some(place) = place_of(it) {
                            take(place, span);
                        }
                    });
                }
                TerminatorKind::Drop { .. } => {}
                kind => MirBody::visit_terminator_places(kind, &mut |it| take(it, span)),
            }
        }
    }
}

/// How a message names the body of `def`.
fn body_name(db: &Db, def: DefWithBodyId) -> String {
    match hir::DefWithBody::from(def) {
        hir::DefWithBody::Function(f) => format!("`fn {}`", f.name(db).as_str()),
        _ => "a constant's initializer".to_owned(), // alloc: a message's part
    }
}

fn function_of(def: DefWithBodyId) -> Option<FunctionId> {
    match def {
        DefWithBodyId::FunctionId(it) => Some(it),
        _ => None,
    }
}

/// Where the pass is written: its file and range; `None` for one a macro wrote.
fn place_of_pass(db: &Db, pass: &Pass) -> Option<(span::FileId, syntax::TextRange)> {
    let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(pass.def));
    let file = analysis.tree().1.file_id()?.file_id(db);
    let offset = Body::anchor_offset(db, pass.def)?;
    Some((file, analysis.expr_range(pass.at)? + offset))
}

/// `name` and `name`s, in backticks: "`a`", "`a` and `b`", "`a`, `b` and `c`".
fn listed(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("`{one}`"),
        [init @ .., last] => {
            let init: Vec<String> = init.iter().map(|it| format!("`{it}`")).collect(); // alloc: a message's part
            format!("{} and `{last}`", init.join(", "))
        }
    }
}

fn bytes(n: u64) -> String {
    match n {
        1 => "1 byte".to_owned(), // alloc: a message's part
        _ => format!("{n} bytes"),
    }
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Struct<'t>) {
    if !cx.on_blaze(LINT)
        || cx.expansion().is_some()
        || !cx.is_written(item.syntax())
        || cx.is_in_test(item.syntax())
    {
        return;
    }
    let Some(hir::Adt::Struct(def)) = cx.adt_def(ast::Adt::Struct(item)) else { return };
    let db = cx.db();
    let ty = def.ty(db);
    let Ok(layout) = ty.layout(db) else { return };
    let size = layout.size();
    if size < MIN_SIZE {
        return;
    }
    let krate = cx.krate();
    let found = soa_passes(db, krate.base());
    let Some(passes) = found.by_struct.get(&StructId::from(def)) else { return };
    let loaded = cx.conf().profile.as_ref();
    // clone: the profile's settings and a refcount, once per struct linted with a profile
    let hits = loaded
        .map(|it| query::profile_hits(db, krate.base(), query::ProfileKey::new(db, it.clone())));
    let hit_of = |pass: &Pass| -> Option<profile::Hit> { hits?.of(function_of(pass.def)?) };
    // alloc: the passes a finding counts, a few
    let counted: Vec<(&Pass, Option<profile::Hit>)> = passes
        .iter()
        .map(|it| (it, hit_of(it)))
        .filter(|(_, hit)| hits.is_none() || hit.is_some_and(|it| it.hot))
        .collect();
    let least = if hits.is_some() { 1 } else { PASSES_WITHOUT_PROFILE };
    if counted.len() < least || counted.iter().any(|(it, _)| it.whole) {
        return;
    }
    let fields = ty.fields(db);
    let field = |index: u32| fields.iter().find(|(it, _)| it.index() == index as usize);
    let size_of =
        |index: u32| field(index).and_then(|(_, it)| it.layout(db).ok()).map(|it| it.size());
    let name_of = |index: u32| {
        field(index).map_or_else(|| index.to_string(), |(it, _)| it.name(db).as_str().to_owned())
    };
    // alloc: the fields the counted passes touch, a few
    let mut touched: Vec<u32> =
        counted.iter().flat_map(|(it, _)| it.fields.iter().copied()).collect();
    touched.sort_unstable();
    touched.dedup();
    let mut needed = 0;
    for &index in &touched {
        let Some(it) = size_of(index) else { return };
        needed += it;
    }
    if needed == 0 || needed * SHARE > size {
        return;
    }
    let Some(name_node) = item.name().map(|it| it.syntax()) else { return };
    let Some(range) = cx.range(name_node) else { return };
    let name = def.name(db);
    let names: Vec<String> = touched.iter().map(|&it| name_of(it)).collect(); // alloc: a message's part
    let hot = if hits.is_some() { " the profile shows hot" } else { "" };
    let message = format!(
        "`{}` takes {}, and the {} loops over collections of it{hot} touch only {}: {} of each element",
        name.as_str(),
        bytes(size),
        counted.len(),
        listed(&names),
        bytes(needed),
    );
    // alloc: the notes of one finding, a few
    let mut notes: Vec<(Option<diag::Span>, String)> = Vec::new();
    for &(pass, hit) in &counted {
        let place = place_of_pass(db, pass);
        let at = match place {
            Some((file, range)) => match db.line_column(file, range.start()) {
                Ok((line, _)) => {
                    format!(" ({}:{})", packable::shown_path(db, krate, file), line + 1)
                }
                Err(()) => format!(" ({})", packable::shown_path(db, krate, file)),
            },
            None => String::new(), // alloc: a message's part
        };
        let pass_names: Vec<String> = pass.fields.iter().map(|&it| name_of(it)).collect(); // alloc: a message's part
        let pass_bytes: u64 = pass.fields.iter().filter_map(|&it| size_of(it)).sum();
        let hot = match (hit, loaded) {
            (Some(hit), Some(loaded)) => {
                format!(", {} of profile `{}`'s counts", hit.percent(), loaded.file_name())
            }
            _ => String::new(), // alloc: a message's part
        };
        let text = format!(
            "{} in {}{at} touches {} ({}){hot}",
            pass.walk.say(),
            body_name(db, pass.def),
            listed(&pass_names),
            bytes(pass_bytes)
        );
        notes.push((place.map(|(file, range)| diag::of_file(file, range)), text));
    }
    let cold = passes.len() - counted.len();
    if let Some(loaded) = loaded
        && cold > 0
    {
        notes.push((
            None,
            format!(
                "{cold} more loops over it are not counted: profile `{}` shows them cold",
                loaded.file_name()
            ),
        ));
    }
    if let Some(defs) = found.unread.get(&StructId::from(def)) {
        let named: Vec<String> = defs.iter().map(|&it| body_name(db, it)).collect(); // alloc: a message's part
        notes.push((
            None,
            format!(
                "the MIR of {} can't be built, so the loops there are not counted",
                named.join(", ")
            ),
        ));
    }
    let span = cx.span(range);
    let diagnostic = cx.blaze(LINT, name_node, span, message);
    for (at, text) in notes {
        match at {
            Some(at) => diagnostic.span_note(at, text),
            None => diagnostic.note(text),
        };
    }
    diagnostic
        .note(format!(
            "each element is {} in memory, so these loops load {}x the bytes they use; stored as columns, they would load only those fields",
            bytes(size),
            size / needed
        ))
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const MINICORE: &str = "//- minicore: sized, copy, clone, option, fn, iterator, iterators, slice, deref, index, builtin_impls, range, add\n";

    const MAIN: &str = "//- /src/lib.rs crate:main\n";

    fn only(main: &str) -> String {
        let fixture = format!("{MINICORE}{MAIN}{}", main.trim_start_matches('\n'));
        lint(&fixture, "-A clippy::all --warn=blaze::soa_candidate")
    }

    /// A 64-byte struct whose two loops each read one `u32` field: 8 of its 64 bytes.
    #[test]
    fn soa_candidate_finds_loops_that_read_a_sliver_of_each_element() {
        let found = only(
            r#"
pub struct Particle { pub a: u32, pub b: u32, pub rest: [u64; 7] }
pub fn sum_a(ps: &[Particle]) -> u32 {
    let mut total = 0;
    for p in ps {
        total += p.a;
    }
    total
}
pub fn max_b(ps: &[Particle]) -> u32 {
    let mut most = 0;
    for i in 0..ps.len() {
        if ps[i].b > most {
            most = ps[i].b;
        }
    }
    most
}
"#,
        );
        assert_eq!(found.matches("`Particle` takes 64 bytes").count(), 1, "{found}");
        assert!(
            found.contains(
                "the 2 loops over collections of it touch only `a` and `b`: 8 bytes of each element"
            ),
            "{found}"
        );
        assert!(
            found.contains("the `for` loop in `fn sum_a` (")
                && found.contains("lib.rs:4) touches `a` (4 bytes)"),
            "{found}"
        );
        assert!(
            found.contains("the loop indexing it in `fn max_b` (")
                && found.contains("lib.rs:11) touches `b` (4 bytes)"),
            "{found}"
        );
    }

    /// The same struct, where the loops read most of each element.
    #[test]
    fn soa_candidate_leaves_loops_that_read_most_of_each_element() {
        let found = only(
            r#"
pub struct Body { pub a: u32, pub b: u32, pub c: [u64; 3], pub d: [u64; 4] }
pub fn first(ps: &[Body]) -> u64 {
    let mut total = 0;
    for p in ps {
        total += p.c[0] + p.d[0];
    }
    total
}
pub fn second(ps: &[Body]) -> u64 {
    let mut total = 0;
    for p in ps {
        total += p.d[1] + p.c[1];
    }
    total
}
"#,
        );
        assert!(!found.contains("blaze::soa_candidate"), "{found}");
    }
}
