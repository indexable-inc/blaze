//! Whether a `for` loop's iterations depend on each other, over the body's MIR.
//!
//! The loop is the natural loop of the block its `loop` jumps back to ([`region`]); an
//! iteration is one trip around it. What the iterations share is what is live where the loop
//! starts, and what pointers reach. Each write in the loop is traced to the memory it changes
//! ([`Root`]), by the definitions of the pointer it goes through, all over the body:
//!
//! - a local of the iteration's own (not live where the loop starts): nothing carries;
//! - the item, when the iterator gives each iteration memory of its own (`iter_mut`,
//!   `chunks_mut`, owned items): nothing carries;
//! - an element at the iteration's own index (`out[i]`, `v[i]` through `IndexMut`, with `i` the
//!   item of a range or the count of `enumerate`), when nothing else in the loop reads that
//!   memory at another index: nothing carries;
//! - a local the loop starts with: a value carried from iteration to iteration, which is
//!   allowed only as a reduction ([`Reduction`]);
//! - anything else (memory a parameter points at, a static, what blaze can't trace): shared,
//!   and the loop is not looked at further.
//!
//! A call writes what its summary says ([`effects::call_summary`]); one with an effect that
//! rules a loop out (I/O, a global, an atomic, what blaze can't see into) rules this one out.
//! What every iteration reads and nothing writes is shared by reference across threads, so its
//! type must be `Sync`, and the item, moved to the thread that runs it, `Send`.

use base_db::Db;
use hir_def::{
    FunctionId, ItemContainerId,
    expr_store::Body,
    hir::{Expr, ExprId, LoopSource, Pat},
    lang_item::lang_items,
    signatures::FunctionSignature,
};
use hir_ty::{
    ParamEnvAndCrate,
    db::HirDatabase as _,
    mir::{
        BinOp, CastKind, LocalId, MirBody, MirSpan, Operand, OperandKind, PlaceTy, ProjectionElem,
        Rvalue, StatementKind, StoredPlace, TerminatorKind,
    },
    next_solver::{Ty, TyKind, infer::InferCtxt},
    traits::implements_trait_unique,
};
use rustc_type_ir::inherent::{AdtDef as _, GenericArgs as _, IntoKind as _};

use super::{
    effects::{self, DEEP, DIRECT, Effects, FRESH, STATIC, UNKNOWN, deep, derefs, direct, index_of},
    iter::{Chain, Std},
    region::{self, Cycles, Liveness},
};

/// A `for` loop of the body, in its HIR and its MIR.
pub(crate) struct Loop {
    /// The `for` expression, the desugaring's outer `match`.
    pub(crate) expr: ExprId,
    /// The iterable after `in`.
    pub(crate) head: ExprId,
    /// The loop's own body, the block.
    pub(crate) body: ExprId,
    /// The block the loop jumps back to.
    header: usize,
    /// The block whose call is `Iterator::next(&mut iter)`.
    next: usize,
    /// The iterator.
    pub(crate) iter: LocalId,
}

/// The `for` loops of `body`, found in its MIR.
pub(crate) fn for_loops(body: &Body, mir: &MirBody<'_>) -> Vec<Loop> {
    let mut found = Vec::new();
    for (id, expr) in body.exprs() {
        let Expr::Match { expr: into_iter, arms: [arm] } = expr else { continue };
        let Expr::Loop { body: inner, source: LoopSource::ForLoop, .. } = body.expr(arm.expr)
        else {
            continue;
        };
        let Expr::Call { args: [head], .. } = body.expr(into_iter) else { continue };
        let Pat::Bind { id: binding, .. } = body.pat(arm.pat) else { continue };
        let Some(&iter) = mir.binding_locals.get(binding) else { continue };
        let Expr::Block { tail: Some(tail), .. } = body.expr(inner) else { continue };
        let Expr::Match { expr: next_call, arms: [_, some] } = body.expr(tail) else { continue };
        let Pat::TupleStruct { args: [_], .. } = body.pat(some.pat) else { continue };
        let span = |it: &MirSpan, expr: ExprId| *it == MirSpan::ExprId(expr);
        let mut header = None;
        let mut next = None;
        for (block_id, block) in mir.basic_blocks.iter() {
            let Some(terminator) = &block.terminator else { continue };
            match &terminator.kind {
                TerminatorKind::Goto { target } if span(&terminator.span, arm.expr) => {
                    header = Some(region::block_index(*target));
                }
                TerminatorKind::Call { .. } if span(&terminator.span, next_call) => {
                    next = Some(region::block_index(block_id));
                }
                _ => {}
            }
        }
        let (Some(header), Some(next)) = (header, next) else { continue };
        found.push(Loop { expr: id, head: *head, body: some.expr, header, next, iter });
    }
    found
}

/// A value carried from iteration to iteration that any order gives the same of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reduction {
    Sum,
    Product,
    Count,
    Or,
    And,
    Xor,
    Min,
    Max,
    /// Inserted into a `BTreeSet`.
    Insert,
}

impl Reduction {
    /// What it is, said: "a sum".
    pub(crate) fn said(self) -> &'static str {
        match self {
            Reduction::Sum => "a sum (`+`)",
            Reduction::Product => "a product (`*`)",
            Reduction::Count => "a count",
            Reduction::Or => "an `|` of every iteration's",
            Reduction::And => "an `&` of every iteration's",
            Reduction::Xor => "an `^` of every iteration's",
            Reduction::Min => "a minimum",
            Reduction::Max => "a maximum",
            Reduction::Insert => "a set every iteration inserts into",
        }
    }

    /// The rayon consumer that computes it.
    pub(crate) fn rayon(self) -> &'static str {
        match self {
            Reduction::Sum => ".sum()",
            Reduction::Product => ".product()",
            Reduction::Count => ".count()",
            Reduction::Or => ".reduce(|| 0, |a, b| a | b)",
            Reduction::And => ".reduce(|| !0, |a, b| a & b)",
            Reduction::Xor => ".reduce(|| 0, |a, b| a ^ b)",
            Reduction::Min => ".min()",
            Reduction::Max => ".max()",
            Reduction::Insert => ".collect::<BTreeSet<_>>()",
        }
    }
}

/// A carried value that is a reduction, named.
pub(crate) struct Reduced {
    pub(crate) name: String,
    pub(crate) reduction: Reduction,
    /// Whether it is of integers whose overflow is checked: in another order, the iteration
    /// that overflows, if one does, may be another.
    pub(crate) overflow: bool,
    /// Whether it is of floats, which `#[blaze::reassociate]` lets be computed in any order.
    pub(crate) float: bool,
}

/// What keeps a loop from running its iterations in any order.
pub(crate) enum Dependence {
    /// `name` is read and written by every iteration, and not as a reduction.
    Carried { name: String },
    /// `name` is a reduction of floats, which rounds differently in another order.
    Float { name: String, reduction: Reduction },
    /// `name` is a `Vec` or `String` every iteration pushes onto, in order.
    Ordered { name: String },
    /// `name` is a `HashSet` every iteration inserts into, whose order of iteration depends on
    /// the order of insertion.
    HashOrder { name: String },
    /// An iteration reads `name` at an index another iteration writes.
    CrossIndex { name: String },
    /// A call has an effect the other iterations see in the order they run: I/O, a static
    /// that changes, an atomic. `said` says what, of the loop.
    Effect { said: String },
}

impl Dependence {
    pub(crate) fn said(&self) -> String {
        match self {
            Dependence::Carried { name } => {
                format!("`{name}` is carried from one iteration to the next")
            }
            Dependence::Float { name, reduction } => format!(
                "`{name}` is {} of floats, which rounds differently in another order",
                reduction.said()
            ),
            Dependence::Ordered { name } => {
                format!("`{name}` is pushed onto in the order the iterations run")
            }
            Dependence::HashOrder { name } => format!(
                "`{name}` is a `HashSet` the iterations insert into, and the order it iterates \
                 in depends on the order they run"
            ),
            Dependence::CrossIndex { name } => format!(
                "an iteration reads `{name}` at an index another iteration writes"
            ),
            Dependence::Effect { said } => {
                format!("each iteration {said}, which the others see in the order they run")
            }
        }
    }

    pub(crate) fn help(&self) -> String {
        match self {
            Dependence::Carried { name } => format!(
                "if `{name}` counts or accumulates, make it a reduction (`+`, `*`, `min`, `max`, \
                 `|`, `&`, `^`) of a value each iteration computes on its own"
            ),
            Dependence::Float { name, reduction } => format!(
                "mark the function `#[blaze::reassociate]` to let `{name}` be computed in any \
                 order, or compute each iteration's value in parallel and combine them in order \
                 (`.par_iter().map(..).collect::<Vec<_>>()` then `{}` over the `Vec`)",
                reduction.rayon()
            ),
            Dependence::Ordered { name } => format!(
                "build `{name}` with rayon's `.map(..).collect()`, which keeps the order"
            ),
            Dependence::HashOrder { name } => format!(
                "collect `{name}` into a `BTreeSet`, whose order is its elements'"
            ),
            Dependence::CrossIndex { name } => format!(
                "read `{name}` from a copy made before the loop, or keep the dependence serial"
            ),
            Dependence::Effect { .. } => "compute each iteration's value in parallel into a \
                                          `Vec` (`.par_iter().map(..).collect()`), then do the \
                                          effect over it in order"
                .to_owned(),
        }
    }
}

/// What a loop's analysis found.
pub(crate) enum Verdict {
    /// Its iterations are independent.
    Parallel(Found),
    /// They are but for one dependence.
    Serial(Found, Dependence),
    /// Neither, or blaze can't tell.
    No,
}

/// What an independent loop does, for its message.
pub(crate) struct Found {
    pub(crate) reductions: Vec<Reduced>,
    /// What it writes at its own index, named.
    pub(crate) indexed: Vec<String>,
    /// Whether it writes through its items.
    pub(crate) item_writes: bool,
    /// Whether an iteration may panic.
    pub(crate) may_panic: bool,
    /// The work of one iteration.
    pub(crate) work: u64,
}

/// The memory a pointer points into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Base {
    /// A local's own memory, and what it owns.
    Local(LocalId),
    /// The current item's memory.
    Item,
    /// What the parameter points at, and all that is reachable from there.
    Outer(LocalId),
    /// Memory allocated in the loop, or before it.
    Fresh { inside: bool },
    Static,
    Unknown,
}

/// Where a pointer points: its base, and the index into it when it is an element's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Root {
    base: Base,
    index: Option<LocalId>,
}

const UNKNOWN_ROOT: Root = Root { base: Base::Unknown, index: None };

fn root(base: Base) -> Option<Root> {
    Some(Root { base, index: None })
}

fn join(a: Option<Root>, b: Option<Root>) -> Option<Root> {
    match (a, b) {
        (None, it) | (it, None) => it,
        (Some(a), Some(b)) if a.base == b.base => {
            Some(Root { base: a.base, index: if a.index == b.index { a.index } else { None } })
        }
        _ => Some(UNKNOWN_ROOT),
    }
}

/// Where a local is defined.
#[derive(Clone, Copy)]
enum Site {
    Statement { block: usize, at: usize },
    Call { block: usize },
    Replace { block: usize },
}

#[derive(Clone, Copy)]
enum Slot {
    Unseen,
    Seeing,
    Seen(Option<Root>),
}

/// One body's MIR, ready to look at its loops.
pub(crate) struct Mir<'a, 'db> {
    pub(crate) db: &'db Db,
    pub(crate) env: ParamEnvAndCrate<'db>,
    pub(crate) mir: &'a MirBody<'db>,
    pub(crate) hir: &'a Body,
    pub(crate) std: &'a Std,
    /// Whether the body's item is `#[blaze::reassociate]`: its float reductions may be
    /// computed in any order.
    pub(crate) reassociate: bool,
    successors: Vec<Vec<usize>>,
    live: Liveness,
    defs: Vec<Vec<Site>>,
    calls: Vec<Option<(Effects, bool)>>,
    infcx: InferCtxt<'db>,
    blocks: Vec<&'a hir_ty::mir::BasicBlock>,
}

impl<'a, 'db> Mir<'a, 'db> {
    pub(crate) fn new(
        db: &'db Db,
        env: ParamEnvAndCrate<'db>,
        mir: &'a MirBody<'db>,
        hir: &'a Body,
        std: &'a Std,
        reassociate: bool,
    ) -> Mir<'a, 'db> {
        let successors = region::successors(mir);
        let live = Liveness::of(mir, &successors);
        let blocks: Vec<_> = mir.basic_blocks.iter().map(|(_, it)| it).collect();
        let mut defs = vec![Vec::new(); mir.locals.len()];
        let mut calls = Vec::with_capacity(blocks.len());
        for (index, block) in blocks.iter().enumerate() {
            for (at, statement) in block.statements.iter().enumerate() {
                if let StatementKind::Assign(place, _) = &statement.kind
                    && derefs(place) == 0
                {
                    defs[index_of(place.local)].push(Site::Statement { block: index, at });
                }
            }
            let call = match block.terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::Call { func, destination, intrinsic, .. })
                    if !block.is_cleanup =>
                {
                    if derefs(destination) == 0 {
                        defs[index_of(destination.local)].push(Site::Call { block: index });
                    }
                    Some(effects::call_summary(db, env, func, *intrinsic))
                }
                Some(TerminatorKind::DropAndReplace { place, .. }) if derefs(place) == 0 => {
                    defs[index_of(place.local)].push(Site::Replace { block: index });
                    None
                }
                _ => None,
            };
            calls.push(call);
        }
        let infcx = effects::infer_ctxt(db, env.krate);
        Mir { db, env, mir, hir, std, reassociate, successors, live, defs, calls, infcx, blocks }
    }

    fn local_ty(&self, local: LocalId) -> Ty<'db> {
        self.mir.locals[local].ty.as_ref()
    }

    fn place_ty(&self, local: LocalId, projection: &[hir_ty::mir::PlaceElem]) -> Ty<'db> {
        PlaceTy::from_ty(self.local_ty(local))
            .multi_projection_ty(&self.infcx, self.env.param_env, projection)
            .ty
    }

    /// The name of the binding `local` is, or `_` for a temporary.
    pub(crate) fn name(&self, local: LocalId) -> String {
        self.mir
            .binding_locals
            .iter()
            .find(|(_, it)| **it == local)
            .map_or_else(|| "_".to_owned(), |(binding, _)| self.hir[binding].name.as_str().to_owned())
    }

    fn implements(&self, ty: Ty<'db>, trait_: Option<hir_def::TraitId>) -> bool {
        trait_.is_some_and(|it| implements_trait_unique(ty, self.db, self.env, it))
    }

    /// Whether a value of `ty` owns all the memory its pointers reach: no reference, raw
    /// pointer, `dyn`, interior mutability or shared ownership, but in std's owning
    /// containers.
    fn owned(&self, ty: Ty<'db>, depth: u32) -> bool {
        if depth > 6 {
            return false;
        }
        match ty.kind() {
            TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Bool
            | TyKind::Char
            | TyKind::Never
            | TyKind::Str => true,
            TyKind::Array(element, _) | TyKind::Slice(element) => self.owned(element, depth + 1),
            TyKind::Tuple(fields) => fields.iter().all(|it| self.owned(it, depth + 1)),
            TyKind::Adt(def, args) => {
                let adt = def.def_id();
                if self.std.owns(adt) {
                    return args.types().all(|it| self.owned(it, depth + 1));
                }
                let lang = lang_items(self.db, self.env.krate);
                if matches!(adt, hir_def::AdtId::StructId(it) if lang.UnsafeCell == Some(it)) {
                    return false;
                }
                let interner = hir_ty::next_solver::DbInterner::new_no_crate(self.db);
                let mut owned = true;
                super::super::alloc::any_variant(self.db, adt, |variant| {
                    for (_, field) in self.db.field_types(variant).iter() {
                        let field = field.ty().instantiate(interner, args).skip_norm_wip();
                        owned &= self.owned(field, depth + 1);
                    }
                    !owned
                });
                owned
            }
            _ => false,
        }
    }

    /// Whether a value of `ty` may hold a pointer.
    fn has_pointers(&self, ty: Ty<'db>) -> bool {
        match ty.kind() {
            TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Bool
            | TyKind::Char
            | TyKind::Never => false,
            TyKind::Array(element, _) => self.has_pointers(element),
            TyKind::Tuple(fields) => fields.iter().any(|it| self.has_pointers(it)),
            _ => true,
        }
    }
}

/// Why a loop is not looked at further: it shares what it writes, or has an effect.
struct Shared;

impl<'a, 'db> Mir<'a, 'db> {
    /// What `lp`, whose iterator is the std chain `chain`, does. `returns_item` says whether a
    /// `Map` closure's result comes from its item alone.
    pub(crate) fn look(
        &self,
        lp: &Loop,
        chain: &Chain<'db>,
        returns_item: impl Fn(Ty<'db>) -> bool + Copy,
    ) -> Verdict {
        let Some(TerminatorKind::Call { destination, .. }) =
            self.blocks[lp.next].terminator.as_ref().map(|it| &it.kind)
        else {
            return Verdict::No;
        };
        if !destination.projection.as_slice().is_empty() {
            return Verdict::No;
        }
        // `next` may change the iterator itself, and nothing else.
        let Some((next, _)) = self.calls[lp.next] else { return Verdict::No };
        if !next.allowed() || next.writes & !direct(0) != 0 {
            return Verdict::No;
        }
        let item = destination.local;
        let Some(payload) = self.local_ty(item).as_adt().and_then(|(_, args)| args.types().next())
        else {
            return Verdict::No;
        };
        if !self.implements(payload, self.std.send) {
            return Verdict::No;
        }
        let look = Look {
            m: self,
            lp,
            inside: region::natural_loop(&self.successors, lp.header),
            item,
            item_owned: self.owned_through(payload),
            disjoint: chain.disjoint(returns_item),
            roots: std::cell::RefCell::new(vec![Slot::Unseen; self.mir.locals.len()]),
            own: self.own_indices(item, chain),
        };
        look.verdict(next).unwrap_or(Verdict::No)
    }

    /// Whether the memory a value of `ty` points at is owned through it: `&mut T` and tuples
    /// of such, with `T` [`owned`](Mir::owned).
    fn owned_through(&self, ty: Ty<'db>) -> bool {
        match ty.kind() {
            TyKind::Ref(_, inner, _) => self.owned(inner, 0),
            TyKind::Tuple(fields) => fields.iter().all(|it| self.owned_through(it)),
            _ => self.owned(ty, 0),
        }
    }

    /// The locals that hold the iteration's own index: the item of a range, through adaptors
    /// that keep items distinct, or the count of an outermost `enumerate`; and their copies.
    fn own_indices(&self, item: LocalId, chain: &Chain<'db>) -> Vec<bool> {
        let n = self.mir.locals.len();
        let mut own = vec![false; n];
        let (distinct, enumerated) = (chain.distinct(), chain.enumerated());
        let defined_by = |local: usize| -> Option<&StoredPlace> {
            let defs = &self.defs[local];
            let [Site::Statement { block, at }] = defs.as_slice() else { return None };
            match &self.blocks[*block].statements[*at].kind {
                StatementKind::Assign(_, Rvalue::Use(operand)) => match &operand.kind {
                    OperandKind::Copy(place) | OperandKind::Move(place) => Some(place),
                    _ => None,
                },
                _ => None,
            }
        };
        for (local, it) in own.iter_mut().enumerate() {
            let Some(place) = defined_by(local) else { continue };
            if place.local != item {
                continue;
            }
            let mut fields = Vec::new();
            for elem in place.projection.as_slice() {
                match elem {
                    ProjectionElem::Downcast(_) => {}
                    ProjectionElem::Field(field) => fields.push(field_index(field)),
                    _ => {
                        fields.clear();
                        fields.push(u32::MAX);
                        break;
                    }
                }
            }
            *it = (distinct && fields == [0]) || (enumerated && fields == [0, 0]);
        }
        loop {
            let mut changed = false;
            for local in 0..n {
                if own[local] {
                    continue;
                }
                let Some(place) = defined_by(local) else { continue };
                if place.projection.as_slice().is_empty() && own[index_of(place.local)] {
                    own[local] = true;
                    changed = true;
                }
            }
            if !changed {
                return own;
            }
        }
    }
}

fn field_index(field: &hir_ty::mir::FieldIndex) -> u32 {
    field.0
}

/// One loop of a body, as it is looked at.
struct Look<'m, 'a, 'db> {
    m: &'m Mir<'a, 'db>,
    lp: &'m Loop,
    /// The blocks of the loop.
    inside: Vec<bool>,
    /// The next call's destination, `Option<Item>`.
    item: LocalId,
    /// Whether the item's pointers reach memory it owns alone.
    item_owned: bool,
    /// Whether two iterations' items never share memory.
    disjoint: bool,
    roots: std::cell::RefCell<Vec<Slot>>,
    /// The locals that hold the iteration's own index.
    own: Vec<bool>,
}

/// What the loop's statements write and read.
struct Tally {
    /// The locals the loop starts with that it writes, but at its own index.
    mutated: Vec<bool>,
    /// What it writes at its own index.
    indexed: Vec<Base>,
    /// What it reads through pointers or at an index, and whether only the length.
    reads: Vec<(Root, bool)>,
    /// The locals it names.
    used: Vec<bool>,
    item_writes: bool,
    may_panic: bool,
    /// The effects its calls have that show the order of the iterations, named.
    effects: Vec<Dependence>,
}

impl<'db> Look<'_, '_, 'db> {
    fn outer(&self, local: LocalId) -> bool {
        self.m.live.live_in(self.lp.header, local)
    }

    fn is_own(&self, index: Option<LocalId>) -> bool {
        index.is_some_and(|it| self.own[index_of(it)])
    }

    fn root_of(&self, local: LocalId) -> Option<Root> {
        let at = index_of(local);
        match self.roots.borrow()[at] {
            Slot::Seen(it) => return it,
            Slot::Seeing => return Some(UNKNOWN_ROOT),
            Slot::Unseen => {}
        }
        self.roots.borrow_mut()[at] = Slot::Seeing;
        let found = self.compute_root(local);
        self.roots.borrow_mut()[at] = Slot::Seen(found);
        found
    }

    fn compute_root(&self, local: LocalId) -> Option<Root> {
        if local == self.item {
            return root(Base::Item);
        }
        if !self.m.has_pointers(self.m.local_ty(local)) {
            return None;
        }
        if self.m.mir.param_locals.contains(&local) {
            return root(Base::Outer(local));
        }
        let defs = &self.m.defs[index_of(local)];
        if defs.is_empty() {
            return Some(UNKNOWN_ROOT);
        }
        defs.iter().fold(None, |found, site| join(found, self.site_root(*site)))
    }

    fn site_root(&self, site: Site) -> Option<Root> {
        match site {
            Site::Statement { block, at } => match &self.m.blocks[block].statements[at].kind {
                StatementKind::Assign(_, rvalue) => self.rvalue_root(rvalue),
                _ => None,
            },
            Site::Call { block } => self.call_root(block),
            Site::Replace { block } => match self.m.blocks[block].terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::DropAndReplace { value, .. }) => self.operand_root(value),
                _ => Some(UNKNOWN_ROOT),
            },
        }
    }

    fn rvalue_root(&self, rvalue: &Rvalue) -> Option<Root> {
        match rvalue {
            Rvalue::Use(it) | Rvalue::Repeat(it, _) | Rvalue::UnaryOp(_, it) => self.operand_root(it),
            Rvalue::Cast(CastKind::PointerFromExposedAddress | CastKind::Transmute, _, _) => {
                Some(UNKNOWN_ROOT)
            }
            Rvalue::Cast(_, it, _) => self.operand_root(it),
            Rvalue::Ref(_, place) => Some(self.memory_root(place.local, place.projection.as_slice())),
            Rvalue::CopyForDeref(place) => self.value_root(place.local, place.projection.as_slice()),
            Rvalue::BinaryOp(BinOp::Offset, it, _) | Rvalue::CheckedBinaryOp(BinOp::Offset, it, _) => {
                self.operand_root(it).map(|it| Root { index: None, ..it })
            }
            Rvalue::BinaryOp(..) | Rvalue::CheckedBinaryOp(..) => None,
            Rvalue::Aggregate(_, operands) => {
                operands.iter().fold(None, |found, it| join(found, self.operand_root(it)))
            }
            Rvalue::Len(_) | Rvalue::Discriminant(_) => None,
            Rvalue::ThreadLocalRef(n) | Rvalue::AddressOf(n) | Rvalue::NullaryOp(n) => match *n {},
        }
    }

    fn operand_root(&self, operand: &Operand) -> Option<Root> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                self.value_root(place.local, place.projection.as_slice())
            }
            OperandKind::Static(_) => root(Base::Static),
            OperandKind::Constant { .. } | OperandKind::Allocation { .. } => None,
        }
    }

    /// Where the pointers of the value at `local` projected by `projection` point.
    fn value_root(&self, local: LocalId, projection: &[hir_ty::mir::PlaceElem]) -> Option<Root> {
        let Some(last) = projection.iter().rposition(|it| matches!(it, ProjectionElem::Deref))
        else {
            return self.root_of(local);
        };
        if !self.m.has_pointers(self.m.place_ty(local, projection)) {
            return None;
        }
        // A pointer loaded from memory points into what that memory owns.
        let memory = self.memory_root(local, &projection[..=last]);
        Some(self.deeper(memory))
    }

    /// The memory the place `local` projected by `projection` is in.
    fn memory_root(&self, local: LocalId, projection: &[hir_ty::mir::PlaceElem]) -> Root {
        let first_index = |elems: &[hir_ty::mir::PlaceElem]| {
            elems.iter().find_map(|it| match it {
                ProjectionElem::Index(local) => Some(*local),
                _ => None,
            })
        };
        let Some(last) = projection.iter().rposition(|it| matches!(it, ProjectionElem::Deref))
        else {
            return Root { base: Base::Local(local), index: first_index(projection) };
        };
        let pointer = self.value_root(local, &projection[..last]).unwrap_or(UNKNOWN_ROOT);
        Root { base: pointer.base, index: pointer.index.or(first_index(&projection[last + 1..])) }
    }

    /// The memory reachable through the pointers in the memory of `root`.
    fn deeper(&self, root: Root) -> Root {
        let base = match root.base {
            Base::Local(local) if self.m.owned(self.m.local_ty(local), 0) => Base::Local(local),
            Base::Local(local) => match self.root_of(local) {
                Some(Root { base: Base::Outer(param), .. }) => Base::Outer(param),
                _ => Base::Unknown,
            },
            Base::Item if self.item_owned => Base::Item,
            Base::Outer(param) => Base::Outer(param),
            Base::Static => Base::Static,
            _ => Base::Unknown,
        };
        Root { base, index: None }
    }

    /// The callee's parameter memory `bits` of a call with `args`, where it is in this body.
    fn mapped(&self, bits: u64, args: &[Operand], fn_call: bool) -> Vec<Root> {
        let mut roots = Vec::new();
        for (index, arg) in args.iter().enumerate() {
            let (own, deeper) = match (fn_call, index) {
                (true, 0) => (bits & direct(0), bits & deep(0)),
                (true, 1) => (bits & DIRECT & !direct(0), bits & DEEP & !deep(0)),
                (true, _) => (0, 0),
                (false, _) => (bits & direct(index), bits & deep(index)),
            };
            if own | deeper == 0 {
                continue;
            }
            let Some(points) = self.operand_root(arg) else { continue };
            if own != 0 {
                roots.push(points);
            }
            if deeper != 0 {
                roots.push(self.deeper(points));
            }
        }
        roots
    }

    /// Where the pointers a call returns point.
    fn call_root(&self, block: usize) -> Option<Root> {
        if block == self.lp.next {
            return root(Base::Item);
        }
        let Some(TerminatorKind::Call { func, args, .. }) =
            self.m.blocks[block].terminator.as_ref().map(|it| &it.kind)
        else {
            return Some(UNKNOWN_ROOT);
        };
        if self.is_index(func)
            && let [base, index] = &args[..]
        {
            let index = match &index.kind {
                OperandKind::Copy(place) | OperandKind::Move(place)
                    if place.projection.as_slice().is_empty() =>
                {
                    Some(place.local)
                }
                _ => None,
            };
            return self.operand_root(base).map(|it| Root { base: it.base, index });
        }
        let Some((summary, fn_call)) = self.m.calls[block] else { return Some(UNKNOWN_ROOT) };
        let mut found = None;
        for it in self.mapped(summary.returns & (DIRECT | DEEP), args, fn_call) {
            found = join(found, Some(Root { index: None, ..it }));
        }
        if summary.returns & FRESH != 0 {
            found = join(found, root(Base::Fresh { inside: self.inside[block] }));
        }
        if summary.returns & STATIC != 0 {
            found = join(found, root(Base::Static));
        }
        if summary.returns & UNKNOWN != 0 {
            found = join(found, Some(UNKNOWN_ROOT));
        }
        found
    }

    /// Whether `func` is `Index::index` or `IndexMut::index_mut`, which return a pointer to the
    /// element at their second argument.
    fn is_index(&self, func: &Operand) -> bool {
        let db = self.m.db;
        let Some((f, _)) = func.fn_item() else { return false };
        let trait_ = match f.loc(db).container {
            ItemContainerId::TraitId(it) => Some(it),
            ItemContainerId::ImplId(it) => db.impl_trait(it).map(|it| it.skip_binder().def_id.0),
            _ => None,
        };
        trait_.is_some_and(|it| [self.m.std.index, self.m.std.index_mut].contains(&Some(it)))
            && matches!(FunctionSignature::of(db, f).name.as_str(), "index" | "index_mut")
    }

    /// Takes in a write to the memory of `root`.
    fn write(&self, root: Root, tally: &mut Tally) -> Result<(), Shared> {
        match root.base {
            Base::Local(local) if local == self.lp.iter => Err(Shared),
            Base::Local(local) if !self.outer(local) => Ok(()),
            Base::Local(local) if self.is_own(root.index) => {
                tally.indexed.push(Base::Local(local));
                Ok(())
            }
            Base::Local(local) => {
                tally.mutated[index_of(local)] = true;
                Ok(())
            }
            Base::Item if self.disjoint => {
                tally.item_writes = true;
                Ok(())
            }
            Base::Outer(param) if self.is_own(root.index) => {
                tally.indexed.push(Base::Outer(param));
                Ok(())
            }
            Base::Fresh { inside: true } => Ok(()),
            _ => Err(Shared),
        }
    }

    fn read_place(&self, place: &StoredPlace, len: bool, tally: &mut Tally) {
        self.use_place(place, tally);
        let projection = place.projection.as_slice();
        let reaches = projection
            .iter()
            .any(|it| matches!(it, ProjectionElem::Deref | ProjectionElem::Index(_)));
        if reaches || len {
            tally.reads.push((self.memory_root(place.local, projection), len));
        }
    }

    fn use_place(&self, place: &StoredPlace, tally: &mut Tally) {
        tally.used[index_of(place.local)] = true;
        for elem in place.projection.as_slice() {
            if let ProjectionElem::Index(local) = elem {
                tally.used[index_of(*local)] = true;
            }
        }
    }

    fn read_operand(&self, operand: &Operand, tally: &mut Tally) -> Result<(), Shared> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                self.read_place(place, false, tally);
                Ok(())
            }
            OperandKind::Static(id) if effects::static_changes(self.m.db, self.m.env.krate, *id) => {
                Err(Shared)
            }
            _ => Ok(()),
        }
    }

    /// Takes in a write to `place`.
    fn write_place(&self, place: &StoredPlace, tally: &mut Tally) -> Result<(), Shared> {
        self.use_place(place, tally);
        let root = self.memory_root(place.local, place.projection.as_slice());
        self.write(root, tally)
    }

    /// The loop's verdict, or [`Shared`] when it shares what it writes or has an effect.
    fn verdict(&self, next: Effects) -> Result<Verdict, Shared> {
        let m = self.m;
        let n = m.mir.locals.len();
        let mut tally = Tally {
            mutated: vec![false; n],
            indexed: Vec::new(),
            reads: Vec::new(),
            used: vec![false; n],
            item_writes: false,
            may_panic: next.has(effects::Effect::Panic),
            effects: Vec::new(),
        };
        let inner = Cycles::of_graph(&m.successors, &self.inside, Some(self.lp.header));
        let mut work: u64 = 0;
        for (index, block) in m.blocks.iter().enumerate() {
            if !self.inside[index] || block.is_cleanup {
                continue;
            }
            let weight = if inner.contains_index(index) { effects::LOOP_WEIGHT } else { 1 };
            work = work.saturating_add(effects::block_cost(m.db, m.env, block).saturating_mul(weight));
            for statement in &block.statements {
                tally.may_panic |= statement.kind.may_panic();
                match &statement.kind {
                    StatementKind::Assign(place, rvalue) => {
                        self.rvalue_reads(rvalue, &mut tally)?;
                        self.write_place(place, &mut tally)?;
                    }
                    StatementKind::InlineAsm(_) => return Err(Shared),
                    StatementKind::FakeRead(place) => self.use_place(place, &mut tally),
                    StatementKind::Deinit(place) => self.write_place(place, &mut tally)?,
                    StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => {}
                }
            }
            let Some(terminator) = &block.terminator else { continue };
            match &terminator.kind {
                TerminatorKind::Call { args, destination, .. } if index == self.lp.next => {
                    for arg in args.iter() {
                        self.read_operand(arg, &mut tally)?;
                    }
                    self.use_place(destination, &mut tally);
                }
                TerminatorKind::Call { func, args, destination, .. } => {
                    let Some((summary, fn_call)) = m.calls[index] else { return Err(Shared) };
                    if !summary.allowed() {
                        // An effect blaze knows is one other iterations see in the order
                        // they run: a dependence, named. What blaze can't see into is not.
                        let Some(why) = summary.why.filter(|_| summary.only_ordered()) else {
                            return Err(Shared);
                        };
                        let said = why.said(m.db);
                        let seen = tally
                            .effects
                            .iter()
                            .any(|it| matches!(it, Dependence::Effect { said: s } if *s == said));
                        if !seen {
                            tally.effects.push(Dependence::Effect { said });
                        }
                    }
                    tally.may_panic |= summary.has(effects::Effect::Panic);
                    let len = func.fn_item().is_some_and(|(f, _)| {
                        matches!(FunctionSignature::of(m.db, f).name.as_str(), "len" | "is_empty")
                    });
                    for arg in args.iter() {
                        self.read_operand(arg, &mut tally)?;
                        if let Some(root) = self.operand_root(arg) {
                            tally.reads.push((root, len));
                        }
                    }
                    for root in self.mapped(summary.writes, args, fn_call) {
                        self.write(root, &mut tally)?;
                    }
                    self.write_place(destination, &mut tally)?;
                }
                TerminatorKind::Drop { place, .. } | TerminatorKind::DropAndReplace { place, .. } => {
                    let ty = self.m.place_ty(place.local, place.projection.as_slice());
                    if !effects::drop_summary(m.db, m.env, ty).allowed() {
                        return Err(Shared);
                    }
                    if let TerminatorKind::DropAndReplace { value, .. } = &terminator.kind {
                        self.read_operand(value, &mut tally)?;
                    }
                    self.write_place(place, &mut tally)?;
                }
                TerminatorKind::Assert { cond, .. } => {
                    tally.may_panic = true;
                    self.read_operand(cond, &mut tally)?;
                }
                TerminatorKind::SwitchInt { discr, .. } => self.read_operand(discr, &mut tally)?,
                TerminatorKind::Yield { .. }
                | TerminatorKind::CoroutineDrop
                | TerminatorKind::TailCall { .. } => return Err(Shared),
                _ => {}
            }
        }
        let mut dependences = std::mem::take(&mut tally.effects);
        for &base in &tally.indexed {
            let at_other = tally.reads.iter().any(|&(read, len)| {
                read.base == base && !len && !self.is_own(read.index)
            });
            let whole = matches!(base, Base::Local(local) if tally.mutated[index_of(local)]);
            if at_other || whole {
                let name = match base {
                    Base::Local(local) | Base::Outer(local) => m.name(local),
                    _ => "_".to_owned(),
                };
                if !dependences.iter().any(|it| matches!(it, Dependence::CrossIndex { name: n } if *n == name)) {
                    dependences.push(Dependence::CrossIndex { name });
                }
            }
        }
        let ids: Vec<LocalId> = m.mir.locals.iter().map(|(id, _)| id).collect();
        // What every iteration reads and none writes is shared between threads.
        for (local, &id) in ids.iter().enumerate() {
            if !tally.used[local] || tally.mutated[local] || id == self.lp.iter || !self.outer(id) {
                continue;
            }
            let indexed = tally
                .indexed
                .iter()
                .any(|it| matches!(it, Base::Local(l) | Base::Outer(l) if *l == id));
            if !indexed && !m.implements(m.local_ty(id), m.std.sync) {
                return Err(Shared);
            }
        }
        let mut reductions = Vec::new();
        for (local, &id) in ids.iter().enumerate() {
            if !tally.mutated[local] || tally.indexed.contains(&Base::Local(id)) {
                continue;
            }
            match self.reduction(id) {
                Ok(it) => {
                    if !m.implements(m.local_ty(id), m.std.send) {
                        return Err(Shared);
                    }
                    reductions.push(it);
                }
                Err(it) => dependences.push(it),
            }
        }
        let indexed = tally
            .indexed
            .iter()
            .filter_map(|it| match it {
                Base::Local(local) | Base::Outer(local) => Some(m.name(*local)),
                _ => None,
            })
            .fold(Vec::new(), |mut names: Vec<String>, name| {
                if !names.contains(&name) {
                    names.push(name);
                }
                names
            });
        let found = Found {
            reductions,
            indexed,
            item_writes: tally.item_writes,
            may_panic: tally.may_panic,
            work,
        };
        Ok(match dependences.len() {
            0 => Verdict::Parallel(found),
            1 => Verdict::Serial(found, dependences.remove(0)),
            _ => Verdict::No,
        })
    }

    fn rvalue_reads(&self, rvalue: &Rvalue, tally: &mut Tally) -> Result<(), Shared> {
        match rvalue {
            Rvalue::Ref(_, place) | Rvalue::Discriminant(place) | Rvalue::CopyForDeref(place) => {
                self.read_place(place, false, tally);
            }
            Rvalue::Len(place) => self.read_place(place, true, tally),
            _ => {
                let mut result = Ok(());
                rvalue.for_each_operand(&mut |it| {
                    if result.is_ok() {
                        result = self.read_operand(it, tally);
                    }
                });
                result?;
            }
        }
        Ok(())
    }

    /// The statements and terminators of the loop that name `local`, as (block, statement
    /// index or `None` for the terminator).
    fn occurrences(&self, local: LocalId) -> Vec<(usize, Option<usize>)> {
        let names_place = |place: &StoredPlace| {
            place.local == local
                || place
                    .projection
                    .as_slice()
                    .iter()
                    .any(|it| matches!(it, ProjectionElem::Index(l) if *l == local))
        };
        let names_operand = |operand: &Operand| match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => names_place(place),
            _ => false,
        };
        let mut found = Vec::new();
        for (index, block) in self.m.blocks.iter().enumerate() {
            if !self.inside[index] || block.is_cleanup {
                continue;
            }
            for (at, statement) in block.statements.iter().enumerate() {
                let names = match &statement.kind {
                    StatementKind::Assign(place, rvalue) => {
                        let mut names = names_place(place);
                        match rvalue {
                            Rvalue::Ref(_, it)
                            | Rvalue::Len(it)
                            | Rvalue::Discriminant(it)
                            | Rvalue::CopyForDeref(it) => names |= names_place(it),
                            _ => rvalue.for_each_operand(&mut |it| names |= names_operand(it)),
                        }
                        names
                    }
                    StatementKind::FakeRead(place) | StatementKind::Deinit(place) => {
                        names_place(place)
                    }
                    StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::InlineAsm(_)
                    | StatementKind::Nop => false,
                };
                if names {
                    found.push((index, Some(at)));
                }
            }
            if let Some(terminator) = &block.terminator {
                let mut names = match &terminator.kind {
                    TerminatorKind::Call { destination, .. } => names_place(destination),
                    TerminatorKind::Drop { place, .. }
                    | TerminatorKind::DropAndReplace { place, .. } => names_place(place),
                    _ => false,
                };
                terminator.kind.for_each_operand(&mut |it| names |= names_operand(it));
                if names {
                    found.push((index, None));
                }
            }
        }
        found
    }

    fn statement(&self, block: usize, at: usize) -> &StatementKind {
        &self.m.blocks[block].statements[at].kind
    }

    /// The reduction the carried local `local` is, or what keeps it from being one.
    fn reduction(&self, local: LocalId) -> Result<Reduced, Dependence> {
        let m = self.m;
        let name = m.name(local);
        let carried = || Dependence::Carried { name: name.clone() };
        let sites = self.occurrences(local);
        fn bare(operand: &Operand, of: LocalId) -> bool {
            match &operand.kind {
                OperandKind::Copy(place) | OperandKind::Move(place) => {
                    place.local == of && place.projection.as_slice().is_empty()
                }
                _ => false,
            }
        }
        fn assigned(kind: &StatementKind, of: LocalId) -> Option<&Rvalue> {
            match kind {
                StatementKind::Assign(place, rvalue)
                    if place.local == of && place.projection.as_slice().is_empty() =>
                {
                    Some(rvalue)
                }
                _ => None,
            }
        }
        // `local = op(local, e)`, at once or through one temporary.
        fn binary(rvalue: &Rvalue, local: LocalId) -> Option<(&BinOp, &Operand, bool)> {
            match rvalue {
                Rvalue::BinaryOp(op, a, b) | Rvalue::CheckedBinaryOp(op, a, b) => {
                    let checked = matches!(rvalue, Rvalue::CheckedBinaryOp(..));
                    match (bare(a, local), bare(b, local)) {
                        (true, false) => Some((op, b, checked)),
                        (false, true) => Some((op, a, checked)),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        let statements: Vec<&StatementKind> = sites
            .iter()
            .filter_map(|&(block, at)| at.map(|at| self.statement(block, at)))
            .collect();
        let terminators: Vec<usize> =
            sites.iter().filter(|it| it.1.is_none()).map(|it| it.0).collect();
        let op = match (statements.as_slice(), terminators.as_slice()) {
            ([kind], []) => assigned(kind, local).and_then(|it| binary(it, local)),
            ([_, _], []) => {
                let via = |from: usize, to: usize| {
                    let (from, to) = (statements[from], statements[to]);
                    let StatementKind::Assign(temp, rvalue) = from else { return None };
                    let op = binary(rvalue, local)?;
                    let Rvalue::Use(moved) = assigned(to, local)? else { return None };
                    let private = temp.projection.as_slice().is_empty()
                        && bare(moved, temp.local)
                        && !self.outer(temp.local)
                        && self.occurrences(temp.local).len() == 2;
                    private.then_some(op)
                };
                via(0, 1).or_else(|| via(1, 0))
            }
            _ => None,
        };
        let ty = m.local_ty(local);
        if let Some((op, other, checked)) = op {
            let one = constant(m.db, other) == Some(1);
            let reduction = match op {
                BinOp::Add if one => Reduction::Count,
                BinOp::Add => Reduction::Sum,
                BinOp::Mul => Reduction::Product,
                BinOp::BitOr => Reduction::Or,
                BinOp::BitAnd => Reduction::And,
                BinOp::BitXor => Reduction::Xor,
                _ => return Err(carried()),
            };
            return self.typed(name, ty, reduction, checked && matches!(op, BinOp::Add | BinOp::Mul));
        }
        // `local = min(local, e)` / `local.max(e)`: `local` passed as it is or read into a
        // temporary only the call takes (rustc's `as_call_operand`), the result straight into
        // `local` or through a temporary only `local` takes.
        let min_max = 'min_max: {
            // A `FakeRead` (of an assignment's right side) uses nothing.
            let private = |temp: LocalId| {
                let uses = self.occurrences(temp).into_iter().filter(|&(block, at)| {
                    at.is_none_or(|at| !matches!(self.statement(block, at), StatementKind::FakeRead(_)))
                });
                !self.outer(temp) && uses.count() == 2
            };
            let (mut read, mut written) = (None, None);
            for &kind in &statements {
                match kind {
                    StatementKind::Assign(place, Rvalue::Use(it))
                        if place.local != local
                            && place.projection.as_slice().is_empty()
                            && bare(it, local) =>
                    {
                        if read.replace(place.local).is_some() {
                            break 'min_max None;
                        }
                    }
                    _ => match assigned(kind, local) {
                        Some(Rvalue::Use(Operand {
                            kind: OperandKind::Copy(from) | OperandKind::Move(from),
                            ..
                        })) if written.is_none() && from.projection.as_slice().is_empty() => {
                            written = Some(from.local);
                        }
                        _ => break 'min_max None,
                    },
                }
            }
            let block = match (terminators.as_slice(), read) {
                ([block], _) => *block,
                ([], Some(temp)) => match self.occurrences(temp).iter().find(|it| it.1.is_none()) {
                    Some(&(block, _)) => block,
                    None => break 'min_max None,
                },
                _ => break 'min_max None,
            };
            let Some(TerminatorKind::Call { func, args, destination, .. }) =
                m.blocks[block].terminator.as_ref().map(|it| &it.kind)
            else {
                break 'min_max None;
            };
            let passed = read.unwrap_or(local);
            // An assignment reads its right side into a temporary of its own before it assigns
            // (`read_assigned`), so the result may come through more than one.
            let copied_from = |temp: LocalId| {
                self.occurrences(temp).into_iter().find_map(|(block, at)| {
                    match assigned(self.statement(block, at?), temp)? {
                        Rvalue::Use(Operand {
                            kind: OperandKind::Copy(from) | OperandKind::Move(from),
                            ..
                        }) if from.projection.as_slice().is_empty() => Some(from.local),
                        _ => None,
                    }
                })
            };
            let into = match written {
                None => destination.local == local,
                Some(mut temp) => {
                    let mut steps = 0;
                    loop {
                        if steps == 4 || !private(temp) {
                            break false;
                        }
                        if temp == destination.local {
                            break true;
                        }
                        let Some(from) = copied_from(temp) else { break false };
                        temp = from;
                        steps += 1;
                    }
                }
            };
            let shape = args.len() == 2
                && args.iter().filter(|it| bare(it, passed)).count() == 1
                && read.is_none_or(|it| private(it))
                && destination.projection.as_slice().is_empty()
                && into;
            if shape { self.min_max(func) } else { None }
        };
        if let Some(reduction) = min_max {
            return self.typed(name, ty, reduction, false);
        }
        if let [StatementKind::Assign(borrow, Rvalue::Ref(_, place))] = statements.as_slice()
            && terminators.is_empty()
            && place.local == local
            && place.projection.as_slice().is_empty()
            && borrow.projection.as_slice().is_empty()
            && let Some((method, unused)) = self.called_with(borrow.local)
            && let Some((adt, _)) = ty.as_adt()
        {
            let is = |it: Option<hir_def::AdtId>| it == Some(adt);
            return match method.as_str() {
                "insert" if unused && is(m.std.btree_set) => {
                    self.typed(name, ty, Reduction::Insert, false)
                }
                "insert" if is(m.std.hash_set) => Err(Dependence::HashOrder { name }),
                "push" | "push_str" if is(m.std.vec) || is(m.std.string) => {
                    Err(Dependence::Ordered { name })
                }
                _ => Err(carried()),
            };
        }
        Err(carried())
    }

    /// `set.insert(x)` / `v.push(x)`: when the loop names the borrow `borrow` only as the
    /// first argument of one call, that call's function's name, and whether its result is
    /// unused.
    fn called_with(&self, borrow: LocalId) -> Option<(String, bool)> {
        let m = self.m;
        let sites = self.occurrences(borrow);
        let calls: Vec<usize> = sites.iter().filter(|it| it.1.is_none()).map(|it| it.0).collect();
        let ([block], 1) = (calls.as_slice(), sites.len() - calls.len()) else { return None };
        let Some(TerminatorKind::Call { func, args, destination, target, .. }) =
            m.blocks[*block].terminator.as_ref().map(|it| &it.kind)
        else {
            return None;
        };
        let first = args.first().is_some_and(|it| match &it.kind {
            OperandKind::Move(place) | OperandKind::Copy(place) => {
                place.local == borrow && place.projection.as_slice().is_empty()
            }
            _ => false,
        });
        if !first || args.iter().skip(1).any(|it| self.operand_names(it, borrow)) {
            return None;
        }
        let (f, _) = func.fn_item()?;
        let unused = destination.projection.as_slice().is_empty()
            && target.is_none_or(|target| {
                !m.live.live_in(region::block_index(target), destination.local)
            });
        Some((FunctionSignature::of(m.db, f).name.as_str().to_owned(), unused))
    }

    fn operand_names(&self, operand: &Operand, local: LocalId) -> bool {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => place.local == local,
            _ => false,
        }
    }

    /// The reduction `min` or `max` is, if `func` names one: `cmp::min`, `cmp::max`, `Ord`'s,
    /// or std's float methods.
    fn min_max(&self, func: &Operand) -> Option<Reduction> {
        let m = self.m;
        let (f, _) = func.fn_item()?;
        let name = FunctionSignature::of(m.db, f).name.as_str().to_owned();
        let reduction = match name.as_str() {
            "min" => Reduction::Min,
            "max" => Reduction::Max,
            _ => return None,
        };
        if Some(f) == m.std.min || Some(f) == m.std.max {
            return Some(reduction);
        }
        let std = match f.loc(m.db).container {
            ItemContainerId::TraitId(trait_) => Some(trait_) == m.std.ord,
            ItemContainerId::ImplId(_) => is_lang(m.db, f),
            _ => false,
        };
        std.then_some(reduction)
    }

    /// `reduction` of a value of `ty`: exact on integers and `bool`s, and on floats only when
    /// the body lets blaze reassociate.
    fn typed(
        &self,
        name: String,
        ty: Ty<'db>,
        reduction: Reduction,
        overflow: bool,
    ) -> Result<Reduced, Dependence> {
        match ty.kind() {
            TyKind::Int(_) | TyKind::Uint(_) | TyKind::Bool => {
                Ok(Reduced { name, reduction, overflow, float: false })
            }
            TyKind::Adt(..) if reduction == Reduction::Insert => {
                Ok(Reduced { name, reduction, overflow: false, float: false })
            }
            TyKind::Float(_) if self.m.reassociate => {
                Ok(Reduced { name, reduction, overflow: false, float: true })
            }
            TyKind::Float(_) => Err(Dependence::Float { name, reduction }),
            _ => Err(Dependence::Carried { name }),
        }
    }
}

/// Whether `f` is of a crate of the toolchain's (`core`, `alloc`, `std`).
fn is_lang(db: &Db, f: FunctionId) -> bool {
    use hir_def::HasModule as _;
    hir::Crate::from(f.krate(db)).origin(db).is_lang()
}

/// The value of an integer constant operand.
fn constant(db: &Db, operand: &Operand) -> Option<u128> {
    match &operand.kind {
        OperandKind::Constant { konst, .. } => hir_ty::consteval::try_const_usize(db, konst.as_ref()),
        OperandKind::Allocation { allocation } => {
            let memory = &allocation.as_ref().memory;
            (memory.len() <= 16).then(|| {
                memory.iter().rev().fold(0u128, |value, byte| value << 8 | u128::from(*byte))
            })
        }
        _ => None,
    }
}
