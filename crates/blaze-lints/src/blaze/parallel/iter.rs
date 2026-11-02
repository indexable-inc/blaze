//! The iterators blaze knows how to split: std's sources and adaptors, their rayon names, and
//! how many items a written source gives.
//!
//! A type is taken apart from the outside in: `Map<Enumerate<slice::Iter<'_, T>>, F>` is the
//! source `slice::Iter`, then `Enumerate`, then `Map` with the closure `F`. A type that is not
//! one of these, all the way down, is not split: rayon has no `par_iter` for it.

use hir_def::{
    AdtId, FunctionId, HasModule as _, TraitId,
    expr_store::{Body, path::Path},
    hir::{Expr, ExprId, Literal, RangeOp},
    lang_item::{LangItemTarget, lang_items},
};
use intern::sym;
use hir_ty::{
    InferenceResult,
    next_solver::{Ty, TyKind},
};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _};

use crate::check::Cx;

/// The std items the lints compare against, found by their public paths.
pub(crate) struct Std {
    slice_iter: Option<AdtId>,
    slice_iter_mut: Option<AdtId>,
    chunks: Option<AdtId>,
    chunks_mut: Option<AdtId>,
    chunks_exact: Option<AdtId>,
    chunks_exact_mut: Option<AdtId>,
    windows: Option<AdtId>,
    vec_into_iter: Option<AdtId>,
    array_into_iter: Option<AdtId>,
    range: Option<AdtId>,
    range_inclusive: Option<AdtId>,
    map: Option<AdtId>,
    filter: Option<AdtId>,
    filter_map: Option<AdtId>,
    enumerate: Option<AdtId>,
    copied: Option<AdtId>,
    cloned: Option<AdtId>,
    zip: Option<AdtId>,
    rev: Option<AdtId>,
    take: Option<AdtId>,
    skip: Option<AdtId>,
    step_by: Option<AdtId>,
    chain: Option<AdtId>,
    pub(crate) vec: Option<AdtId>,
    pub(crate) string: Option<AdtId>,
    pub(crate) owned_box: Option<AdtId>,
    pub(crate) vec_deque: Option<AdtId>,
    pub(crate) btree_set: Option<AdtId>,
    pub(crate) btree_map: Option<AdtId>,
    pub(crate) hash_set: Option<AdtId>,
    pub(crate) hash_map: Option<AdtId>,
    pub(crate) iterator: Option<TraitId>,
    pub(crate) send: Option<TraitId>,
    pub(crate) sync: Option<TraitId>,
    pub(crate) index: Option<TraitId>,
    pub(crate) index_mut: Option<TraitId>,
    pub(crate) ord: Option<TraitId>,
    pub(crate) min: Option<FunctionId>,
    pub(crate) max: Option<FunctionId>,
    pub(crate) next: Option<FunctionId>,
}

impl Std {
    pub(crate) fn new(cx: &Cx<'_, '_>) -> Std {
        let db = cx.db();
        // An item of `core` or `alloc` is also `std`'s, at the same path past the crate: a
        // crate that has only `std` among its dependencies reaches them there.
        let def = |path: &[&str]| {
            cx.std_def(path).or_else(|| {
                let mut in_std = Vec::with_capacity(path.len());
                in_std.push("std");
                in_std.extend_from_slice(path.get(1..)?);
                cx.std_def(&in_std)
            })
        };
        let adt = |path: &[&str]| match def(path) {
            Some(hir::ModuleDef::Adt(it)) => Some(AdtId::from(it)),
            _ => None,
        };
        let function = |path: &[&str]| match def(path) {
            Some(hir::ModuleDef::Function(it)) => FunctionId::try_from(it).ok(),
            _ => None,
        };
        let trait_ = |path: &[&str]| match def(path) {
            Some(hir::ModuleDef::Trait(it)) => Some(TraitId::from(it)),
            _ => None,
        };
        let lang = lang_items(db, cx.krate().into());
        Std {
            slice_iter: adt(&["core", "slice", "Iter"]),
            slice_iter_mut: adt(&["core", "slice", "IterMut"]),
            chunks: adt(&["core", "slice", "Chunks"]),
            chunks_mut: adt(&["core", "slice", "ChunksMut"]),
            chunks_exact: adt(&["core", "slice", "ChunksExact"]),
            chunks_exact_mut: adt(&["core", "slice", "ChunksExactMut"]),
            windows: adt(&["core", "slice", "Windows"]),
            vec_into_iter: adt(&["alloc", "vec", "IntoIter"]),
            array_into_iter: adt(&["core", "array", "IntoIter"]),
            range: lang.Range.map(AdtId::from),
            range_inclusive: lang.RangeInclusiveStruct.map(AdtId::from),
            map: adt(&["core", "iter", "Map"]),
            filter: adt(&["core", "iter", "Filter"]),
            filter_map: adt(&["core", "iter", "FilterMap"]),
            enumerate: adt(&["core", "iter", "Enumerate"]),
            copied: adt(&["core", "iter", "Copied"]),
            cloned: adt(&["core", "iter", "Cloned"]),
            zip: adt(&["core", "iter", "Zip"]),
            rev: adt(&["core", "iter", "Rev"]),
            take: adt(&["core", "iter", "Take"]),
            skip: adt(&["core", "iter", "Skip"]),
            step_by: adt(&["core", "iter", "StepBy"]),
            chain: adt(&["core", "iter", "Chain"]),
            vec: adt(&["alloc", "vec", "Vec"]),
            string: adt(&["alloc", "string", "String"]),
            owned_box: lang.OwnedBox.map(AdtId::from),
            vec_deque: adt(&["alloc", "collections", "VecDeque"]),
            btree_set: adt(&["alloc", "collections", "BTreeSet"]),
            btree_map: adt(&["alloc", "collections", "BTreeMap"]),
            hash_set: adt(&["std", "collections", "HashSet"]),
            hash_map: adt(&["std", "collections", "HashMap"]),
            iterator: lang.Iterator,
            send: trait_(&["core", "marker", "Send"]),
            sync: trait_(&["core", "marker", "Sync"]).or(lang.Sync),
            index: lang.Index,
            index_mut: lang.IndexMut,
            ord: trait_(&["core", "cmp", "Ord"]),
            min: function(&["core", "cmp", "min"]),
            max: function(&["core", "cmp", "max"]),
            next: lang.IteratorNext,
        }
    }

    /// Whether `adt` is a std container that owns what it holds: `Vec`, `String`, `Box`,
    /// `VecDeque`, the B-tree and hash collections.
    pub(crate) fn owns(&self, adt: AdtId) -> bool {
        [
            self.vec,
            self.string,
            self.owned_box,
            self.vec_deque,
            self.btree_set,
            self.btree_map,
            self.hash_set,
            self.hash_map,
        ]
        .contains(&Some(adt))
    }
}

/// Where a chain's items come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// `slice::Iter` or `IterMut`.
    Slice { mutable: bool },
    /// `slice::Chunks`, `ChunksExact` and their `Mut`s.
    Chunks { mutable: bool, exact: bool },
    /// `slice::Windows`.
    Windows,
    /// `vec::IntoIter`.
    VecIntoIter,
    /// `array::IntoIter`.
    ArrayIntoIter,
    /// A `Range` or `RangeInclusive` of integers.
    Range,
}

/// An adaptor of a chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Map,
    Filter,
    FilterMap,
    Enumerate,
    Copied,
    Cloned,
    Zip,
    Rev,
    Take,
    Skip,
    StepBy,
    Chain,
}

/// A chain of std iterators, taken apart.
pub(crate) struct Chain<'db> {
    pub(crate) source: Source,
    /// The adaptors, the outermost first.
    pub(crate) steps: Vec<Step>,
    /// The closures (or functions) the adaptors call, as types.
    pub(crate) closures: Vec<Ty<'db>>,
    /// The other chains `Zip` and `Chain` take in.
    pub(crate) others: Vec<Chain<'db>>,
}

impl<'db> Chain<'db> {
    /// `ty` taken apart, if it is a chain of std iterators all the way down.
    pub(crate) fn of(std: &Std, ty: Ty<'db>) -> Option<Chain<'db>> {
        let mut steps = Vec::new();
        let mut closures = Vec::new();
        let mut others = Vec::new();
        let mut ty = ty;
        loop {
            let (adt, args) = ty.as_adt()?;
            let is = |it: Option<AdtId>| it == Some(adt);
            let mut types = args.types();
            let source = if is(std.slice_iter) || is(std.slice_iter_mut) {
                Some(Source::Slice { mutable: is(std.slice_iter_mut) })
            } else if is(std.chunks) || is(std.chunks_mut) {
                Some(Source::Chunks { mutable: is(std.chunks_mut), exact: false })
            } else if is(std.chunks_exact) || is(std.chunks_exact_mut) {
                Some(Source::Chunks { mutable: is(std.chunks_exact_mut), exact: true })
            } else if is(std.windows) {
                Some(Source::Windows)
            } else if is(std.vec_into_iter) {
                Some(Source::VecIntoIter)
            } else if is(std.array_into_iter) {
                Some(Source::ArrayIntoIter)
            } else if is(std.range) || is(std.range_inclusive) {
                let index = types.next()?;
                matches!(index.kind(), TyKind::Int(_) | TyKind::Uint(_)).then_some(Source::Range)
            } else {
                None
            };
            if let Some(source) = source {
                return Some(Chain { source, steps, closures, others });
            }
            let step = [
                (std.map, Step::Map),
                (std.filter, Step::Filter),
                (std.filter_map, Step::FilterMap),
                (std.enumerate, Step::Enumerate),
                (std.copied, Step::Copied),
                (std.cloned, Step::Cloned),
                (std.zip, Step::Zip),
                (std.rev, Step::Rev),
                (std.take, Step::Take),
                (std.skip, Step::Skip),
                (std.step_by, Step::StepBy),
                (std.chain, Step::Chain),
            ]
            .into_iter()
            .find_map(|(it, step)| is(it).then_some(step))?;
            let inner = types.next()?;
            match step {
                Step::Map | Step::Filter | Step::FilterMap => closures.push(types.next()?),
                Step::Zip | Step::Chain => others.push(Chain::of(std, types.next()?)?),
                _ => {}
            }
            steps.push(step);
            ty = inner;
        }
    }

    /// Whether two iterations' items never share memory a write through one could change:
    /// `IterMut`'s and `ChunksMut`'s, owned items, numbers; and what adaptors keep of them.
    /// A `Map` keeps them when its closure's result comes from its item alone
    /// (`returns_item`).
    pub(crate) fn disjoint(&self, returns_item: impl Fn(Ty<'db>) -> bool + Copy) -> bool {
        let source = match self.source {
            Source::Slice { mutable } | Source::Chunks { mutable, .. } => mutable,
            Source::Windows => false,
            Source::VecIntoIter | Source::ArrayIntoIter | Source::Range => true,
        };
        let mut closures = self.closures.iter();
        let mut others = self.others.iter();
        let mut disjoint = source;
        for step in self.steps.iter().rev() {
            match step {
                Step::Map | Step::FilterMap => {
                    let closure = closures.next_back();
                    disjoint &= closure.is_some_and(|it| returns_item(*it));
                }
                Step::Filter => {
                    closures.next_back();
                }
                // A `Zip`'s item is a pair: a write goes through one half, and a half from a
                // shared source can't be written through at all (its memory has no
                // `UnsafeCell`, which the item's ownership check refuses). A `Chain`'s items
                // come from either.
                Step::Zip => {
                    disjoint |= others.next_back().is_some_and(|it| it.disjoint(returns_item));
                }
                Step::Chain => {
                    disjoint &= others.next_back().is_some_and(|it| it.disjoint(returns_item));
                }
                Step::Copied | Step::Cloned => disjoint = true,
                Step::Enumerate | Step::Rev | Step::Take | Step::Skip | Step::StepBy => {}
            }
        }
        disjoint
    }

    /// Whether no two iterations get the same item: a range, through adaptors that only drop
    /// or reorder items.
    pub(crate) fn distinct(&self) -> bool {
        self.source == Source::Range
            && self.steps.iter().all(|it| {
                matches!(it, Step::Rev | Step::Take | Step::Skip | Step::StepBy | Step::Filter)
            })
    }

    /// Whether the outermost adaptor is `Enumerate`, whose item's field 0 differs between
    /// iterations.
    pub(crate) fn enumerated(&self) -> bool {
        self.steps.first() == Some(&Step::Enumerate)
    }

    /// The work of one item's trip through the adaptors, past what their closures run.
    pub(crate) fn overhead(&self) -> u64 {
        4 * (1 + self.steps.len() as u64)
            + self.others.iter().map(Chain::overhead).sum::<u64>()
    }

    /// Whether rayon has every adaptor with the same name and the rewrite is the source's
    /// alone (`Zip` and `Chain` take a parallel iterator of their own).
    pub(crate) fn rewrites_locally(&self) -> bool {
        self.others.is_empty()
    }
}

/// The name rayon has for a source method, `iter` → `par_iter`.
pub(crate) fn rayon_name(method: &str) -> Option<&'static str> {
    Some(match method {
        "iter" => "par_iter",
        "iter_mut" => "par_iter_mut",
        "into_iter" => "into_par_iter",
        "chunks" => "par_chunks",
        "chunks_mut" => "par_chunks_mut",
        "chunks_exact" => "par_chunks_exact",
        "chunks_exact_mut" => "par_chunks_exact_mut",
        "windows" => "par_windows",
        _ => return None,
    })
}

/// What the written head of a chain says: the call of its source method, the range it starts
/// from, and how many items it gives when that is written down.
#[derive(Default)]
pub(crate) struct Head {
    /// The source method's call (`xs.iter()`) and its name.
    pub(crate) call: Option<(ExprId, &'static str)>,
    /// The range the chain starts from.
    pub(crate) range: Option<ExprId>,
    /// The expression the chain starts from when it is neither: `v` of `for x in v`, `&v`.
    pub(crate) base: Option<ExprId>,
    /// How many items it gives, when the source's length and the adaptors' counts are
    /// literals or array lengths.
    pub(crate) trips: Option<u128>,
}

impl Head {
    /// The head that starts at `expr`, the receiver of a consumer or a `for` loop's iterable.
    pub(crate) fn of(db: &base_db::Db, body: &Body, infer: &InferenceResult<'_>, expr: ExprId) -> Head {
        let mut head = Head::default();
        let mut takes: Option<u128> = None;
        let mut chunk: Option<u128> = None;
        let mut at = expr;
        loop {
            match body.expr(at) {
                Expr::MethodCall { receiver, method_name, args, .. } => {
                    let name = method_name.as_str();
                    if let Some(source) = source_name(name) {
                        head.call = Some((at, source));
                        if name.starts_with("chunks") {
                            chunk = args.first().and_then(|it| literal(body, *it));
                        }
                        head.trips = array_len(db, infer.expr_ty(receiver));
                        if head.trips.is_none() && range_bounds(db, body, receiver).is_some() {
                            head.range = Some(receiver);
                            head.trips = range_len(db, body, receiver);
                        }
                        break;
                    }
                    if name == "take" {
                        let n = args.first().and_then(|it| literal(body, *it));
                        takes = match (takes, n) {
                            (Some(a), Some(b)) => Some(a.min(b)),
                            (a, b) => a.or(b),
                        };
                    }
                    at = receiver;
                }
                _ if range_bounds(db, body, at).is_some() => {
                    head.range = Some(at);
                    head.trips = range_len(db, body, at);
                    break;
                }
                _ => {
                    head.base = Some(at);
                    head.trips = array_len(db, infer.expr_ty(at));
                    break;
                }
            }
        }
        if let (Some(trips), Some(chunk)) = (head.trips, chunk)
            && chunk > 0
        {
            head.trips = Some(trips.div_ceil(chunk));
        }
        if let Some(takes) = takes {
            head.trips = Some(head.trips.map_or(takes, |it| it.min(takes)));
        }
        head
    }
}

/// The source method `method` names, if it is one rayon has a parallel form of.
fn source_name(method: &str) -> Option<&'static str> {
    const SOURCES: [&str; 8] = [
        "iter",
        "iter_mut",
        "into_iter",
        "chunks",
        "chunks_mut",
        "chunks_exact",
        "chunks_exact_mut",
        "windows",
    ];
    SOURCES.into_iter().find(|&it| it == method)
}

/// The value of an integer literal.
fn literal(body: &Body, expr: ExprId) -> Option<u128> {
    match body.expr(expr) {
        Expr::Literal(Literal::Uint(value, _)) => Some(*value),
        Expr::Literal(Literal::Int(value, _)) => u128::try_from(*value).ok(),
        _ => None,
    }
}

/// The start, the end and the kind of a range expression as it is lowered: a struct literal of
/// a range lang item (`a..b` is `Range { start: a, end: b }`), or `RangeInclusive::new(a, b)`
/// for `a..=b`. Source code cannot name a lang item, so no other expression is one of these.
fn range_bounds(db: &base_db::Db, body: &Body, expr: ExprId) -> Option<(Option<ExprId>, Option<ExprId>, RangeOp)> {
    match body.expr(expr) {
        Expr::RecordLit { path: Path::LangItem(LangItemTarget::StructId(id), _), fields, .. } => {
            let lang = lang_items(db, id.krate(db));
            let exclusive = [lang.Range, lang.RangeFrom, lang.RangeTo, lang.RangeFull, lang.RangeCopy, lang.RangeFromCopy];
            let inclusive = [lang.RangeToInclusive, lang.RangeInclusiveCopy, lang.RangeToInclusiveCopy];
            let op = if exclusive.contains(&Some(id)) {
                RangeOp::Exclusive
            } else if inclusive.contains(&Some(id)) {
                RangeOp::Inclusive
            } else {
                return None;
            };
            let field = |name| fields.iter().find(|it| *it.name.symbol() == name).map(|it| it.expr);
            Some((field(sym::start), field(sym::end).or_else(|| field(sym::last)), op))
        }
        Expr::Call { callee, args: &[start, end] } => match body.expr(callee) {
            Expr::Path(Path::LangItem(LangItemTarget::FunctionId(id), _))
                if lang_items(db, id.krate(db)).RangeInclusiveNew == Some(id) =>
            {
                Some((Some(start), Some(end), RangeOp::Inclusive))
            }
            _ => None,
        },
        _ => None,
    }
}

/// How many items a range of literals gives.
fn range_len(db: &base_db::Db, body: &Body, expr: ExprId) -> Option<u128> {
    let (Some(start), Some(end), range_type) = range_bounds(db, body, expr)? else {
        return None;
    };
    let (start, end) = (literal(body, start)?, literal(body, end)?);
    match range_type {
        RangeOp::Exclusive => Some(end.saturating_sub(start)),
        RangeOp::Inclusive => Some(end.saturating_sub(start) + u128::from(end >= start)),
    }
}

/// The length of an array type, or of the array a reference points at.
fn array_len(db: &base_db::Db, ty: Ty<'_>) -> Option<u128> {
    match ty.kind() {
        TyKind::Array(_, len) => hir_ty::consteval::try_const_usize(db, len),
        TyKind::Ref(_, inner, _) => array_len(db, inner),
        _ => None,
    }
}
