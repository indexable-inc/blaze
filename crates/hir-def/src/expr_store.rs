//! Defines `ExpressionStore`: a lowered representation of functions, statics and
//! consts.
pub mod body;
mod expander;
pub mod lower;
pub mod path;
pub mod persist;
pub mod pretty;
pub mod scope;
mod source_map;
#[cfg(test)]
mod tests;

use std::{borrow::Borrow, cell::RefCell, mem, ops::Index, sync::LazyLock};

use base_db::Db;
use blaze_hash::{HashMap, HashSet};
use blaze_ids::radix;
use cfg::CfgExpr;
use either::Either;
use hir_expand::{InFile, MacroCallId, mod_path::ModPath, name::Name};
use la_arena::Arena;
use smallvec::SmallVec;
use span::{Edition, SyntaxContext};
use syntax::{AstPtr, SyntaxNodePtr, ast};
use thin_vec::ThinVec;

use crate::{
    AdtId, BlockId, ExpressionStoreOwnerId, GenericDefId,
    expr_store::path::{
        AssociatedTypeBinding, GenericArg, GenericArgs, GenericArgsIdx, NormalPath, Path,
        PathColumns, PathIdx,
    },
    hir::{
        Array, AsmOperand, Binding, BindingId, Expr, ExprId, ExprOrPatId, ExprOrPatIdPacked,
        InlineAsm, Label, LabelId, Literal, MatchArm, OffsetOf, Pat, PatId, RecordFieldPat,
        RecordLitField, RecordSpread, Statement, Statements,
        col::{Col, SlabLayout, SlabWriter},
        columns::{self, Frozen, Growing, HirColumns},
    },
    nameres::{DefMap, block_def_map},
    signatures::VariantFields,
    type_ref::{
        ArrayType, ConstRef, FnType, LifetimeRef, LifetimeRefId, RefType, TypeBound, TypeColumns,
        TypeRef, TypeRefId, UseArgRef,
    },
};

use self::source_map::{
    BackPushes, BindingPats, ExpressionOnlySourceMap, FormatTemplate, FreezeScratch, IdTable,
    PendingTemplate, SourceTable,
};

pub(crate) use self::source_map::{Move, MoveTo};

pub use self::body::{Body, BodySourceMap};
pub use self::lower::{
    hir_assoc_type_binding_to_ast_view, hir_generic_arg_to_ast_view,
    hir_segment_to_ast_segment_view,
};
pub use self::source_map::ExpressionStoreSourceMap;

/// A wrapper around [`span::SyntaxContext`] that is intended only for comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HygieneId(span::SyntaxContext);

impl HygieneId {
    // The edition doesn't matter here, we only use this for comparisons and to lookup the macro.
    pub const ROOT: Self = Self(span::SyntaxContext::root(Edition::Edition2015));

    pub fn new(mut ctx: span::SyntaxContext) -> Self {
        // See `Name` for why we're doing that.
        ctx.remove_root_edition();
        Self(ctx)
    }

    pub(crate) fn syntax_context(self) -> SyntaxContext {
        self.0
    }

    pub(crate) fn is_root(self) -> bool {
        self.0.is_root()
    }
}

impl crate::hir::persist::Code for HygieneId {
    fn put(&self, w: &mut crate::hir::persist::Words) {
        self.0.put(w);
    }

    fn get(r: &mut crate::hir::persist::Read<'_>) -> Result<Self, blaze_format::FormatError> {
        r.get().map(HygieneId)
    }
}

pub type ExprPtr = AstPtr<ast::Expr>;
pub type ExprSource = InFile<ExprPtr>;

pub type PatPtr = AstPtr<ast::Pat>;
pub type PatSource = InFile<PatPtr>;

/// BlockExpr -> Desugared label from try block
pub type LabelPtr = AstPtr<Either<ast::Label, ast::BlockExpr>>;
pub type LabelSource = InFile<LabelPtr>;

pub type FieldPtr = AstPtr<ast::RecordExprField>;
pub type FieldSource = InFile<FieldPtr>;

pub type PatFieldPtr = AstPtr<Either<ast::RecordExprField, ast::RecordPatField>>;
pub type PatFieldSource = InFile<PatFieldPtr>;

pub type ExprOrPatPtr = AstPtr<Either<ast::Expr, ast::Pat>>;
pub type ExprOrPatSource = InFile<ExprOrPatPtr>;

pub type SelfParamPtr = AstPtr<ast::SelfParam>;
pub type MacroCallPtr = AstPtr<ast::MacroCall>;

pub type TypePtr = AstPtr<ast::Type>;
pub type TypeSource = InFile<TypePtr>;

pub type LifetimePtr = AstPtr<ast::Lifetime>;
pub type LifetimeSource = InFile<LifetimePtr>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ExprRoot {
    root: ExprId,
    // We store, for each root, the range of exprs (and pats and bindings) it holds.
    // We store only the end (exclusive), since the start can be inferred from the previous
    // roots or is zero.
    exprs_end: ExprId,
    pats_end: PatId,
    bindings_end: BindingId,
}

// We split the store into types-only and expressions, because most stores (e.g. generics)
// don't store any expressions and this saves memory. Same thing for the source map.
#[derive(Debug, PartialEq, Eq, Hash)]
struct ExpressionOnlyStore {
    /// The expressions and patterns.
    columns: HirColumns<Frozen>,
    /// [`kinds_of`] the expression kinds of `columns`.
    kinds: u64,
    bindings: Arena<Binding>,
    labels: Arena<Label>,
    /// The closure/coroutine/anon const that owns each binding, by binding: [`NO_OWNER`] for a
    /// binding the top level expression owns. Empty when it owns them all.
    binding_owners: Col<u32>,
    /// Block expressions in this store that may contain inner items.
    block_scopes: Col<BlockId>,

    /// The hygiene of variable usages whose hygiene is not the root's, by expression or pattern
    /// in increasing order.
    ///
    /// Expressions (and destructuring patterns) that can be recorded here are single segment path, although not all single segments path refer
    /// to variables and have hygiene (some refer to items, we don't know at this stage).
    ident_hygiene: Col<(ExprOrPatIdPacked, HygieneId)>,

    /// The edition of each block that a macro wrote, by block in increasing order: it is the
    /// macro's, not the one of the crate that uses the macro.
    block_editions: Col<(ExprId, Edition)>,

    /// The closures written with `#[rustc_inherit_overflow_checks]`, in increasing order.
    overflow_inheriting: Col<ExprId>,

    /// Maps expression roots to their origin.
    ///
    /// Note: while every root expr is an inference root (aka. an `AnonConst`), there could be other roots that do not appear here.
    /// This can happen when anon consts are nested, for example:
    ///
    /// ```
    /// [
    ///     ();
    ///     {
    ///         // this repeat expr is anon const #1, and *only it* appears in this list.
    ///         [
    ///             ();
    ///             {
    ///                 // this repeat expr is anon const #2.
    ///                 0
    ///             }
    ///         ];
    ///         0
    ///     }
    /// ]
    /// ```
    /// We do this because this allows us to search this list using a binary search,
    /// and it does not bother us because we use this list for two things: constructing `ExprScopes`, which
    /// works fine with nested exprs, and retrieving inference results, and we copy the inner const's inference
    /// into the outer const.
    expr_roots: SmallVec<[ExprRoot; 1]>,
}

/// The owner in [`ExpressionOnlyStore::binding_owners`] of a binding the top level owns.
const NO_OWNER: u32 = u32::MAX;

const _: () = assert!((columns::ExprKind::UnsafeBinderCast as u32) < u64::BITS);

/// A bit per kind byte of `expr_kind` (each an [`columns::ExprKind`], which the columns are
/// checked to hold before a store is made of them), in variant order.
fn kinds_of(expr_kind: &[u8]) -> u64 {
    expr_kind.iter().fold(0, |kinds, &kind| kinds | 1 << kind)
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct ExpressionStore {
    expr_only: Option<Box<ExpressionOnlyStore>>,
    /// The type refs ([`ExpressionStore::type_ref`]).
    pub(crate) types: TypeColumns<Frozen>,
    pub lifetimes: Arena<LifetimeRef>,
    /// The hygiene of the plain type paths a `macro` wrote, by type in increasing order.
    type_hygiene: Option<Box<[(TypeRefId, HygieneId)]>>,
    /// [`ExpressionStore::macro_calls`]: here rather than in `expr_only`, as a signature's
    /// type macros make a store with calls and no expressions.
    macro_calls: ThinVec<MacroCallId>,
}

/// The body of an item (function, const etc.) while it is lowered. Its columns, forward maps,
/// back maps and list stacks are a [`Scratch`] taken from this thread's pool by
/// [`ExpressionStoreBuilder::new`] and given back, emptied, by [`ExpressionStoreBuilder::finish`].
#[derive(Debug, Eq, PartialEq)]
pub struct ExpressionStoreBuilder {
    pub(crate) columns: HirColumns<Growing>,
    pub(crate) types: TypeColumns<Growing>,
    /// The stacks lowering collects a node's children on ([`ExpressionStoreBuilder::list_mark`]).
    lists: Lists,
    pub bindings: Arena<Binding>,
    pub labels: Arena<Label>,
    pub lifetimes: Arena<LifetimeRef>,
    pub binding_owners: HashMap<BindingId, ExprId>,
    block_scopes: Vec<BlockId>,
    ident_hygiene: HashMap<ExprOrPatIdPacked, HygieneId>,
    pub(crate) type_hygiene: Vec<(TypeRefId, HygieneId)>,
    block_editions: HashMap<ExprId, Edition>,

    /// The closures written with `#[rustc_inherit_overflow_checks]`.
    overflow_inheriting: HashSet<ExprId>,
    inference_roots: Option<SmallVec<[ExprRoot; 1]>>,

    // AST expressions can create patterns in destructuring assignments. Therefore, `ExprSource` can also map
    // to `PatId`, and `PatId` can also map to `ExprSource` (the other way around is unaffected).
    // The forward maps are pushed to in lowering order and never read while lowering; `finish`
    // sorts each into a `SourceTable`, a later push for a pointer replacing an earlier one. The
    // back maps are pushed to in lowering order too, and `finish` makes each the dense columns of
    // a `BackMap`, a later push for an id replacing an earlier one.
    expr_map: Vec<(ExprSource, ExprOrPatIdPacked)>,
    expr_map_back: BackPushes<Expr<'static>, Either<ast::Expr, ast::Pat>>,

    pat_map: Vec<(PatSource, ExprOrPatIdPacked)>,
    pat_map_back: BackPushes<Pat<'static>, Either<ast::Expr, ast::Pat>>,

    label_map: Vec<(LabelSource, LabelId)>,
    label_map_back: BackPushes<Label, Either<ast::Label, ast::BlockExpr>>,

    types_map_back: BackPushes<TypeRef<'static>, ast::Type>,
    types_map: Vec<(TypeSource, TypeRefId)>,

    lifetime_map_back: BackPushes<LifetimeRef, ast::Lifetime>,
    lifetime_map: Vec<(LifetimeSource, LifetimeRefId)>,

    /// Each binding and a pattern that defines it, in lowering order: `finish` makes them CSR.
    binding_definitions: Vec<(BindingId, PatId)>,

    /// We don't create explicit nodes for record fields (`S { record_field: 92 }`).
    /// Instead, we use id of expression (`92`) to identify the field.
    field_map_back: Vec<(ExprId, FieldSource)>,
    pat_field_map_back: Vec<(PatId, PatFieldSource)>,

    template_map: Option<Box<PendingTemplate>>,

    expansions: Vec<(InFile<MacroCallPtr>, MacroCallId)>,
    /// Each macro call lowered into the store, in lowering order, recorded with or without the
    /// source map.
    macro_calls: Vec<MacroCallId>,

    /// Diagnostics accumulated during lowering. These contain `AstPtr`s and so are stored in
    /// the source map (since they're just as volatile).
    //
    // We store diagnostics on the `ExpressionOnlySourceMap` because diagnostics are rare (except
    // maybe for cfgs, and they are also not common in type places).
    pub(crate) diagnostics: Vec<ExpressionStoreDiagnostics>,

    /// What `finish` builds the back maps in, empty until then.
    freeze: FreezeScratch,
    /// What `finish` gathers and sorts the store's side columns in, empty until then.
    packing: Packing,

    /// Whether lowering records the source map. Without it lowering pushes to none of the maps
    /// above, and `finish` gives an empty source map: the store is the same either way, as
    /// lowering reads the maps only to fill them.
    record: bool,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum MissingBodyItemKind {
    AssocConst,
    AssocType,
    Const,
    Static,
    TypeAlias,
}

#[derive(Debug, Eq, PartialEq, Hash)]
pub enum ExpressionStoreDiagnostics {
    InactiveCode { node: InFile<SyntaxNodePtr>, cfg: CfgExpr },
    UnresolvedMacroCall { node: InFile<MacroCallPtr>, path: ModPath },
    UnreachableLabel { node: InFile<AstPtr<ast::Lifetime>>, name: Name },
    AwaitOutsideOfAsync { node: InFile<AstPtr<ast::AwaitExpr>>, location: String },
    UndeclaredLabel { node: InFile<AstPtr<ast::Lifetime>>, name: Name },
    PatternArgInExternFn { node: InFile<AstPtr<ast::Pat>> },
    FruInDestructuringAssignment { node: InFile<AstPtr<ast::Expr>> },
    MissingBody { node: InFile<SyntaxNodePtr>, kind: MissingBodyItemKind },
    /// An integer literal whose digits do not fit `u128`, lowered as `0`: rustc's
    /// `LitError::IntTooLarge`, reported by `rustc_ast_lowering`'s `lower_lit` (so only for
    /// code cfg keeps), whose `LitKind::Err` taints the body.
    IntLiteralTooLarge { node: InFile<AstPtr<ast::Literal>> },
}

/// The stacks lowering collects a node's children on before it allocates the node, one per
/// element type of a list. They are used in stack order: a nested node's children go above its
/// parent's and are truncated away before the parent is allocated, so no list needs a `Vec`,
/// `SmallVec` or `Box<[_]>` of its own.
///
/// To allocate a node from the lists above a mark, lowering moves the stacks out of the builder
/// ([`ExpressionStoreBuilder::take_lists`], a move of a few `Vec` headers), builds the view from
/// `lists.exprs_since(mark)` and the like, allocates it, and puts them back truncated to the mark
/// ([`ExpressionStoreBuilder::put_lists`]): borrowing them in place would hold the builder
/// shared while it allocates.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Lists {
    exprs: Vec<ExprId>,
    pats: Vec<PatId>,
    types: Vec<TypeRefId>,
    opt_types: Vec<Option<TypeRefId>>,
    /// Statement rows, as a store keeps them: only the statement columns, their `extra` words
    /// and macro definitions are used. A [`Statement`] view borrows its macro definition, so
    /// the stack keeps rows that own it.
    statements: HirColumns<Growing>,
    arms: Vec<MatchArm>,
    record_fields: Vec<RecordLitField>,
    field_pats: Vec<RecordFieldPat>,
    bounds: Vec<TypeBound>,
}

/// How high each list stack stood: [`ExpressionStoreBuilder::list_mark`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListMark {
    exprs: u32,
    pats: u32,
    types: u32,
    opt_types: u32,
    statements: u32,
    statement_words: u32,
    macro_defs: u32,
    arms: u32,
    record_fields: u32,
    field_pats: u32,
    bounds: u32,
}

// Each `*_since` is the elements of one stack pushed since `mark`. It panics when the stack is
// below the mark: lowering truncated past a mark it did not own.
impl Lists {
    #[inline]
    pub(crate) fn exprs_since(&self, mark: ListMark) -> &[ExprId] {
        &self.exprs[mark.exprs as usize..]
    }

    /// The expressions pushed since `mark`, to reorder or rewrite in place.
    #[inline]
    pub(crate) fn exprs_since_mut(&mut self, mark: ListMark) -> &mut [ExprId] {
        &mut self.exprs[mark.exprs as usize..]
    }

    #[inline]
    pub(crate) fn pats_since(&self, mark: ListMark) -> &[PatId] {
        &self.pats[mark.pats as usize..]
    }

    #[inline]
    pub(crate) fn pats_since_mut(&mut self, mark: ListMark) -> &mut [PatId] {
        &mut self.pats[mark.pats as usize..]
    }

    /// The elements of a `TypeRef::Tuple`.
    #[inline]
    pub(crate) fn types_since(&self, mark: ListMark) -> &[TypeRefId] {
        &self.types[mark.types as usize..]
    }

    /// A closure's argument types.
    #[inline]
    pub(crate) fn opt_types_since(&self, mark: ListMark) -> &[Option<TypeRefId>] {
        &self.opt_types[mark.opt_types as usize..]
    }

    #[inline]
    pub(crate) fn statements_since(&self, mark: ListMark) -> Statements<'_> {
        self.statements.statements_since(mark.statements)
    }

    #[inline]
    pub(crate) fn arms_since(&self, mark: ListMark) -> &[MatchArm] {
        &self.arms[mark.arms as usize..]
    }

    #[inline]
    pub(crate) fn record_fields_since(&self, mark: ListMark) -> &[RecordLitField] {
        &self.record_fields[mark.record_fields as usize..]
    }

    #[inline]
    pub(crate) fn field_pats_since(&self, mark: ListMark) -> &[RecordFieldPat] {
        &self.field_pats[mark.field_pats as usize..]
    }

    /// The bounds of a `TypeRef::ImplTrait` or `DynTrait`.
    #[inline]
    pub(crate) fn bounds_since(&self, mark: ListMark) -> &[TypeBound] {
        &self.bounds[mark.bounds as usize..]
    }

    fn mark(&self) -> ListMark {
        ListMark {
            exprs: columns::len(self.exprs.len()),
            pats: columns::len(self.pats.len()),
            types: columns::len(self.types.len()),
            opt_types: columns::len(self.opt_types.len()),
            statements: columns::len(self.statements.statement_count()),
            statement_words: columns::len(self.statements.extra.len()),
            macro_defs: columns::len(self.statements.macro_defs.len()),
            arms: columns::len(self.arms.len()),
            record_fields: columns::len(self.record_fields.len()),
            field_pats: columns::len(self.field_pats.len()),
            bounds: columns::len(self.bounds.len()),
        }
    }

    fn truncate(&mut self, mark: ListMark) {
        self.exprs.truncate(mark.exprs as usize);
        self.pats.truncate(mark.pats as usize);
        self.types.truncate(mark.types as usize);
        self.opt_types.truncate(mark.opt_types as usize);
        self.statements.truncate_statements(mark.statements, mark.statement_words, mark.macro_defs);
        self.arms.truncate(mark.arms as usize);
        self.record_fields.truncate(mark.record_fields as usize);
        self.field_pats.truncate(mark.field_pats as usize);
        self.bounds.truncate(mark.bounds as usize);
    }

    fn clear(&mut self) {
        let Lists {
            exprs,
            pats,
            types,
            opt_types,
            statements,
            arms,
            record_fields,
            field_pats,
            bounds,
        } = self;
        exprs.clear();
        pats.clear();
        types.clear();
        opt_types.clear();
        statements.clear();
        arms.clear();
        record_fields.clear();
        field_pats.clear();
        bounds.clear();
    }

    /// The first stack that is not empty, by name: a builder finishes with every one empty, or
    /// the ids left on it would land in the next store's lists.
    fn stacked(&self) -> Option<&'static str> {
        let Lists {
            exprs,
            pats,
            types,
            opt_types,
            statements,
            arms,
            record_fields,
            field_pats,
            bounds,
        } = self;
        [
            (exprs.is_empty(), "exprs"),
            (pats.is_empty(), "pats"),
            (types.is_empty(), "types"),
            (opt_types.is_empty(), "opt_types"),
            (statements.statement_count() == 0, "statements"),
            (arms.is_empty(), "arms"),
            (record_fields.is_empty(), "record_fields"),
            (field_pats.is_empty(), "field_pats"),
            (bounds.is_empty(), "bounds"),
        ]
        .into_iter()
        .find_map(|(empty, name)| (!empty).then_some(name))
    }

    fn capacity(&self) -> usize {
        self.exprs.capacity()
            + self.pats.capacity()
            + self.types.capacity()
            + self.opt_types.capacity()
            + self.statements.capacity()
            + self.statements.stmt_data.capacity()
            + self.statements.macro_defs.capacity()
            + self.arms.capacity()
            + self.record_fields.capacity()
            + self.field_pats.capacity()
            + self.bounds.capacity()
    }
}

/// What a builder grows while it lowers a store, kept per thread between stores
/// ([`Scratch::take`], [`Scratch::give`]): a thread's scratch grows to its largest body once,
/// and after that lowering a body allocates nothing per node, only the exact columns of the
/// finished store.
#[derive(Default)]
struct Scratch {
    columns: HirColumns<Growing>,
    types: TypeColumns<Growing>,
    lists: Lists,
    bindings: Arena<Binding>,
    labels: Arena<Label>,
    lifetimes: Arena<LifetimeRef>,
    binding_owners: HashMap<BindingId, ExprId>,
    block_scopes: Vec<BlockId>,
    ident_hygiene: HashMap<ExprOrPatIdPacked, HygieneId>,
    type_hygiene: Vec<(TypeRefId, HygieneId)>,
    block_editions: HashMap<ExprId, Edition>,
    overflow_inheriting: HashSet<ExprId>,
    expr_map: Vec<(ExprSource, ExprOrPatIdPacked)>,
    pat_map: Vec<(PatSource, ExprOrPatIdPacked)>,
    label_map: Vec<(LabelSource, LabelId)>,
    types_map: Vec<(TypeSource, TypeRefId)>,
    lifetime_map: Vec<(LifetimeSource, LifetimeRefId)>,
    expansions: Vec<(InFile<MacroCallPtr>, MacroCallId)>,
    macro_calls: Vec<MacroCallId>,
    expr_map_back: BackPushes<Expr<'static>, Either<ast::Expr, ast::Pat>>,
    pat_map_back: BackPushes<Pat<'static>, Either<ast::Expr, ast::Pat>>,
    label_map_back: BackPushes<Label, Either<ast::Label, ast::BlockExpr>>,
    types_map_back: BackPushes<TypeRef<'static>, ast::Type>,
    lifetime_map_back: BackPushes<LifetimeRef, ast::Lifetime>,
    binding_definitions: Vec<(BindingId, PatId)>,
    field_map_back: Vec<(ExprId, FieldSource)>,
    pat_field_map_back: Vec<(PatId, PatFieldSource)>,
    /// What [`ExpressionStoreBuilder::finish`] builds the back maps and sorts the forward maps
    /// in.
    freeze: FreezeScratch,
    packing: Packing,
}

/// What [`ExpressionStoreBuilder::finish`] gathers the store's side columns in, and sorts them
/// in, before it copies them into their slab.
#[derive(Debug, Default, PartialEq, Eq)]
struct Packing {
    owners: Vec<u32>,
    hygiene: Vec<(ExprOrPatIdPacked, HygieneId)>,
    editions: Vec<(ExprId, Edition)>,
    inheriting: Vec<ExprId>,
    /// The radix sorts' buffers.
    editions_sort: Vec<(ExprId, Edition)>,
    inheriting_sort: Vec<ExprId>,
}

impl Packing {
    fn clear(&mut self) {
        let Packing { owners, hygiene, editions, inheriting, editions_sort, inheriting_sort } =
            self;
        owners.clear();
        hygiene.clear();
        editions.clear();
        inheriting.clear();
        editions_sort.clear();
        inheriting_sort.clear();
    }

    fn capacity(&self) -> usize {
        self.owners.capacity()
            + self.hygiene.capacity()
            + self.editions.capacity()
            + self.inheriting.capacity()
            + self.editions_sort.capacity()
            + self.inheriting_sort.capacity()
    }
}

/// An exact copy of a pooled arena: one allocation of its length, none when it is empty.
fn exact_arena<T: Clone>(pooled: &Arena<T>) -> Arena<T> {
    let mut arena = Arena::with_capacity(pooled.len());
    arena.alloc_many(pooled.values().cloned());
    arena
}

/// The scratch sets a thread keeps, and the elements above which a set is dropped instead of
/// kept, so that one huge body doesn't pin its memory.
const POOL_SETS: usize = 4;
const POOL_ELEMENTS: usize = 1 << 20;

thread_local! {
    /// This thread's spare scratch, a stack: lowering one store can run a query that lowers
    /// another on the same thread, which takes the next set (or a fresh one) and gives it back
    /// before the outer one finishes. Only this thread touches it: no lock, no shared write.
    // alloc: const-empty, once per thread.
    static POOL: RefCell<Vec<Scratch>> = const { RefCell::new(Vec::new()) };
}

impl Scratch {
    fn take() -> Scratch {
        POOL.try_with(|pool| pool.borrow_mut().pop()).ok().flatten().unwrap_or_default()
    }

    /// Empties `self` and keeps it for the next builder on this thread, unless the pool is
    /// full, `self` is over [`POOL_ELEMENTS`] or the thread is exiting.
    fn give(mut self) {
        // An `Arena` reports only its length, so the size is measured before clearing.
        let held = self.capacity();
        self.clear();
        if held > POOL_ELEMENTS {
            return;
        }
        let _exiting = POOL.try_with(move |pool| {
            let mut pool = pool.borrow_mut();
            if pool.len() < POOL_SETS {
                pool.push(self);
            }
        });
    }

    fn clear(&mut self) {
        let Scratch {
            columns,
            types,
            lists,
            bindings,
            labels,
            lifetimes,
            binding_owners,
            block_scopes,
            ident_hygiene,
            type_hygiene,
            block_editions,
            overflow_inheriting,
            expr_map,
            pat_map,
            label_map,
            types_map,
            lifetime_map,
            expansions,
            macro_calls,
            expr_map_back,
            pat_map_back,
            label_map_back,
            types_map_back,
            lifetime_map_back,
            binding_definitions,
            field_map_back,
            pat_field_map_back,
            freeze,
            packing,
        } = self;
        columns.clear();
        types.clear();
        lists.clear();
        bindings.clear();
        labels.clear();
        lifetimes.clear();
        binding_owners.clear();
        block_scopes.clear();
        ident_hygiene.clear();
        type_hygiene.clear();
        block_editions.clear();
        overflow_inheriting.clear();
        expr_map.clear();
        pat_map.clear();
        label_map.clear();
        types_map.clear();
        lifetime_map.clear();
        expansions.clear();
        macro_calls.clear();
        expr_map_back.clear();
        pat_map_back.clear();
        label_map_back.clear();
        types_map_back.clear();
        lifetime_map_back.clear();
        binding_definitions.clear();
        field_map_back.clear();
        pat_field_map_back.clear();
        freeze.clear();
        packing.clear();
    }

    fn capacity(&self) -> usize {
        self.columns.capacity()
            + self.types.capacity()
            + self.lists.capacity()
            + self.bindings.len()
            + self.labels.len()
            + self.lifetimes.len()
            + self.binding_owners.capacity()
            + self.block_scopes.capacity()
            + self.ident_hygiene.capacity()
            + self.type_hygiene.capacity()
            + self.block_editions.capacity()
            + self.overflow_inheriting.capacity()
            + self.expr_map.capacity()
            + self.pat_map.capacity()
            + self.label_map.capacity()
            + self.types_map.capacity()
            + self.lifetime_map.capacity()
            + self.expansions.capacity()
            + self.macro_calls.capacity()
            + self.expr_map_back.capacity()
            + self.pat_map_back.capacity()
            + self.label_map_back.capacity()
            + self.types_map_back.capacity()
            + self.lifetime_map_back.capacity()
            + self.binding_definitions.capacity()
            + self.field_map_back.capacity()
            + self.pat_field_map_back.capacity()
            + self.freeze.capacity()
            + self.packing.capacity()
    }
}

impl ExpressionStoreBuilder {
    /// A builder on this thread's pooled scratch.
    pub(crate) fn new() -> Self {
        let Scratch {
            columns,
            types,
            lists,
            bindings,
            labels,
            lifetimes,
            binding_owners,
            block_scopes,
            ident_hygiene,
            type_hygiene,
            block_editions,
            overflow_inheriting,
            expr_map,
            pat_map,
            label_map,
            types_map,
            lifetime_map,
            expansions,
            macro_calls,
            expr_map_back,
            pat_map_back,
            label_map_back,
            types_map_back,
            lifetime_map_back,
            binding_definitions,
            field_map_back,
            pat_field_map_back,
            freeze,
            packing,
        } = Scratch::take();
        ExpressionStoreBuilder {
            columns,
            types,
            lists,
            bindings,
            labels,
            lifetimes,
            binding_owners,
            block_scopes,
            ident_hygiene,
            type_hygiene,
            block_editions,
            overflow_inheriting,
            inference_roots: None,
            expr_map,
            expr_map_back,
            pat_map,
            pat_map_back,
            label_map,
            label_map_back,
            types_map_back,
            types_map,
            lifetime_map_back,
            lifetime_map,
            binding_definitions,
            field_map_back,
            pat_field_map_back,
            template_map: None,
            expansions,
            macro_calls,
            // alloc: empty unless lowering reports one, the error path.
            diagnostics: Vec::new(),
            freeze,
            packing,
            record: true,
        }
    }

    /// The pooled tables, moved out and left empty: what `finish` reads and gives back, and what
    /// a builder dropped unfinished gives back.
    fn take_scratch(&mut self) -> Scratch {
        Scratch {
            columns: mem::take(&mut self.columns),
            types: mem::take(&mut self.types),
            lists: mem::take(&mut self.lists),
            bindings: mem::take(&mut self.bindings),
            labels: mem::take(&mut self.labels),
            lifetimes: mem::take(&mut self.lifetimes),
            binding_owners: mem::take(&mut self.binding_owners),
            block_scopes: mem::take(&mut self.block_scopes),
            ident_hygiene: mem::take(&mut self.ident_hygiene),
            type_hygiene: mem::take(&mut self.type_hygiene),
            block_editions: mem::take(&mut self.block_editions),
            overflow_inheriting: mem::take(&mut self.overflow_inheriting),
            expr_map: mem::take(&mut self.expr_map),
            pat_map: mem::take(&mut self.pat_map),
            label_map: mem::take(&mut self.label_map),
            types_map: mem::take(&mut self.types_map),
            lifetime_map: mem::take(&mut self.lifetime_map),
            expansions: mem::take(&mut self.expansions),
            macro_calls: mem::take(&mut self.macro_calls),
            expr_map_back: mem::take(&mut self.expr_map_back),
            pat_map_back: mem::take(&mut self.pat_map_back),
            label_map_back: mem::take(&mut self.label_map_back),
            types_map_back: mem::take(&mut self.types_map_back),
            lifetime_map_back: mem::take(&mut self.lifetime_map_back),
            binding_definitions: mem::take(&mut self.binding_definitions),
            field_map_back: mem::take(&mut self.field_map_back),
            pat_field_map_back: mem::take(&mut self.pat_field_map_back),
            freeze: mem::take(&mut self.freeze),
            packing: mem::take(&mut self.packing),
        }
    }

    /// Lowers without recording the source map (see `record`).
    pub(crate) fn skip_source_map(&mut self) {
        self.record = false;
    }

    #[inline]
    pub(crate) fn alloc_type_ref(&mut self, type_ref: TypeRef<'_>) -> TypeRefId {
        self.types.push(type_ref)
    }

    /// A `TypeRef::Path` of the path `path` lowering pushed, where [`Self::alloc_type_ref`]
    /// copies a view's rows.
    #[inline]
    pub(crate) fn alloc_path_type(&mut self, path: PathIdx) -> TypeRefId {
        self.types.push_path(path)
    }

    /// A `TypeRef::Fn`, `fn_type` moved into its table.
    #[inline]
    pub(crate) fn alloc_fn_type(&mut self, fn_type: FnType) -> TypeRefId {
        self.types.push_fn(fn_type)
    }

    #[inline]
    pub(crate) fn type_ref(&self, id: TypeRefId) -> TypeRef<'_> {
        self.types.get(id)
    }

    #[inline]
    pub(crate) fn type_ref_count(&self) -> usize {
        self.types.len()
    }

    #[inline]
    pub(crate) fn list_mark(&self) -> ListMark {
        self.lists.mark()
    }

    #[inline]
    pub(crate) fn push_list_expr(&mut self, expr: ExprId) {
        self.lists.exprs.push(expr);
    }

    #[inline]
    pub(crate) fn push_list_pat(&mut self, pat: PatId) {
        self.lists.pats.push(pat);
    }

    #[inline]
    pub(crate) fn push_list_type(&mut self, type_ref: TypeRefId) {
        self.lists.types.push(type_ref);
    }

    #[inline]
    pub(crate) fn push_list_opt_type(&mut self, type_ref: Option<TypeRefId>) {
        self.lists.opt_types.push(type_ref);
    }

    /// Pushes a statement as a row of the stack: its ids and words copied, its macro definition
    /// copied into the stack's table.
    #[inline]
    pub(crate) fn push_list_statement(&mut self, statement: Statement<'_>) {
        self.lists.statements.push_statement(statement);
    }

    #[inline]
    pub(crate) fn push_list_arm(&mut self, arm: MatchArm) {
        self.lists.arms.push(arm);
    }

    #[inline]
    pub(crate) fn push_list_record_field(&mut self, field: RecordLitField) {
        self.lists.record_fields.push(field);
    }

    #[inline]
    pub(crate) fn push_list_field_pat(&mut self, field: RecordFieldPat) {
        self.lists.field_pats.push(field);
    }

    #[inline]
    pub(crate) fn push_list_bound(&mut self, bound: TypeBound) {
        self.lists.bounds.push(bound);
    }

    // For code that only reads its lists; to allocate a node from them, see `take_lists`.

    #[inline]
    pub(crate) fn exprs_since(&self, mark: ListMark) -> &[ExprId] {
        self.lists.exprs_since(mark)
    }

    #[inline]
    pub(crate) fn pats_since(&self, mark: ListMark) -> &[PatId] {
        self.lists.pats_since(mark)
    }

    #[inline]
    pub(crate) fn types_since(&self, mark: ListMark) -> &[TypeRefId] {
        self.lists.types_since(mark)
    }

    /// The list stacks, moved out so a node can be allocated from them ([`Lists`]). The builder
    /// holds empty stacks (no allocation) until [`Self::put_lists`] returns them; what is pushed
    /// onto the builder meanwhile is lost.
    #[inline]
    pub(crate) fn take_lists(&mut self) -> Lists {
        mem::take(&mut self.lists)
    }

    /// Puts back the stacks [`Self::take_lists`] took, truncated to `mark`.
    #[inline]
    pub(crate) fn put_lists(&mut self, mut lists: Lists, mark: ListMark) {
        debug_assert_eq!(self.lists.stacked(), None, "pushed onto this stack while taken");
        lists.truncate(mark);
        self.lists = lists;
    }

    /// The last statement pushed since `mark`, taken off the stack when it is an expression
    /// without a `;`: the tail of the block the statements are of. `None`, and the stack left
    /// as it is, otherwise.
    #[inline]
    pub(crate) fn pop_list_tail_expr(&mut self, mark: ListMark) -> Option<ExprId> {
        self.lists.statements.pop_tail_expr(mark.statements)
    }

    /// The bounds pushed since `mark`, moved off the stack.
    #[inline]
    pub(crate) fn drain_bounds(&mut self, mark: ListMark) -> std::vec::Drain<'_, TypeBound> {
        self.lists.bounds.drain(mark.bounds as usize..)
    }

    /// Pops every stack back to `mark`.
    #[inline]
    pub(crate) fn truncate_to(&mut self, mark: ListMark) {
        self.lists.truncate(mark);
    }

    // The by-value allocations below move their cold values (names, literals) into the store
    // where building the view would clone them, and take the paths and generic arguments
    // lowering pushed into the store's path columns by index. Their lists are copied into the
    // store's arrays, as `alloc_expr` copies a view's.

    #[inline]
    pub(crate) fn alloc_path_expr(&mut self, path: PathIdx) -> ExprId {
        self.columns.push_path_expr(path)
    }

    #[inline]
    pub(crate) fn alloc_path_pat(&mut self, path: PathIdx) -> PatId {
        self.columns.push_path_pat(path)
    }

    #[inline]
    pub(crate) fn alloc_literal_expr(&mut self, literal: Literal) -> ExprId {
        self.columns.push_literal_expr(literal)
    }

    #[inline]
    pub(crate) fn alloc_offset_of_expr(&mut self, offset_of: OffsetOf) -> ExprId {
        self.columns.push_offset_of_expr(offset_of)
    }

    #[inline]
    pub(crate) fn alloc_asm_expr(&mut self, asm: InlineAsm) -> ExprId {
        self.columns.push_asm_expr(asm)
    }

    #[inline]
    pub(crate) fn alloc_method_call(
        &mut self,
        receiver: ExprId,
        method_name: Name,
        args: &[ExprId],
        generic_args: Option<GenericArgsIdx>,
    ) -> ExprId {
        self.columns.push_method_call(receiver, method_name, args, generic_args)
    }

    #[inline]
    pub(crate) fn alloc_record_lit(
        &mut self,
        path: PathIdx,
        fields: &[RecordLitField],
        spread: RecordSpread,
    ) -> ExprId {
        self.columns.push_record_lit(path, fields.iter().cloned(), spread)
    }

    #[inline]
    pub(crate) fn alloc_record_pat(
        &mut self,
        path: PathIdx,
        args: &[RecordFieldPat],
        ellipsis: bool,
    ) -> PatId {
        self.columns.push_record_pat(path, args.iter().cloned(), ellipsis)
    }

    #[inline]
    pub(crate) fn alloc_tuple_struct_pat(
        &mut self,
        path: PathIdx,
        args: &[PatId],
        ellipsis: Option<u32>,
    ) -> PatId {
        self.columns.push_tuple_struct_pat(path, args, ellipsis)
    }

    /// Gives the block `block` the tail `tail`, in place. Panics when `block` is not a block.
    #[inline]
    pub(crate) fn set_block_tail(&mut self, block: ExprId, tail: ExprId) {
        self.columns.set_block_tail(block, tail)
    }

    #[inline]
    pub(crate) fn alloc_expr(&mut self, expr: Expr<'_>) -> ExprId {
        self.columns.push_expr(&mut self.types.paths, expr)
    }

    #[inline]
    pub(crate) fn alloc_pat(&mut self, pat: Pat<'_>) -> PatId {
        self.columns.push_pat(&mut self.types.paths, pat)
    }

    #[inline]
    pub(crate) fn expr(&self, id: ExprId) -> Expr<'_> {
        self.columns.expr(&self.types.paths, id)
    }

    #[inline]
    pub(crate) fn pat(&self, id: PatId) -> Pat<'_> {
        self.columns.pat(&self.types.paths, id)
    }

    /// Replaces an expression: read it with [`Self::expr`], build the changed view, set it.
    #[inline]
    pub(crate) fn set_expr(&mut self, id: ExprId, expr: Expr<'_>) {
        self.columns.set_expr(&mut self.types.paths, id, expr)
    }

    #[inline]
    pub(crate) fn set_pat(&mut self, id: PatId, pat: Pat<'_>) {
        self.columns.set_pat(&mut self.types.paths, id, pat)
    }

    #[inline]
    pub(crate) fn expr_count(&self) -> usize {
        self.columns.expr_count()
    }

    #[inline]
    pub(crate) fn pat_count(&self) -> usize {
        self.columns.pat_count()
    }

    /// The finished store and source map: each column, table and map copied out of the pooled
    /// scratch at its exact length, the store's `Copy` columns into slabs, the scratch given
    /// back to this thread's pool. The gathering and sorting run in the pooled scratch too, so
    /// the copies are the only allocations.
    pub fn finish(mut self) -> (ExpressionStore, ExpressionStoreSourceMap) {
        debug_assert_eq!(self.lists.stacked(), None, "lowering left a list on this stack");
        let mut expr_roots = self.inference_roots.take();
        let template_map = self.template_map.take();
        let diagnostics = mem::take(&mut self.diagnostics);
        let record = self.record;
        let mut scratch = self.take_scratch();
        let columns = scratch.columns.freeze_into();
        let types = scratch.types.freeze_into();
        let has_exprs =
            !columns.is_empty() || !scratch.labels.is_empty() || !scratch.bindings.is_empty();
        let source_map = match record {
            false => ExpressionStoreSourceMap::default(),
            true => {
                // Each back map is dense over its arena, so a source map can be checked on its
                // own.
                let freeze = &mut scratch.freeze;
                let expr_map_back = scratch.expr_map_back.freeze(columns.expr_count(), freeze);
                let pat_map_back = scratch.pat_map_back.freeze(columns.pat_count(), freeze);
                let label_map_back = scratch.label_map_back.freeze(scratch.labels.len(), freeze);
                let types_map_back = scratch.types_map_back.freeze(types.len(), freeze);
                let lifetime_map_back =
                    scratch.lifetime_map_back.freeze(scratch.lifetimes.len(), freeze);
                let binding_definitions = BindingPats::from_pushed(
                    &mut scratch.binding_definitions,
                    scratch.bindings.len(),
                    &mut freeze.cursors,
                );
                let sort = &mut freeze.sort;
                let expr_map = SourceTable::from_pushed_in(&mut scratch.expr_map, &mut sort.exprs);
                let pat_map = SourceTable::from_pushed_in(&mut scratch.pat_map, &mut sort.pats);
                let label_map =
                    SourceTable::from_pushed_in(&mut scratch.label_map, &mut sort.labels);
                let types_map =
                    SourceTable::from_pushed_in(&mut scratch.types_map, &mut sort.types);
                let lifetime_map =
                    SourceTable::from_pushed_in(&mut scratch.lifetime_map, &mut sort.lifetimes);
                let expansions =
                    SourceTable::from_pushed_in(&mut scratch.expansions, &mut sort.expansions);
                let field_map_back =
                    IdTable::from_pushed_in(&mut scratch.field_map_back, &mut sort.fields);
                let pat_field_map_back =
                    IdTable::from_pushed_in(&mut scratch.pat_field_map_back, &mut sort.field_pats);
                let template_map = template_map.map(|template| {
                    let FormatTemplate {
                        format_args_to_captures,
                        asm_to_captures,
                        implicit_capture_to_source,
                    } = *template;
                    // alloc: once per store with format templates, the cold side of the map.
                    Box::new(FormatTemplate {
                        format_args_to_captures: IdTable::from_pushed(format_args_to_captures),
                        asm_to_captures: IdTable::from_pushed(asm_to_captures),
                        implicit_capture_to_source: IdTable::from_pushed(
                            implicit_capture_to_source,
                        ),
                    })
                });
                let expr_only = (has_exprs || !expansions.is_empty() || !diagnostics.is_empty())
                    .then(|| {
                        // alloc: once per store, keeps the map small for signatures.
                        Box::new(ExpressionOnlySourceMap {
                            expr_map,
                            expr_map_back,
                            pat_map,
                            pat_map_back,
                            label_map,
                            label_map_back,
                            binding_definitions,
                            field_map_back,
                            pat_field_map_back,
                            template_map,
                            expansions,
                            diagnostics: ThinVec::from_iter(diagnostics),
                        })
                    });
                ExpressionStoreSourceMap {
                    expr_only,
                    types_map_back,
                    types_map,
                    lifetime_map_back,
                    lifetime_map,
                }
            }
        };
        let expr_only = match has_exprs {
            false => None,
            true => {
                if let Some(expr_roots) = &mut expr_roots {
                    expr_roots.shrink_to_fit();
                }
                let Scratch {
                    bindings,
                    labels,
                    binding_owners,
                    block_scopes,
                    ident_hygiene,
                    block_editions,
                    overflow_inheriting,
                    packing,
                    ..
                } = &mut scratch;
                if !binding_owners.is_empty() {
                    packing.owners.resize(bindings.len(), NO_OWNER);
                    for (binding, owner) in binding_owners.iter() {
                        packing.owners[binding.into_raw().into_u32() as usize] =
                            owner.into_raw().into_u32();
                    }
                }
                packing.hygiene.extend(ident_hygiene.iter().map(|(&id, &hygiene)| (id, hygiene)));
                packing.hygiene.sort_unstable_by_key(|&(id, _)| id);
                packing.editions.extend(block_editions.iter().map(|(&id, &edition)| (id, edition)));
                radix::sort_copy_by_key_in(
                    &mut packing.editions,
                    |&(id, _)| id.into_raw().into_u32(),
                    &mut packing.editions_sort,
                );
                packing.inheriting.extend(overflow_inheriting.iter().copied());
                radix::sort_copy_by_key_in(
                    &mut packing.inheriting,
                    |it| it.into_raw().into_u32(),
                    &mut packing.inheriting_sort,
                );
                // Every element type here is 4-byte aligned, so no padding between columns.
                let mut layout = SlabLayout::default();
                layout.add::<BlockId>(block_scopes.len());
                layout.add::<(ExprOrPatIdPacked, HygieneId)>(packing.hygiene.len());
                layout.add::<(ExprId, Edition)>(packing.editions.len());
                layout.add::<u32>(packing.owners.len());
                layout.add::<ExprId>(packing.inheriting.len());
                let mut slab = SlabWriter::new(layout);
                let block_scopes = slab.copy(&block_scopes[..]);
                let ident_hygiene = slab.copy(&packing.hygiene[..]);
                let block_editions = slab.copy(&packing.editions[..]);
                let binding_owners = slab.copy(&packing.owners[..]);
                let overflow_inheriting = slab.copy(&packing.inheriting[..]);
                // alloc: once per store, keeps signature-only stores small.
                Some(Box::new(ExpressionOnlyStore {
                    kinds: kinds_of(&columns.expr_kind),
                    columns,
                    bindings: exact_arena(bindings),
                    labels: exact_arena(labels),
                    binding_owners,
                    block_scopes,
                    ident_hygiene,
                    block_editions,
                    overflow_inheriting,
                    expr_roots: expr_roots
                        .expect("should always finish with a `Some(_)` expr_roots"),
                }))
            }
        };
        let lifetimes = exact_arena(&scratch.lifetimes);
        // alloc: the column's one exact copy, only for a store with macro-expanded types.
        let type_hygiene = (!scratch.type_hygiene.is_empty())
            .then(|| Box::<[_]>::from(&scratch.type_hygiene[..]));
        // alloc: the column's one exact copy, only for a store that met a macro call.
        let macro_calls = ThinVec::from_iter(scratch.macro_calls.iter().copied());
        scratch.give();
        (ExpressionStore { expr_only, types, lifetimes, type_hygiene, macro_calls }, source_map)
    }
}

impl Drop for ExpressionStoreBuilder {
    /// A builder dropped without [`ExpressionStoreBuilder::finish`] (a path lowered only to be
    /// resolved) gives its scratch back too. After `finish` nothing is left, and an empty set is
    /// not pooled, where it would shadow a grown one.
    fn drop(&mut self) {
        let scratch = self.take_scratch();
        if scratch.capacity() != 0 {
            scratch.give();
        }
    }
}

impl ExpressionStore {
    /// The store with nothing in it. A `static` rather than a `const`: boxed columns can't be
    /// made in a `const`. Empty columns allocate nothing.
    #[inline]
    pub fn empty() -> &'static ExpressionStore {
        static EMPTY: LazyLock<ExpressionStore> = LazyLock::new(ExpressionStore::empty_owned);
        &EMPTY
    }

    /// [`ExpressionStore::empty`] as a value of its own, for an owner with no store to lower.
    pub fn empty_owned() -> ExpressionStore {
        ExpressionStore {
            expr_only: None,
            types: TypeColumns::default(),
            lifetimes: Arena::new(),
            type_hygiene: None,
            macro_calls: ThinVec::new(),
        }
    }

    /// The type ref `id`, decoded from the store's columns.
    #[inline]
    pub fn type_ref(&self, id: TypeRefId) -> TypeRef<'_> {
        self.types.get(id)
    }

    #[inline]
    pub fn type_refs(&self) -> impl Iterator<Item = (TypeRefId, TypeRef<'_>)> {
        self.types.iter()
    }

    pub fn of(db: &Db, def: ExpressionStoreOwnerId) -> &ExpressionStore {
        match def {
            ExpressionStoreOwnerId::Signature(def) => {
                use crate::signatures::{
                    ConstSignature, EnumSignature, FunctionSignature, ImplSignature,
                    StaticSignature, StructSignature, TraitSignature, TypeAliasSignature,
                    UnionSignature,
                };
                match def {
                    GenericDefId::AdtId(AdtId::EnumId(id)) => &EnumSignature::of(db, id).store,
                    GenericDefId::AdtId(AdtId::StructId(id)) => &StructSignature::of(db, id).store,
                    GenericDefId::AdtId(AdtId::UnionId(id)) => &UnionSignature::of(db, id).store,
                    GenericDefId::ConstId(id) => &ConstSignature::of(db, id).store,
                    GenericDefId::FunctionId(id) => &FunctionSignature::of(db, id).store,
                    GenericDefId::ImplId(id) => &ImplSignature::of(db, id).store,
                    GenericDefId::StaticId(id) => &StaticSignature::of(db, id).store,
                    GenericDefId::TraitId(id) => &TraitSignature::of(db, id).store,
                    GenericDefId::TypeAliasId(id) => &TypeAliasSignature::of(db, id).store,
                }
            }
            ExpressionStoreOwnerId::Body(body) => &Body::of(db, body).store,
            ExpressionStoreOwnerId::VariantFields(variant_id) => {
                &VariantFields::of(db, variant_id).store
            }
        }
    }

    pub fn with_source_map(
        db: &Db,
        def: ExpressionStoreOwnerId,
    ) -> (&ExpressionStore, &ExpressionStoreSourceMap) {
        match def {
            ExpressionStoreOwnerId::Signature(def) => {
                use crate::signatures::{
                    ConstSignature, EnumSignature, FunctionSignature, ImplSignature,
                    StaticSignature, StructSignature, TraitSignature, TypeAliasSignature,
                    UnionSignature,
                };
                match def {
                    GenericDefId::AdtId(AdtId::EnumId(id)) => {
                        let sig = EnumSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::AdtId(AdtId::StructId(id)) => {
                        let sig = StructSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::AdtId(AdtId::UnionId(id)) => {
                        let sig = UnionSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::ConstId(id) => {
                        let sig = ConstSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::FunctionId(id) => {
                        let sig = FunctionSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::ImplId(id) => {
                        let sig = ImplSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::StaticId(id) => {
                        let sig = StaticSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::TraitId(id) => {
                        let sig = TraitSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::TypeAliasId(id) => {
                        let sig = TypeAliasSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                }
            }
            ExpressionStoreOwnerId::Body(body) => {
                let (store, sm) = Body::with_source_map(db, body);
                (&store.store, &sm.store)
            }
            ExpressionStoreOwnerId::VariantFields(variant_id) => {
                let (store, sm) = VariantFields::with_source_map(db, variant_id);
                (&store.store, sm)
            }
        }
    }

    /// Returns all expression root `ExprId`s found in this store.
    pub fn expr_roots(&self) -> impl DoubleEndedIterator<Item = ExprId> {
        self.expr_only
            .as_ref()
            .map_or(&[][..], |expr_only| &expr_only.expr_roots)
            .iter()
            .map(|root| root.root)
    }

    fn find_root_for(
        &self,
        mut get: impl FnMut(&ExprRoot) -> la_arena::RawIdx,
        find: la_arena::RawIdx,
    ) -> ExprId {
        let expr_only = self.assert_expr_only();
        let find = find.into_u32();
        let entry = expr_only.expr_roots.partition_point(|root| get(root).into_u32() <= find);
        expr_only.expr_roots[entry].root
    }

    pub fn find_root_for_expr(&self, expr: ExprId) -> ExprId {
        self.find_root_for(|root| root.exprs_end.into_raw(), expr.into_raw())
    }

    pub fn find_root_for_pat(&self, pat: PatId) -> ExprId {
        self.find_root_for(|root| root.pats_end.into_raw(), pat.into_raw())
    }

    pub fn find_root_for_binding(&self, binding: BindingId) -> ExprId {
        self.find_root_for(|root| root.bindings_end.into_raw(), binding.into_raw())
    }

    /// Returns an iterator over all block expressions in this store that define inner items.
    pub fn blocks<'a>(
        &'a self,
        db: &'a Db,
    ) -> impl Iterator<Item = (BlockId, &'a DefMap)> + 'a {
        self.expr_only
            .as_ref()
            .map(|it| &*it.block_scopes)
            .unwrap_or_default()
            .iter()
            .map(move |&block| (block, block_def_map(db, block)))
    }

    /// The blocks this store's lowering made, in the order it made them: what lowering it again
    /// for its source map is given ([`lower::BodyLowering::SourceMap`]).
    pub(crate) fn block_scopes(&self) -> &[BlockId] {
        self.expr_only.as_ref().map_or(&[][..], |it| &*it.block_scopes)
    }

    pub fn walk_bindings_in_pat(&self, pat_id: PatId, mut f: impl FnMut(BindingId)) {
        self.walk_pats(pat_id, &mut |pat| {
            if let Pat::Bind { id, .. } = self.pat(pat) {
                f(id);
            }
        });
    }

    pub fn visit_pat_children(&self, pat_id: PatId, mut visitor: impl StoreVisitor) {
        // Do not use `..` patterns or field accesses here, only destructuring, to ensure we cover all cases
        // (we've had multiple bugs with this in the past).
        match self.pat(pat_id) {
            Pat::Range { start, end, range_type: _ } => {
                visitor.on_expr_opt(start);
                visitor.on_expr_opt(end);
            }
            Pat::Lit(expr) | Pat::Expr(expr) => visitor.on_expr(expr),
            Pat::Path(path) => visitor.on_path(path),
            Pat::Wild | Pat::Missing | Pat::Rest | Pat::NotNull => {}
            Pat::Bind { subpat, id: _ } => visitor.on_pat_opt(subpat),
            Pat::Or(args) | Pat::Tuple { args, ellipsis: _ } => visitor.on_pats(args),
            Pat::TupleStruct { args, ellipsis: _, path } => {
                visitor.on_pats(args);
                visitor.on_path(path);
            }
            Pat::Ref { pat, mutability: _ } => visitor.on_pat(pat),
            Pat::Slice { prefix, slice, suffix } => {
                visitor.on_pats(prefix);
                visitor.on_pat_opt(slice);
                visitor.on_pats(suffix);
            }
            Pat::Record { args, ellipsis: _, path } => {
                args.iter().for_each(|RecordFieldPat { pat, name: _ }| visitor.on_pat(*pat));
                visitor.on_path(path);
            }
            Pat::Box { inner } | Pat::Deref { inner } => visitor.on_pat(inner),
        }
    }

    pub fn walk_pats_shallow(&self, pat_id: PatId, f: impl FnMut(PatId)) {
        return self.visit_pat_children(pat_id, Visitor(f));

        struct Visitor<F>(F);

        impl<F: FnMut(PatId)> StoreVisitor for Visitor<F> {
            fn on_pat(&mut self, pat: PatId) {
                (self.0)(pat);
            }
        }
    }

    pub fn walk_pats(&self, pat_id: PatId, f: &mut impl FnMut(PatId)) {
        f(pat_id);
        self.walk_pats_shallow(pat_id, |p| self.walk_pats(p, f));
    }

    #[inline]
    pub fn binding_owner(&self, id: BindingId) -> Option<ExprId> {
        let owners = &self.expr_only.as_ref()?.binding_owners;
        let owner = *owners.get(id.into_raw().into_u32() as usize)?;
        (owner != NO_OWNER).then(|| ExprId::from_raw(la_arena::RawIdx::from_u32(owner)))
    }

    pub fn visit_expr_children(&self, expr_id: ExprId, visitor: impl StoreVisitor) {
        // A walk of the expressions recurses through here, once for each that is in another:
        // rustc's 5,204-arm `else if`.
        stdx::ensure_sufficient_stack(|| self.visit_expr_children_here(expr_id, visitor))
    }

    fn visit_expr_children_here(&self, expr_id: ExprId, mut visitor: impl StoreVisitor) {
        // Do not use `..` patterns or field accesses here, only destructuring, to ensure we cover all cases
        // (we've had multiple bugs with this in the past).
        match self.expr(expr_id) {
            Expr::OffsetOf(OffsetOf { container, fields: _ }) => visitor.on_type(*container),
            Expr::Path(path) => visitor.on_path(path),
            Expr::Continue { label: _ } | Expr::Missing | Expr::Literal(_) | Expr::Underscore => {}
            Expr::InlineAsm(InlineAsm { operands, .. }) => {
                operands.iter().for_each(|(_, op)| match op {
                    AsmOperand::In { expr, reg: _ }
                    | AsmOperand::Out { expr: Some(expr), late: _, reg: _ }
                    | AsmOperand::InOut { expr, late: _, reg: _ }
                    | AsmOperand::Const(expr)
                    | AsmOperand::Label(expr) => visitor.on_expr(*expr),
                    AsmOperand::SplitInOut { in_expr, out_expr, late: _, reg: _ } => {
                        visitor.on_expr(*in_expr);
                        visitor.on_expr_opt(*out_expr);
                    }
                    AsmOperand::Out { expr: None, late: _, reg: _ } | AsmOperand::Sym(_) => (),
                })
            }
            Expr::If { condition, then_branch, else_branch } => {
                visitor.on_expr(condition);
                visitor.on_expr(then_branch);
                visitor.on_expr_opt(else_branch);
            }
            Expr::Let { expr, pat } => {
                visitor.on_pat(pat);
                visitor.on_expr(expr);
            }
            Expr::Block { statements, tail, id: _, label: _, unsafe_: _ } => {
                for stmt in statements {
                    match stmt {
                        Statement::Let { initializer, else_branch, pat, type_ref, is_super: _ } => {
                            visitor.on_expr_opt(initializer);
                            visitor.on_expr_opt(else_branch);
                            visitor.on_pat(pat);
                            visitor.on_type_opt(type_ref);
                        }
                        Statement::Expr { expr: expression, has_semi: _ } => {
                            visitor.on_expr(expression)
                        }
                        Statement::Item(_) => (),
                    }
                }
                visitor.on_expr_opt(tail);
            }
            Expr::Loop { body, label: _, source: _ } => visitor.on_expr(body),
            Expr::Call { callee, args } => {
                visitor.on_expr(callee);
                visitor.on_exprs(args);
            }
            Expr::MethodCall { receiver, args, generic_args, method_name: _ } => {
                visitor.on_expr(receiver);
                visitor.on_exprs(args);
                visitor.on_generic_args_opt(generic_args);
            }
            Expr::Match { expr, arms } => {
                visitor.on_expr(expr);
                arms.iter().for_each(|MatchArm { pat, guard, expr }| {
                    visitor.on_expr(*expr);
                    visitor.on_expr_opt(*guard);
                    visitor.on_pat(*pat);
                });
            }
            Expr::Break { expr, label: _ }
            | Expr::Return { expr }
            | Expr::Yield { expr }
            | Expr::Yeet { expr } => visitor.on_expr_opt(expr),
            Expr::Become { expr } => visitor.on_expr(expr),
            Expr::RecordLit { fields, spread, path } => {
                for RecordLitField { name: _, expr } in fields.iter() {
                    visitor.on_expr(*expr);
                }
                match spread {
                    RecordSpread::Expr(expr) => visitor.on_expr(expr),
                    RecordSpread::None | RecordSpread::FieldDefaults => {}
                }
                visitor.on_path(path);
            }
            Expr::Closure { body, args, arg_types, ret_type, capture_by: _, closure_kind: _ } => {
                visitor.on_expr(body);
                visitor.on_pats(args);
                arg_types.iter().for_each(|arg_type| visitor.on_type_opt(*arg_type));
                visitor.on_type_opt(ret_type);
            }
            Expr::BinaryOp { lhs, rhs, op: _ } => {
                visitor.on_expr(lhs);
                visitor.on_expr(rhs);
            }
            Expr::Index { base, index } => {
                visitor.on_expr(base);
                visitor.on_expr(index);
            }
            Expr::Cast { expr, type_ref } => {
                visitor.on_expr(expr);
                visitor.on_type(type_ref);
            }
            Expr::UnsafeBinderCast { expr, ty, kind: _ } => {
                visitor.on_expr(expr);
                visitor.on_type_opt(ty);
            }
            Expr::Field { expr, name: _ }
            | Expr::Await { expr }
            | Expr::Ref { expr, mutability: _, rawness: _ }
            | Expr::UnaryOp { expr, op: _ } => visitor.on_expr(expr),
            Expr::Const(expr) => visitor.on_anon_const_expr(expr),
            Expr::Tuple { exprs } => visitor.on_exprs(exprs),
            Expr::Array(a) => match a {
                Array::ElementList { elements } => visitor.on_exprs(elements),
                Array::Repeat { initializer, repeat } => {
                    visitor.on_expr(initializer);
                    visitor.on_anon_const_expr(repeat)
                }
            },
            Expr::Assignment { target, value } => {
                visitor.on_pat(target);
                visitor.on_expr(value);
            }
            Expr::IncludeBytes => {}
        }
    }

    /// Walks the immediate children expressions and calls `f` for each child expression.
    pub fn walk_child_exprs(&self, expr_id: ExprId, callback: impl FnMut(ExprId)) {
        return self.visit_expr_children(expr_id, Visitor { callback, store: self });

        struct Visitor<'a, F> {
            callback: F,
            store: &'a ExpressionStore,
        }

        impl<F: FnMut(ExprId)> StoreVisitor for Visitor<'_, F> {
            fn on_expr(&mut self, expr: ExprId) {
                (self.callback)(expr);
            }

            fn on_pat(&mut self, pat: PatId) {
                self.store.walk_exprs_in_pat(pat, &mut self.callback);
            }
        }
    }

    /// Walks the immediate children expressions and calls `f` for each child expression but does
    /// not walk expressions within patterns.
    pub fn walk_child_exprs_without_pats(&self, expr_id: ExprId, callback: impl FnMut(ExprId)) {
        return self.visit_expr_children(expr_id, Visitor { callback });

        struct Visitor<F> {
            callback: F,
        }

        impl<F: FnMut(ExprId)> StoreVisitor for Visitor<F> {
            fn on_expr(&mut self, expr: ExprId) {
                (self.callback)(expr);
            }
        }
    }

    pub fn walk_exprs_in_pat(&self, pat_id: PatId, callback: impl FnMut(ExprId)) {
        return Visitor { callback, store: self }.on_pat(pat_id);

        struct Visitor<'a, F> {
            callback: F,
            store: &'a ExpressionStore,
        }

        impl<F: FnMut(ExprId)> StoreVisitor for Visitor<'_, F> {
            fn on_expr(&mut self, expr: ExprId) {
                (self.callback)(expr);
            }

            fn on_pat(&mut self, pat: PatId) {
                self.store.visit_pat_children(pat, self);
            }
        }
    }

    pub fn visit_type_ref_children(&self, type_ref: TypeRefId, mut visitor: impl StoreVisitor) {
        match self.type_ref(type_ref) {
            TypeRef::Never | TypeRef::Placeholder | TypeRef::TypeParam(_) | TypeRef::Error => {}
            TypeRef::PatternType(ty, pat) => {
                visitor.on_type(ty);
                visitor.on_pat(pat)
            }
            TypeRef::UnsafeBinder { lifetimes: _, ty } => visitor.on_type(ty),
            TypeRef::Tuple(types) => visitor.on_types(types),
            TypeRef::Path(path) => visitor.on_path(path),
            TypeRef::RawPtr(inner, _) | TypeRef::Slice(inner) => visitor.on_type(inner),
            TypeRef::Reference(RefType { ty, lifetime, mutability: _ }) => {
                visitor.on_type(*ty);
                visitor.on_lifetime_opt(*lifetime);
            }
            TypeRef::Array(ArrayType { ty, len: ConstRef { expr: len } }) => {
                visitor.on_type(*ty);
                visitor.on_anon_const_expr(*len);
            }
            TypeRef::Fn(FnType { params, is_varargs: _, is_unsafe: _, abi: _, binder: _ }) => {
                params.iter().for_each(|(_, param_ty)| visitor.on_type(*param_ty));
            }
            TypeRef::ImplTrait(bounds) | TypeRef::DynTrait(bounds) => {
                visit_type_bounds(&mut visitor, &self.types.paths, bounds)
            }
        }
    }

    #[inline]
    #[track_caller]
    fn assert_expr_only(&self) -> &ExpressionOnlyStore {
        self.expr_only.as_ref().expect("should have `ExpressionStore::expr_only`")
    }

    fn binding_hygiene(&self, binding: BindingId) -> HygieneId {
        self.assert_expr_only().bindings[binding].hygiene
    }

    /// Whether a closure takes the overflow checks of the crate that compiles it, as a function
    /// with `#[rustc_inherit_overflow_checks]` does.
    pub fn inherits_overflow_checks(&self, closure: ExprId) -> bool {
        self.assert_expr_only().overflow_inheriting.binary_search(&closure).is_ok()
    }

    /// The edition a block was written in, when a macro wrote it.
    pub fn block_edition(&self, block: ExprId) -> Option<Edition> {
        let editions = &self.assert_expr_only().block_editions;
        let index = editions.binary_search_by_key(&block, |&(id, _)| id).ok()?;
        Some(editions[index].1)
    }

    pub fn type_path_hygiene(&self, type_ref: TypeRefId) -> HygieneId {
        let Some(hygiene) = &self.type_hygiene else { return HygieneId::ROOT };
        match hygiene.binary_search_by_key(&type_ref, |&(id, _)| id) {
            Ok(index) => hygiene[index].1,
            Err(_) => HygieneId::ROOT,
        }
    }

    fn ident_hygiene(&self, id: ExprOrPatIdPacked) -> HygieneId {
        let hygiene = &self.assert_expr_only().ident_hygiene;
        match hygiene.binary_search_by_key(&id, |&(id, _)| id) {
            Ok(index) => hygiene[index].1,
            Err(_) => HygieneId::ROOT,
        }
    }

    pub fn expr_path_hygiene(&self, expr: ExprId) -> HygieneId {
        self.ident_hygiene(expr.into())
    }

    pub fn pat_path_hygiene(&self, pat: PatId) -> HygieneId {
        self.ident_hygiene(pat.into())
    }

    pub fn expr_or_pat_path_hygiene(&self, id: ExprOrPatId) -> HygieneId {
        match id {
            ExprOrPatId::ExprId(id) => self.expr_path_hygiene(id),
            ExprOrPatId::PatId(id) => self.pat_path_hygiene(id),
        }
    }

    /// The expression `id`, decoded from the store's columns.
    #[inline]
    pub fn expr(&self, id: ExprId) -> Expr<'_> {
        self.assert_expr_only().columns.expr(&self.types.paths, id)
    }

    /// The pattern `id`, decoded from the store's columns.
    #[inline]
    pub fn pat(&self, id: PatId) -> Pat<'_> {
        self.assert_expr_only().columns.pat(&self.types.paths, id)
    }

    #[inline]
    pub fn exprs(&self) -> impl Iterator<Item = (ExprId, Expr<'_>)> {
        let paths = &self.types.paths;
        self.expr_only.as_deref().into_iter().flat_map(move |it| it.columns.exprs(paths))
    }

    /// How many expressions the store holds.
    #[inline]
    pub fn expr_count(&self) -> usize {
        self.expr_only.as_deref().map_or(0, |it| it.columns.expr_count())
    }

    /// A bit per variant of [`Expr`], in variant order: bit `k` is set when some expression of
    /// the store is of the `k`th variant. Zero for a store without expressions. Made once, with
    /// the store.
    #[inline]
    pub fn kinds_present(&self) -> u64 {
        self.expr_only.as_deref().map_or(0, |it| it.kinds)
    }

    /// Whether any expression is a closure.
    #[inline]
    pub fn has_closures(&self) -> bool {
        self.kinds_present() & (1 << columns::ExprKind::Closure as u32) != 0
    }

    /// The kind of expression `id`: one byte of the `expr_kind` column, with no row decoded.
    #[inline]
    pub fn expr_kind(&self, id: ExprId) -> columns::ExprKind {
        self.assert_expr_only().columns.expr_kind_of(id)
    }

    /// Each `let` statement of the store, in row order, as its pattern and whether it has an
    /// `else`, decoding only the `let` rows; none for a store without expressions.
    ///
    /// Every statement row is a row of exactly one `Expr::Block` of the store: lowering keeps
    /// the statements it is collecting on a stack of its own and writes a block's rows into
    /// the store only when it writes that block. So this is every `let` of every block of the
    /// store, each once, the blocks lowering made and left unreferenced included. Only
    /// replacing a block with [`ExpressionStoreBuilder::set_expr`], which lowering never does,
    /// could leave rows of no block.
    pub fn let_statements(&self) -> impl Iterator<Item = (PatId, bool)> + '_ {
        self.expr_only.as_deref().into_iter().flat_map(|it| it.columns.let_statements())
    }

    /// Every macro call lowered into the store, in the order lowering met them: the calls of
    /// [`ExpressionStoreSourceMap::macro_calls`], without the map.
    #[inline]
    pub fn macro_calls(&self) -> &[MacroCallId] {
        &self.macro_calls
    }

    /// How many patterns the store holds.
    #[inline]
    pub fn pat_count(&self) -> usize {
        self.expr_only.as_deref().map_or(0, |it| it.columns.pat_count())
    }

    /// Every expression's id, in order, without decoding any: for a scan that wants the ids
    /// (or tests only some of them), where [`Self::exprs`] decodes every node.
    #[inline]
    pub fn expr_ids(&self) -> impl DoubleEndedIterator<Item = ExprId> + ExactSizeIterator {
        (0..self.expr_count() as u32).map(|raw| ExprId::from_raw(la_arena::RawIdx::from_u32(raw)))
    }

    /// Every pattern's id, in order, without decoding any.
    #[inline]
    pub fn pat_ids(&self) -> impl DoubleEndedIterator<Item = PatId> + ExactSizeIterator {
        (0..self.pat_count() as u32).map(|raw| PatId::from_raw(la_arena::RawIdx::from_u32(raw)))
    }

    /// The store's columns, `None` for a store without expressions.
    #[cfg(test)]
    pub(crate) fn hir_columns(&self) -> Option<&crate::hir::columns::HirColumns> {
        self.expr_only.as_deref().map(|it| &it.columns)
    }

    #[inline]
    pub fn pats(&self) -> impl Iterator<Item = (PatId, Pat<'_>)> {
        let paths = &self.types.paths;
        self.expr_only.as_deref().into_iter().flat_map(move |it| it.columns.pats(paths))
    }

    #[inline]
    pub fn bindings(&self) -> impl Iterator<Item = (BindingId, &Binding)> {
        match &self.expr_only {
            Some(it) => it.bindings.iter(),
            None => const { &Arena::new() }.iter(),
        }
    }

    #[inline]
    pub fn labels(&self) -> impl Iterator<Item = (LabelId, &Label)> {
        match &self.expr_only {
            Some(it) => it.labels.iter(),
            None => const { &Arena::new() }.iter(),
        }
    }

    /// The coroutine associated with a coroutine closure.
    #[inline]
    pub fn coroutine_for_closure(coroutine_closure: ExprId) -> ExprId {
        // We keep the async closure exactly one expr before.
        ExprId::from_raw(la_arena::RawIdx::from_u32(coroutine_closure.into_raw().into_u32() - 1))
    }

    /// The opposite of [`Self::coroutine_for_closure()`].
    #[inline]
    pub fn closure_for_coroutine(coroutine: ExprId) -> ExprId {
        // We keep the async closure exactly one expr before.
        ExprId::from_raw(la_arena::RawIdx::from_u32(coroutine.into_raw().into_u32() + 1))
    }
}

pub trait StoreVisitor: Sized {
    fn on_expr(&mut self, expr: ExprId) {
        let _ = expr;
    }
    fn on_anon_const_expr(&mut self, expr: ExprId) {
        self.on_expr(expr);
    }
    fn on_pat(&mut self, pat: PatId) {
        let _ = pat;
    }
    fn on_type(&mut self, ty: TypeRefId) {
        let _ = ty;
    }
    fn on_lifetime(&mut self, lifetime: LifetimeRefId) {
        let _ = lifetime;
    }

    fn on_generic_args(&mut self, args: GenericArgs<'_>) {
        visit_generic_args(self, args);
    }
}

pub(crate) fn visit_generic_args<V: StoreVisitor>(visitor: &mut V, args: GenericArgs<'_>) {
    let GenericArgs { args, bindings, parenthesized: _, has_self_type: _ } = args;
    for arg in args {
        match arg {
            GenericArg::Type(arg) => visitor.on_type(*arg),
            GenericArg::Const(ConstRef { expr }) => visitor.on_anon_const_expr(*expr),
            GenericArg::Lifetime(arg) => visitor.on_lifetime(*arg),
        }
    }
    for AssociatedTypeBinding { name: _, args, type_ref, bounds } in bindings {
        visitor.on_generic_args_opt(args);
        visitor.on_type_opt(type_ref);
        visit_type_bounds(visitor, bindings.columns(), bounds);
    }
}

/// The paths and lifetimes of `bounds`, their `use<..>` arguments read from `paths`, the
/// columns the bounds are of.
pub(crate) fn visit_type_bounds<V: StoreVisitor>(
    visitor: &mut V,
    paths: &PathColumns,
    bounds: &[TypeBound],
) {
    for bound in bounds {
        match *bound {
            TypeBound::Path(path_id, _) | TypeBound::ForLifetime(_, path_id) => {
                visitor.on_type(path_id.type_ref())
            }
            TypeBound::Lifetime(lifetime) => visitor.on_lifetime(lifetime),
            TypeBound::Use(args) => {
                for arg in paths.use_args(args) {
                    match arg {
                        UseArgRef::Lifetime(lifetime) => visitor.on_lifetime(*lifetime),
                        UseArgRef::Name(_) => {}
                    }
                }
            }
            TypeBound::Error => {}
        }
    }
}

impl<V: StoreVisitor> StoreVisitor for &mut V {
    fn on_expr(&mut self, expr: ExprId) {
        V::on_expr(self, expr);
    }
    fn on_anon_const_expr(&mut self, expr: ExprId) {
        V::on_anon_const_expr(self, expr);
    }
    fn on_pat(&mut self, pat: PatId) {
        V::on_pat(self, pat);
    }
    fn on_type(&mut self, ty: TypeRefId) {
        V::on_type(self, ty);
    }
    fn on_lifetime(&mut self, lifetime: LifetimeRefId) {
        V::on_lifetime(self, lifetime);
    }

    fn on_generic_args(&mut self, args: GenericArgs<'_>) {
        V::on_generic_args(self, args);
    }
}

pub trait StoreVisitorExt: StoreVisitor {
    fn on_path(&mut self, path: Path<'_>) {
        match path {
            Path::Normal(NormalPath { generic_args, type_anchor, mod_path: _ }) => {
                generic_args.iter().for_each(|generic_arg| self.on_generic_args_opt(generic_arg));
                self.on_type_opt(type_anchor);
            }
            Path::BarePath(_) | Path::LangItem(..) => {}
        }
    }

    fn on_expr_opt(&mut self, expr: Option<ExprId>) {
        if let Some(expr) = expr {
            self.on_expr(expr);
        }
    }
    fn on_pat_opt(&mut self, pat: Option<PatId>) {
        if let Some(pat) = pat {
            self.on_pat(pat);
        }
    }
    fn on_type_opt(&mut self, ty: Option<TypeRefId>) {
        if let Some(ty) = ty {
            self.on_type(ty);
        }
    }
    fn on_lifetime_opt(&mut self, lifetime: Option<LifetimeRefId>) {
        if let Some(lifetime) = lifetime {
            self.on_lifetime(lifetime);
        }
    }
    fn on_generic_args_opt(&mut self, args: Option<GenericArgs<'_>>) {
        if let Some(args) = args {
            self.on_generic_args(args);
        }
    }

    fn on_exprs(&mut self, exprs: impl IntoIterator<Item: Borrow<ExprId>>) {
        exprs.into_iter().for_each(|expr| self.on_expr(*expr.borrow()));
    }
    fn on_pats(&mut self, pats: impl IntoIterator<Item: Borrow<PatId>>) {
        pats.into_iter().for_each(|pat| self.on_pat(*pat.borrow()));
    }
    fn on_types(&mut self, types: impl IntoIterator<Item: Borrow<TypeRefId>>) {
        types.into_iter().for_each(|ty| self.on_type(*ty.borrow()));
    }
}
impl<V: StoreVisitor> StoreVisitorExt for V {}

impl Index<LabelId> for ExpressionStore {
    type Output = Label;

    #[inline]
    fn index(&self, label: LabelId) -> &Label {
        &self.assert_expr_only().labels[label]
    }
}

impl Index<BindingId> for ExpressionStore {
    type Output = Binding;

    #[inline]
    fn index(&self, b: BindingId) -> &Binding {
        &self.assert_expr_only().bindings[b]
    }
}

impl Index<LifetimeRefId> for ExpressionStore {
    type Output = LifetimeRef;

    #[inline]
    fn index(&self, b: LifetimeRefId) -> &LifetimeRef {
        &self.lifetimes[b]
    }
}

#[cfg(test)]
mod scratch_tests {
    use blaze_alloc_count::{assert_at_most, measure};
    use syntax::SyntaxKind;

    use super::{source_map::tests::src, *};
    use crate::hir::BindingAnnotation;

    fn empty_pool() {
        POOL.with(|pool| pool.borrow_mut().clear());
    }

    fn pooled() -> usize {
        POOL.with(|pool| pool.borrow().len())
    }

    /// A small store: type refs, expressions and patterns made through the list stacks, and
    /// source maps with repeated pointers.
    fn lower_one(builder: &mut ExpressionStoreBuilder, shift: u32) {
        builder.inference_roots = Some(SmallVec::new());
        let unit = builder.alloc_type_ref(TypeRef::UNIT);
        let mark = builder.list_mark();
        builder.push_list_type(unit);
        builder.push_list_type(unit);
        let lists = builder.take_lists();
        let tuple = builder.alloc_type_ref(TypeRef::Tuple(lists.types_since(mark)));
        builder.put_lists(lists, mark);
        let missing = builder.alloc_expr(Expr::Missing);
        let outer = builder.list_mark();
        builder.push_list_expr(missing);
        let inner = builder.list_mark();
        builder.push_list_expr(missing);
        builder.push_list_expr(missing);
        let lists = builder.take_lists();
        let tuple_expr = builder.alloc_expr(Expr::Tuple { exprs: lists.exprs_since(inner) });
        builder.put_lists(lists, inner);
        assert_eq!(builder.exprs_since(outer), &[missing], "the inner list is gone");
        builder.push_list_expr(tuple_expr);
        let lists = builder.take_lists();
        let call = builder.alloc_expr(Expr::Call { callee: missing, args: lists.exprs_since(outer) });
        builder.put_lists(lists, outer);
        let wild = builder.alloc_pat(Pat::Wild);
        for at in [shift, shift + 1, shift] {
            builder.expr_map.push((src(0, false, SyntaxKind::CALL_EXPR, at, at + 2), call.into()));
        }
        builder.pat_map.push((src(0, false, SyntaxKind::WILDCARD_PAT, 4, 5), wild.into()));
        builder.types_map.push((src(0, false, SyntaxKind::TUPLE_TYPE, 6, 9), tuple));
        builder.expr_map_back.insert(call, src(0, false, SyntaxKind::CALL_EXPR, shift, shift + 2));
        builder.types_map_back.insert(tuple, src(0, false, SyntaxKind::TUPLE_TYPE, 6, 9));
    }

    fn finished(shift: u32) -> (ExpressionStore, ExpressionStoreSourceMap) {
        let mut builder = ExpressionStoreBuilder::new();
        lower_one(&mut builder, shift);
        builder.finish()
    }

    /// Pushes to every table lowering fills beside the columns: the arenas, the hash maps, the
    /// side vectors, and `n` entries of a forward map and of the field map.
    fn lower_every_table(builder: &mut ExpressionStoreBuilder, n: u32) {
        lower_one(builder, 0);
        let missing = builder.alloc_expr(Expr::Missing);
        let arm = MatchArm { pat: builder.alloc_pat(Pat::Wild), guard: None, expr: missing };
        let matched = builder.alloc_expr(Expr::Match { expr: missing, arms: &[arm] });
        let binding = builder.bindings.alloc(Binding {
            name: Name::missing(),
            mode: BindingAnnotation::Unannotated,
            problems: None,
            hygiene: HygieneId::ROOT,
        });
        builder.labels.alloc(Label { name: Name::missing() });
        let lifetime = builder.lifetimes.alloc(LifetimeRef::Static);
        builder.binding_owners.insert(binding, matched);
        builder.ident_hygiene.insert(missing.into(), HygieneId::ROOT);
        builder.block_editions.insert(matched, Edition::Edition2021);
        builder.overflow_inheriting.insert(matched);
        let unit = builder.alloc_type_ref(TypeRef::UNIT);
        builder.type_hygiene.push((unit, HygieneId::ROOT));
        for at in 0..n {
            let (start, end) = (10 * at, 10 * at + 5);
            let ptr = src(0, false, SyntaxKind::MATCH_EXPR, start, end);
            builder.expr_map.push((ptr, matched.into()));
            let field = src(0, false, SyntaxKind::RECORD_EXPR_FIELD, at, at + 1);
            builder.field_map_back.push((missing, field));
        }
        builder.lifetime_map.push((src(0, false, SyntaxKind::LIFETIME, 1, 3), lifetime));
    }

    /// A warm builder lowers without allocating: every table it pushes to is pooled scratch.
    #[test]
    fn a_warm_lowering_allocates_nothing() {
        empty_pool();
        let mut cold = ExpressionStoreBuilder::new();
        lower_every_table(&mut cold, 1000);
        drop(cold.finish());
        let mut warm = ExpressionStoreBuilder::new();
        assert_at_most(0, "lowering on warm scratch", || lower_every_table(&mut warm, 1000));
        drop(warm.finish());
    }

    /// `finish` allocates as many blocks for a body with a thousand map entries as for one with
    /// ten: no sort takes a buffer that grows with the body.
    #[test]
    fn finishing_allocates_no_scratch() {
        empty_pool();
        let finish_of = |n| {
            let mut builder = ExpressionStoreBuilder::new();
            lower_every_table(&mut builder, n);
            measure(|| builder.finish()).1.allocations
        };
        finish_of(1000);
        assert_eq!(finish_of(1000), finish_of(10), "a sort's buffer grew with the body");
    }

    /// Without a source map, a warm lowering and `finish` allocate exactly the finished store's
    /// blocks: the expression box, the column slab, the side-column slab, the three arenas,
    /// the type slab and the type hygiene.
    #[test]
    fn a_warm_store_is_its_blocks_and_nothing_else() {
        empty_pool();
        let lowered = || {
            let mut builder = ExpressionStoreBuilder::new();
            builder.skip_source_map();
            lower_every_table(&mut builder, 10);
            builder.finish()
        };
        drop(lowered());
        let ((store, _), used) = measure(lowered);
        assert_eq!(used.allocations, 8, "a store of 8 blocks");
        let only = store.expr_only.as_deref().expect("a store with expressions");
        assert!(only.columns.arms.in_slab(), "match arms are in the column slab");
        assert!(only.block_editions.in_slab() && only.binding_owners.in_slab());
        assert_eq!(&*only.overflow_inheriting, &[only.block_editions[0].0]);
    }

    /// A store's kinds are a bit for each variant some expression of it has, and no other.
    #[test]
    fn a_store_knows_its_expression_kinds() {
        use columns::ExprKind;

        empty_pool();
        let mut builder = ExpressionStoreBuilder::new();
        lower_every_table(&mut builder, 1);
        let (store, _) = builder.finish();
        let bit = |kind: ExprKind| 1u64 << kind as u32;
        let lowered = [ExprKind::Missing, ExprKind::Tuple, ExprKind::Call, ExprKind::Match];
        assert_eq!(store.kinds_present(), lowered.into_iter().map(bit).fold(0, |a, b| a | b));
        assert!(!store.has_closures());
        assert_eq!(ExpressionStore::empty().kinds_present(), 0, "no expressions, no kinds");
    }

    /// A builder dropped without `finish` (a path lowered only to be resolved) gives its
    /// scratch back emptied, and a finished one gives nothing more when it is dropped.
    #[test]
    fn an_unfinished_builder_gives_its_scratch_back() {
        empty_pool();
        let mut builder = ExpressionStoreBuilder::new();
        lower_every_table(&mut builder, 10);
        let at = builder.columns.expr_kind.as_ptr();
        drop(builder);
        assert_eq!(pooled(), 1, "the dropped builder's scratch is pooled");
        let next = ExpressionStoreBuilder::new();
        assert_eq!(next.columns.expr_kind.as_ptr(), at, "and taken by the next builder");
        assert!(next.columns.is_empty() && next.bindings.is_empty(), "given back emptied");
        drop(next.finish());
        assert_eq!(pooled(), 1, "a finished builder is pooled once");
    }

    #[test]
    fn nested_builders_take_the_pool_in_stack_order() {
        empty_pool();
        let mut outer = ExpressionStoreBuilder::new();
        outer.alloc_expr(Expr::Missing);
        outer.inference_roots = Some(SmallVec::new());
        let mut inner = ExpressionStoreBuilder::new();
        inner.alloc_expr(Expr::Missing);
        inner.inference_roots = Some(SmallVec::new());
        let (outer_at, inner_at) =
            (outer.columns.expr_kind.as_ptr(), inner.columns.expr_kind.as_ptr());
        assert_ne!(outer_at, inner_at, "a nested builder has scratch of its own");

        let _ = inner.finish();
        let _ = outer.finish();
        assert_eq!(pooled(), 2);
        let first = ExpressionStoreBuilder::new();
        let second = ExpressionStoreBuilder::new();
        assert_eq!(first.columns.expr_kind.as_ptr(), outer_at, "the last given is taken first");
        assert_eq!(second.columns.expr_kind.as_ptr(), inner_at);
        assert!(first.columns.is_empty() && second.columns.is_empty(), "given back emptied");
        assert_eq!(pooled(), 0);
    }

    #[test]
    fn a_store_is_the_same_on_fresh_or_reused_scratch() {
        empty_pool();
        let (fresh, fresh_map) = finished(0);
        assert_eq!(pooled(), 1);
        let _ = finished(10);
        let (reused, reused_map) = finished(0);
        assert_eq!(pooled(), 1, "one set is enough for builders one after another");
        assert_eq!(reused, fresh);
        assert_eq!(reused_map, fresh_map);
        let (fresh_only, reused_only) =
            (fresh_map.expr_only.as_deref().unwrap(), reused_map.expr_only.as_deref().unwrap());
        assert_eq!(reused_only.expr_map, fresh_only.expr_map);
        assert_eq!(reused_only.pat_map, fresh_only.pat_map);
        assert_eq!(reused_map.types_map, fresh_map.types_map);

        assert_eq!(fresh_only.expr_map.len(), 2, "a repeated pointer is one entry");
        let call = fresh.exprs().find(|(_, expr)| matches!(expr, Expr::Call { .. })).unwrap().0;
        let pushed_twice = src(0, false, SyntaxKind::CALL_EXPR, 0, 2);
        assert_eq!(fresh_only.expr_map.get(&pushed_twice), Some(&call.into()));
        let Expr::Call { args, .. } = fresh.expr(call) else { unreachable!() };
        assert!(matches!(fresh.expr(args[1]), Expr::Tuple { exprs } if exprs.len() == 2));
        let tuple_src = src(0, false, SyntaxKind::TUPLE_TYPE, 6, 9);
        let tuple = *fresh_map.types_map.get(&tuple_src).unwrap();
        // `TypeRef::UNIT` is the empty tuple, allocated first; this is the pair of them.
        assert!(matches!(fresh.type_ref(tuple), TypeRef::Tuple(types) if types.len() == 2));
    }

    #[test]
    fn the_statement_stack_nests_and_pops_back_to_a_mark() {
        use crate::hir::{Item, Unsafe};

        empty_pool();
        let mut builder = ExpressionStoreBuilder::new();
        builder.inference_roots = Some(SmallVec::new());
        let missing = builder.alloc_expr(Expr::Missing);
        let wild = builder.alloc_pat(Pat::Wild);
        let first = Statement::Let {
            pat: wild,
            type_ref: None,
            initializer: Some(missing),
            else_branch: None,
            is_super: false,
        };
        let inner_let = Statement::Let {
            pat: wild,
            type_ref: None,
            initializer: None,
            else_branch: Some(missing),
            is_super: true,
        };
        let outer = builder.list_mark();
        builder.push_list_statement(first);
        builder.push_list_statement(Statement::Item(Item::Other));
        let inner = builder.list_mark();
        builder.push_list_statement(inner_let);
        builder.push_list_statement(Statement::Expr { expr: missing, has_semi: false });
        assert_eq!(builder.pop_list_tail_expr(inner), Some(missing));
        assert_eq!(builder.pop_list_tail_expr(inner), None, "a `let` is not a tail");
        let lists = builder.take_lists();
        assert_eq!(lists.statements_since(inner).iter().collect::<Vec<_>>(), [inner_let]);
        let inner_block = builder.alloc_expr(Expr::Block {
            id: None,
            statements: lists.statements_since(inner),
            tail: Some(missing),
            label: None,
            unsafe_: Unsafe::No,
        });
        builder.put_lists(lists, inner);
        assert_eq!(builder.pop_list_tail_expr(inner), None, "nothing above the mark");
        let tail_stmt = Statement::Expr { expr: inner_block, has_semi: true };
        builder.push_list_statement(tail_stmt);
        let lists = builder.take_lists();
        let outer_block = builder.alloc_expr(Expr::Block {
            id: None,
            statements: lists.statements_since(outer),
            tail: None,
            label: None,
            unsafe_: Unsafe::No,
        });
        builder.put_lists(lists, outer);
        assert_eq!(builder.lists.stacked(), None);
        assert!(builder.lists.statements.extra.is_empty(), "the `let` words went with their rows");

        let (store, _) = builder.finish();
        let Expr::Block { statements, .. } = store.expr(outer_block) else { panic!("a block") };
        assert_eq!(
            statements.iter().collect::<Vec<_>>(),
            [first, Statement::Item(Item::Other), tail_stmt]
        );
        let Expr::Block { statements, tail, .. } = store.expr(inner_block) else { panic!("a block") };
        assert_eq!((statements.iter().collect::<Vec<_>>(), tail), (vec![inner_let], Some(missing)));
        let columns = store.hir_columns().unwrap();
        assert_eq!(columns.statement_count(), 4, "each block's run, and no stack rows");
        assert_eq!(columns.check(0, 0, 0, &store.types.paths), Ok(()));
    }

    #[test]
    fn the_pool_keeps_few_and_small_sets() {
        empty_pool();
        let builders: Vec<_> = (0..POOL_SETS + 2).map(|_| ExpressionStoreBuilder::new()).collect();
        builders.into_iter().for_each(|builder| drop(builder.finish()));
        assert_eq!(pooled(), POOL_SETS);

        empty_pool();
        let mut huge = ExpressionStoreBuilder::new();
        huge.columns.expr_kind.reserve(POOL_ELEMENTS + 1);
        let _ = huge.finish();
        assert_eq!(pooled(), 0, "a set over the cap is dropped");
    }
}
