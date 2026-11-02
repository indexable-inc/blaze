//! Name resolution for expressions.
//!
//! A store's scopes are columns: per scope a parent, a kind with one data word and a range of
//! one entries array, and the scope of each expression in a dense column by [`ExprId`]. A
//! query is an index or two; building them allocates only the exact-size columns.
use std::{cell::Cell, mem};

use base_db::Db;
use blaze_ids::radix;
use hir_expand::{MacroDefId, name::Name};
use la_arena::{Idx, RawIdx};

use crate::{
    BlockId, DefWithBodyId, ExpressionStoreOwnerId, GenericDefId, VariantId,
    expr_store::{Body, ExpressionStore, HygieneId, StoreVisitor, StoreVisitorExt, body::Param},
    hir::{
        Binding, BindingId, Expr, ExprId, Item, LabelId, Pat, PatId, Statement, Statements,
        generics::GenericParams,
    },
    signatures::VariantFields,
    type_ref::TypeRefId,
};

pub type ScopeId = Idx<ScopeData>;

/// What a [`ScopeId`] numbers: a scope is a row of the columns of [`ExprScopes`], not a value.
#[derive(Debug, PartialEq, Eq)]
pub enum ScopeData {}

/// The parent of a root scope, and the scope of an expression no scope was given. Not
/// `u32::MAX`, which `RawIdx` can't hold.
const NO_SCOPE: u32 = u32::MAX - 1;
const UNSCOPED: ScopeId = ScopeId::from_raw(RawIdx::from_u32(NO_SCOPE));

#[derive(Debug, PartialEq, Eq)]
pub struct ExprScopes {
    // Per scope, 17 bytes in all.
    parent: Box<[u32]>,
    kind: Box<[ScopeKind]>,
    /// A `Label`'s raw label id, a `Block`'s index in `blocks`, a `MacroDef`'s in `macro_defs`;
    /// 0 for `None`.
    data: Box<[u32]>,
    /// Where each scope's entries begin and end in `scope_entries`, which holds them grouped by
    /// scope, each scope's in the order they were added. Grouped, not in walk order: a scope
    /// made while another is still taking entries (a block in a `const` pattern) puts its entries
    /// between the outer scope's, and a range over the walk order would give the outer scope
    /// the inner one's names too.
    entries: Box<[[u32; 2]]>,
    scope_entries: Box<[ScopeEntry]>,
    // Cold: a block scope's block and label, a macro scope's macro.
    blocks: Box<[(BlockId, Option<LabelId>)]>,
    macro_defs: Box<[MacroDefId]>,
    scope_by_expr: ScopeByExpr,
}

/// A scope's kind, a column of one byte per scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ScopeKind {
    None,
    Block,
    Label,
    MacroDef,
}

const _: () = assert!(size_of::<ScopeKind>() == 1);
// 12 bytes an entry: a symbol, a hygiene and a binding id.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<ScopeEntry>() == 12);
// 12 bytes a block scope: an id (4) and an `Option` of an arena id, which has no niche.
const _: () = assert!(size_of::<(BlockId, Option<LabelId>)>() == 12);
const _: () = assert!(size_of::<ScopeId>() == 4);

#[derive(Debug, PartialEq, Eq)]
pub struct ScopeEntry {
    name: Name,
    hygiene: HygieneId,
    binding: BindingId,
}

impl ScopeEntry {
    pub fn name(&self) -> &Name {
        &self.name
    }

    pub fn hygiene(&self) -> HygieneId {
        self.hygiene
    }

    pub fn binding(&self) -> BindingId {
        self.binding
    }
}

/// The scope of each expression, by [`ExprId`]: 4 bytes an expression, a lookup one index.
/// Expressions past the last one given a scope are not in it.
#[derive(Debug, PartialEq, Eq)]
pub struct ScopeByExpr(Box<[ScopeId]>);

impl ScopeByExpr {
    #[inline]
    pub fn get(&self, expr: ExprId) -> Option<&ScopeId> {
        self.0.get(expr.into_raw().into_u32() as usize).filter(|it| **it != UNSCOPED)
    }

    /// Each expression with a scope and its scope, in the order of the expressions.
    pub fn iter(&self) -> impl Iterator<Item = (ExprId, &ScopeId)> + '_ {
        self.0.iter().enumerate().filter(|(_, it)| **it != UNSCOPED).map(|(at, it)| {
            (ExprId::from_raw(RawIdx::from_u32(at as u32)), it)
        })
    }
}

#[blaze_query::tracked]
impl ExprScopes {
    #[blaze_query::tracked(returns(ref))]
    pub fn body_expr_scopes(db: &Db, def: DefWithBodyId) -> ExprScopes {
        let body = Body::of(db, def);
        build(|scopes| walk_body(scopes, body))
    }

    #[blaze_query::tracked(returns(ref))]
    pub fn sig_expr_scopes(db: &Db, def: GenericDefId) -> ExprScopes {
        let (_, store) = GenericParams::with_store(db, def);
        build(|scopes| walk_store(scopes, store, store.expr_roots()))
    }

    #[blaze_query::tracked(returns(ref))]
    pub fn variant_scopes(db: &Db, def: VariantId) -> ExprScopes {
        let fields = VariantFields::of(db, def);
        build(|scopes| walk_store(scopes, &fields.store, fields.store.expr_roots()))
    }
}

impl ExprScopes {
    #[inline]
    pub fn of(db: &Db, def: impl Into<ExpressionStoreOwnerId>) -> &ExprScopes {
        match def.into() {
            ExpressionStoreOwnerId::Body(def) => Self::body_expr_scopes(db, def),
            ExpressionStoreOwnerId::Signature(def) => Self::sig_expr_scopes(db, def),
            ExpressionStoreOwnerId::VariantFields(variant_id) => {
                Self::variant_scopes(db, variant_id)
            }
        }
    }

    #[inline]
    pub fn entries(&self, scope: ScopeId) -> &[ScopeEntry] {
        let [start, end] = self.entries[index(scope)];
        &self.scope_entries[start as usize..end as usize]
    }

    /// If `scope` refers to a block expression scope, returns the corresponding `BlockId`.
    pub fn block(&self, scope: ScopeId) -> Option<BlockId> {
        let at = index(scope);
        match self.kind[at] {
            ScopeKind::Block => Some(self.blocks[self.data[at] as usize].0),
            ScopeKind::None | ScopeKind::Label | ScopeKind::MacroDef => None,
        }
    }

    /// If `scope` refers to a macro def scope, returns the corresponding `MacroId`.
    pub fn macro_def(&self, scope: ScopeId) -> Option<&MacroDefId> {
        let at = index(scope);
        match self.kind[at] {
            ScopeKind::MacroDef => Some(&self.macro_defs[self.data[at] as usize]),
            ScopeKind::None | ScopeKind::Block | ScopeKind::Label => None,
        }
    }

    /// If `scope` refers to a labeled expression scope, returns the corresponding `Label`.
    pub fn label(&self, scope: ScopeId) -> Option<LabelId> {
        let at = index(scope);
        match self.kind[at] {
            ScopeKind::Block => self.blocks[self.data[at] as usize].1,
            ScopeKind::Label => Some(LabelId::from_raw(RawIdx::from_u32(self.data[at]))),
            ScopeKind::None | ScopeKind::MacroDef => None,
        }
    }

    /// Returns the scopes in ascending order.
    pub fn scope_chain(&self, scope: Option<ScopeId>) -> impl Iterator<Item = ScopeId> + '_ {
        std::iter::successors(scope, move |&scope| {
            let parent = self.parent[index(scope)];
            (parent != NO_SCOPE).then(|| ScopeId::from_raw(RawIdx::from_u32(parent)))
        })
    }

    pub fn resolve_name_in_scope(&self, scope: ScopeId, name: &Name) -> Option<&ScopeEntry> {
        // A name a pattern binds twice (`a @ (Ok(a) | Err(a))`) means its last binding.
        self.scope_chain(Some(scope))
            .find_map(|scope| self.entries(scope).iter().rev().find(|it| it.name == *name))
    }

    #[inline]
    pub fn scope_for(&self, expr: ExprId) -> Option<ScopeId> {
        self.scope_by_expr.get(expr).copied()
    }

    pub fn scope_by_expr(&self) -> &ScopeByExpr {
        &self.scope_by_expr
    }

    /// How many scopes there are; they are numbered from 0.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.parent.len()
    }
}

#[inline]
fn index(scope: ScopeId) -> usize {
    scope.into_raw().into_u32() as usize
}

/// A scope as the walk makes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewScope {
    Plain,
    Block { id: BlockId, label: Option<LabelId> },
    Label(LabelId),
    MacroDef(MacroDefId),
}

impl NewScope {
    fn labeled(label: Option<LabelId>) -> NewScope {
        label.map_or(NewScope::Plain, NewScope::Label)
    }

    fn block(block: Option<BlockId>, label: Option<LabelId>) -> NewScope {
        match (block, label) {
            (Some(id), label) => NewScope::Block { id, label },
            (None, Some(label)) => NewScope::Label(label),
            (None, None) => NewScope::Plain,
        }
    }
}

/// Where the walk writes the scopes it makes: the columns, or the model the tests check them
/// against.
trait ScopeSink {
    fn push_scope(&mut self, parent: Option<ScopeId>, kind: NewScope) -> ScopeId;
    /// Adds an entry to `scope`, after its others.
    fn push_entry(&mut self, scope: ScopeId, entry: ScopeEntry);
    fn set_scope(&mut self, expr: ExprId, scope: ScopeId);
}

/// The columns of [`ExprScopes`] while they are built.
#[derive(Default)]
struct Building {
    parent: Vec<u32>,
    kind: Vec<ScopeKind>,
    data: Vec<u32>,
    /// Each entry beside the raw id of its scope, in walk order.
    scope_entries: Vec<(u32, ScopeEntry)>,
    blocks: Vec<(BlockId, Option<LabelId>)>,
    macro_defs: Vec<MacroDefId>,
    scope_by_expr: Vec<ScopeId>,
}

impl ScopeSink for Building {
    fn push_scope(&mut self, parent: Option<ScopeId>, kind: NewScope) -> ScopeId {
        let scope = ScopeId::from_raw(RawIdx::from_u32(self.parent.len() as u32));
        let (kind, data) = match kind {
            NewScope::Plain => (ScopeKind::None, 0),
            NewScope::Block { id, label } => {
                self.blocks.push((id, label));
                (ScopeKind::Block, self.blocks.len() as u32 - 1)
            }
            NewScope::Label(label) => (ScopeKind::Label, label.into_raw().into_u32()),
            NewScope::MacroDef(macro_def) => {
                self.macro_defs.push(macro_def);
                (ScopeKind::MacroDef, self.macro_defs.len() as u32 - 1)
            }
        };
        self.parent.push(parent.map_or(NO_SCOPE, |it| it.into_raw().into_u32()));
        self.kind.push(kind);
        self.data.push(data);
        scope
    }

    fn push_entry(&mut self, scope: ScopeId, entry: ScopeEntry) {
        self.scope_entries.push((scope.into_raw().into_u32(), entry));
    }

    fn set_scope(&mut self, expr: ExprId, scope: ScopeId) {
        let at = expr.into_raw().into_u32() as usize;
        if at >= self.scope_by_expr.len() {
            self.scope_by_expr.resize(at + 1, UNSCOPED);
        }
        self.scope_by_expr[at] = scope;
    }
}

impl Building {
    /// The columns, each copied into an allocation of its exact size, and `self` cleared.
    fn finish(&mut self) -> ExprScopes {
        // Stable, so each scope keeps its entries in the order they were added; a body with no
        // scope made inside another's entries is already grouped and skips the sort.
        if !self.scope_entries.is_sorted_by_key(|&(scope, _)| scope) {
            radix::sort_by_key(&mut self.scope_entries, |&(scope, _)| scope);
        }
        // alloc: the frozen column, once per store.
        let mut entries: Box<[[u32; 2]]> = vec![[0, 0]; self.parent.len()].into_boxed_slice();
        for (at, &(scope, _)) in self.scope_entries.iter().enumerate() {
            let range = &mut entries[scope as usize];
            if range[0] == range[1] {
                range[0] = at as u32;
            }
            range[1] = at as u32 + 1;
        }
        let scopes = ExprScopes {
            parent: Box::from(&*self.parent),
            kind: Box::from(&*self.kind),
            data: Box::from(&*self.data),
            entries,
            // alloc: the frozen column, once per store, exact size from the drain.
            scope_entries: self.scope_entries.drain(..).map(|(_, entry)| entry).collect(),
            blocks: Box::from(&*self.blocks),
            macro_defs: Box::from(&*self.macro_defs),
            scope_by_expr: ScopeByExpr(Box::from(&*self.scope_by_expr)),
        };
        let Building {
            parent,
            kind,
            data,
            scope_entries: _,
            blocks,
            macro_defs,
            scope_by_expr,
        } = self;
        parent.clear();
        kind.clear();
        data.clear();
        blocks.clear();
        macro_defs.clear();
        scope_by_expr.clear();
        scopes
    }
}

/// A scratch that grew past this many expressions or entries is let go of, so that one huge
/// body doesn't keep its memory.
const KEEP_UP_TO: usize = 1 << 20;

thread_local! {
    /// The columns of the last scopes built on this thread, cleared: building the next one
    /// grows nothing once they are as big as the biggest body.
    static SCRATCH: Cell<Option<Building>> = const { Cell::new(None) };
}

fn build(walk: impl FnOnce(&mut Building)) -> ExprScopes {
    let mut building = SCRATCH.take().unwrap_or_default();
    walk(&mut building);
    let scopes = building.finish();
    let small = building.scope_by_expr.capacity() <= KEEP_UP_TO
        && building.scope_entries.capacity() <= KEEP_UP_TO
        && building.parent.capacity() <= KEEP_UP_TO;
    if small {
        SCRATCH.set(Some(building));
    }
    scopes
}

fn walk_body(sink: &mut impl ScopeSink, body: &Body) {
    let root = sink.push_scope(None, NewScope::Plain);
    if let Some(Param { formal: self_param, user_written: _ }) = body.self_param {
        add_binding(&mut *sink, body, root, self_param);
    }
    let mut visitor = ExprScopeVisitor { store: body, scopes: sink, scope: root, const_scope: root };
    body.params.iter().for_each(|param| visitor.on_pat(param.formal));
    visitor.on_expr(body.root_expr());
}

fn walk_store(
    sink: &mut impl ScopeSink,
    store: &ExpressionStore,
    roots: impl IntoIterator<Item = ExprId>,
) {
    let root = sink.push_scope(None, NewScope::Plain);
    for root_expr in roots {
        let scope = sink.push_scope(Some(root), NewScope::Plain);
        ExprScopeVisitor { store, scopes: &mut *sink, scope, const_scope: scope }
            .on_expr(root_expr);
    }
}

fn add_binding(
    sink: &mut impl ScopeSink,
    store: &ExpressionStore,
    scope: ScopeId,
    binding: BindingId,
) {
    let Binding { name, .. } = &store[binding];
    let hygiene = store.binding_hygiene(binding);
    sink.push_entry(scope, ScopeEntry { name: *name, binding, hygiene });
}

struct ExprScopeVisitor<'a, S> {
    store: &'a ExpressionStore,
    scopes: &'a mut S,
    scope: ScopeId,
    const_scope: ScopeId,
}

impl<S: ScopeSink> ExprScopeVisitor<'_, S> {
    fn with_scope(&mut self, scope: ScopeId, f: impl FnOnce(&mut Self)) {
        let old_scope = mem::replace(&mut self.scope, scope);
        f(self);
        self.scope = old_scope;
    }

    fn new_scope(&mut self, parent: ScopeId, kind: NewScope) -> ScopeId {
        self.scopes.push_scope(Some(parent), kind)
    }

    fn visit_block(
        &mut self,
        expr: ExprId,
        id: Option<BlockId>,
        statements: Statements<'_>,
        tail: Option<ExprId>,
        label: Option<LabelId>,
    ) {
        let scope = self.new_scope(self.scope, NewScope::block(id, label));
        self.with_scope(scope, |this| {
            let old_const_scope = if id.is_some() {
                let const_scope = this.new_scope(this.const_scope, NewScope::block(id, None));
                mem::replace(&mut this.const_scope, const_scope)
            } else {
                // We don't need to allocate a new scope, since only items matter to us.
                this.const_scope
            };
            // Overwrite the old scope for the block expr, so that every block scope can be found
            // via the block itself (important for blocks that only contain items, no expressions).
            this.scopes.set_scope(expr, this.scope);

            for stmt in statements {
                match stmt {
                    Statement::Let { pat, initializer, else_branch, type_ref, .. } => {
                        this.on_type_opt(type_ref);
                        this.on_expr_opt(initializer);
                        this.on_expr_opt(else_branch);
                        this.scope = this.new_scope(this.scope, NewScope::Plain);
                        this.on_pat(pat);
                    }
                    Statement::Expr { expr, has_semi: _ } => this.on_expr(expr),
                    Statement::Item(Item::MacroDef(macro_id)) => {
                        let macro_def = NewScope::MacroDef(*macro_id);
                        this.scope = this.new_scope(this.scope, macro_def);
                        this.const_scope = this.new_scope(this.const_scope, macro_def);
                    }
                    Statement::Item(Item::Other) => (),
                }
            }
            this.on_expr_opt(tail);

            this.const_scope = old_const_scope;
        });
    }
}

impl<S: ScopeSink> StoreVisitor for ExprScopeVisitor<'_, S> {
    fn on_expr(&mut self, expr: ExprId) {
        self.scopes.set_scope(expr, self.scope);
        match self.store.expr(expr) {
            Expr::Block { statements, tail, id, label, unsafe_: _ } => {
                self.visit_block(expr, id, statements, tail, label);
            }
            Expr::Loop { body, label, source: _ } => {
                let scope = self.new_scope(self.scope, NewScope::labeled(label));
                self.with_scope(scope, |this| this.on_expr(body));
            }
            Expr::Closure { args, arg_types, ret_type, body, capture_by: _, closure_kind: _ } => {
                arg_types.iter().for_each(|type_ref| self.on_type_opt(*type_ref));
                self.on_type_opt(ret_type);
                let scope = self.new_scope(self.scope, NewScope::Plain);
                self.with_scope(scope, |this| {
                    this.on_pats(args);
                    this.on_expr(body);
                });
            }
            Expr::Match { expr, arms } => {
                self.on_expr(expr);
                for arm in arms {
                    let scope = self.new_scope(self.scope, NewScope::Plain);
                    self.with_scope(scope, |this| {
                        this.on_pat(arm.pat);
                        this.on_expr_opt(arm.guard);
                        this.on_expr(arm.expr);
                    });
                }
            }
            Expr::If { condition, then_branch, else_branch } => {
                let then_branch_scope = self.new_scope(self.scope, NewScope::Plain);
                self.with_scope(then_branch_scope, |this| {
                    this.on_expr(condition);
                    this.on_expr(then_branch);
                });
                self.on_expr_opt(else_branch);
            }
            Expr::Let { pat, expr } => {
                self.on_expr(expr);
                self.scope = self.new_scope(self.scope, NewScope::Plain);
                self.on_pat(pat);
            }
            _ => self.store.visit_expr_children(expr, self),
        }
    }

    fn on_anon_const_expr(&mut self, expr: ExprId) {
        self.with_scope(self.const_scope, |this| this.on_expr(expr));
    }

    fn on_pat(&mut self, pat: PatId) {
        if let Pat::Bind { id, .. } = self.store.pat(pat) {
            add_binding(&mut *self.scopes, self.store, self.scope, id);
        }

        self.store.visit_pat_children(pat, self);
    }

    fn on_type(&mut self, ty: TypeRefId) {
        let store = self.store;
        self.with_scope(self.const_scope, |this| store.visit_type_ref_children(ty, this));
    }
}

#[cfg(test)]
mod tests {
    use hir_expand::InFile;
    use itertools::Itertools as _;
    use la_arena::ArenaMap;
    use span::FileId;
    use syntax::{
        TextSize,
        view::{self, AstNode as _},
    };
    use hir_expand::fixture;
    use test_utils::{assert_eq_text, extract_offset};

    use super::*;
    use crate::{
        DefWithBodyId, FunctionId, ModuleDefId,
        expr_store::{Body, scope::ExprScopes},
        nameres::crate_def_map,
        test_db::TestDB,
    };

    /// The scopes as plain values: a parent, a kind with its payload and a list of entries per
    /// scope, an `ArenaMap` by expression.
    #[derive(Default)]
    struct Model {
        scopes: Vec<(Option<ScopeId>, NewScope, Vec<ScopeEntry>)>,
        scope_by_expr: ArenaMap<ExprId, ScopeId>,
    }

    impl ScopeSink for Model {
        fn push_scope(&mut self, parent: Option<ScopeId>, kind: NewScope) -> ScopeId {
            self.scopes.push((parent, kind, Vec::new()));
            ScopeId::from_raw(RawIdx::from_u32(self.scopes.len() as u32 - 1))
        }

        fn push_entry(&mut self, scope: ScopeId, entry: ScopeEntry) {
            self.scopes[index(scope)].2.push(entry);
        }

        fn set_scope(&mut self, expr: ExprId, scope: ScopeId) {
            self.scope_by_expr.insert(expr, scope);
        }
    }

    impl Model {
        fn chain(&self, scope: ScopeId) -> Vec<ScopeId> {
            std::iter::successors(Some(scope), |&it| self.scopes[index(it)].0).collect()
        }

        fn entries(&self, scope: ScopeId) -> &[ScopeEntry] {
            &self.scopes[index(scope)].2
        }

        fn kind(&self, scope: ScopeId) -> NewScope {
            self.scopes[index(scope)].1
        }

        fn resolve(&self, scope: ScopeId, name: &Name) -> Option<&ScopeEntry> {
            self.chain(scope)
                .into_iter()
                .find_map(|it| self.entries(it).iter().rev().find(|entry| entry.name == *name))
        }
    }

    /// Every query of `scopes` answers as the model the same walk built does.
    fn assert_matches_model(scopes: &ExprScopes, model: &Model) {
        assert_eq!(scopes.len(), model.scopes.len(), "the number of scopes");
        let all =
            || (0..model.scopes.len() as u32).map(|at| ScopeId::from_raw(RawIdx::from_u32(at)));
        let names: Vec<&Name> =
            model.scopes.iter().flat_map(|(_, _, entries)| entries).map(|it| &it.name).collect();
        for scope in all() {
            assert_eq!(scopes.scope_chain(Some(scope)).collect::<Vec<_>>(), model.chain(scope));
            assert_eq!(scopes.entries(scope), model.entries(scope), "the entries of {scope:?}");
            let (block, label, macro_def) = match model.kind(scope) {
                NewScope::Plain => (None, None, None),
                NewScope::Block { id, label } => (Some(id), label, None),
                NewScope::Label(label) => (None, Some(label), None),
                NewScope::MacroDef(it) => (None, None, Some(it)),
            };
            assert_eq!(scopes.block(scope), block, "the block of {scope:?}");
            assert_eq!(scopes.label(scope), label, "the label of {scope:?}");
            assert_eq!(scopes.macro_def(scope).copied(), macro_def, "the macro of {scope:?}");
            for name in &names {
                let resolved = scopes.resolve_name_in_scope(scope, name);
                assert_eq!(resolved, model.resolve(scope, name), "{name:?} in {scope:?}");
            }
        }
        let by_expr: Vec<(ExprId, ScopeId)> =
            scopes.scope_by_expr().iter().map(|(expr, &scope)| (expr, scope)).collect();
        let model_by_expr: Vec<(ExprId, ScopeId)> =
            model.scope_by_expr.iter().map(|(expr, &scope)| (expr, scope)).collect();
        assert_eq!(by_expr, model_by_expr, "the scope of each expression");
        let past_the_last = model_by_expr.last().map_or(0, |(it, _)| it.into_raw().into_u32() + 2);
        for raw in 0..past_the_last {
            let expr = ExprId::from_raw(RawIdx::from_u32(raw));
            assert_eq!(scopes.scope_for(expr), model.scope_by_expr.get(expr).copied(), "{expr:?}");
        }
    }

    /// The scopes of `function`'s body and of its signature against the model, and built again
    /// on this thread's reused scratch, after another body's, the same.
    fn check_model(db: &TestDB, function: FunctionId) {
        let def = DefWithBodyId::from(function);
        let body = Body::of(db, def);
        let scopes = ExprScopes::body_expr_scopes(db, def);
        let mut model = Model::default();
        walk_body(&mut model, body);
        assert_matches_model(scopes, &model);

        let generic_def = GenericDefId::from(function);
        let (_, store) = GenericParams::with_store(db, generic_def);
        let mut model = Model::default();
        walk_store(&mut model, store, store.expr_roots());
        assert_matches_model(ExprScopes::sig_expr_scopes(db, generic_def), &model);

        let other = build(|it| walk_store(it, store, store.expr_roots()));
        assert_matches_model(&other, &model);
        assert_eq!(&build(|it| walk_body(it, body)), scopes, "built again on a reused scratch");
    }

    fn find_function(db: &TestDB, file_id: FileId) -> FunctionId {
        let krate = fixture::test_crate(db);
        let crate_def_map = crate_def_map(db, krate);

        let module = crate_def_map.modules_for_file(db, file_id).next().unwrap();
        let (_, def) = crate_def_map.module(db, module).scope(db).entries().next().unwrap();
        match def.take_values().unwrap() {
            ModuleDefId::FunctionId(it) => it,
            _ => panic!(),
        }
    }

    /// The shortest `N` at `offset`, as `syntax::algo::find_node_at_offset` finds it.
    fn find_at<'t, N: view::AstNode<'t>>(file: view::Node<'t>, offset: TextSize) -> Option<N> {
        file.token_at_offset(offset)
            .map(|token| token.parent_ancestors())
            .kmerge_by(|a, b| a.text_range().len() < b.text_range().len())
            .find_map(N::cast)
    }

    fn do_check(#[rust_analyzer::rust_fixture] ra_fixture: &str, expected: &[&str]) {
        let (offset, code) = extract_offset(ra_fixture);
        let code = {
            let mut buf = String::new();
            let off: usize = offset.into();
            buf.push_str(&code[..off]);
            buf.push_str("$0marker");
            buf.push_str(&code[off..]);
            buf
        };

        let fx = fixture::db(&code);
        let position = fx.position();
        let db = fx.into_db();
        let editioned_file_id = position.file_id;
        let offset = position.offset;

        let (file_id, _) = editioned_file_id.unpack(&db);

        let parse = editioned_file_id.parse(&db);
        let marker = find_at::<view::PathExpr<'_>>(parse.root(), offset);
        let function = find_function(&db, file_id);

        let scopes = ExprScopes::of(&db, DefWithBodyId::from(function));
        let (body, source_map) = Body::with_source_map(&db, function.into());

        let expr_id = marker
            .and_then(|marker| {
                source_map
                    .node_expr_view(InFile::new(editioned_file_id.into(), marker.into()))
                    .and_then(|expr| expr.as_expr())
            })
            .or_else(|| {
                body.exprs().find_map(|(expr, value)| {
                    let crate::hir::Expr::Path(path) = value else { return None };
                    path.mod_path()
                        .and_then(|path| path.as_ident())
                        .is_some_and(|name| name.as_str() == "marker")
                        .then_some(expr)
                })
            })
            .unwrap();
        let scope = scopes.scope_for(expr_id);

        let actual = scopes
            .scope_chain(scope)
            .flat_map(|scope| scopes.entries(scope))
            .map(|it| it.name().as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let expected = expected.join("\n");
        assert_eq_text!(&expected, &actual);
        check_model(&db, function);
    }

    #[test]
    fn every_scope_kind_matches_the_model() {
        let fx = fixture::db(
            r#"
fn f<const N: usize>(p: u32, s: [u8; { let k = 1; k }]) {
    macro_rules! m { () => { 1 } }
    'a: loop { break 'a; }
    let x = 'b: { let y = 1; y };
    match p {
        q @ (0 | 1) if let Some(r) = Some(q) => {}
        _ => {}
    }
    let (a, const { let w = 2; w }, b) = (1, 2, 3);
    let _ = |e: u32, [g, ..]: [u32; 1]| e + g;
    while let Some(h) = None::<u32> {}
    {
        fn inner() {}
        let i = m!();
        $0
    }
}
"#,
        );
        let position = fx.position();
        let db = fx.into_db();
        let (file_id, _) = position.file_id.unpack(&db);
        check_model(&db, find_function(&db, file_id));
    }

    #[test]
    fn type_anon_const_scope() {
        do_check(
            r#"
fn f(param: usize) {
    let local = 0;
    let _: [(); $0] = [];
}
"#,
            &["param"],
        );
    }

    #[test]
    fn pattern_type_expr_scope() {
        do_check(
            r#"
fn f(param: usize) {
    let local = 0;
    let _: builtin#pattern_type (usize is 0..=$0) = 0;
}
"#,
            &["param"],
        );
    }

    #[test]
    fn closure_pattern_type_expr_scope() {
        do_check(
            r#"
fn f(param: usize) {
    let local = 0;
    let _ = |_: builtin#pattern_type (usize is 0..=$0)| {};
}
"#,
            &["param"],
        );
    }

    #[test]
    fn array_repeat_expr_scope() {
        do_check(
            r#"
fn f(param: usize) {
    let local = 0;
    let _ = [(); $0];
}
"#,
            &["param"],
        );
    }

    #[test]
    fn inline_const_expr_scope() {
        do_check(
            r#"
fn f(param: usize) {
    let local = 0;
    let _ = const { $0 };
}
"#,
            &["param"],
        );
    }

    #[test]
    fn test_lambda_scope() {
        do_check(
            r"
            fn quux(foo: i32) {
                let f = |bar, baz: i32| {
                    $0
                };
            }",
            &["bar", "baz", "foo"],
        );
    }

    #[test]
    fn test_call_scope() {
        do_check(
            r"
            fn quux() {
                f(|x| $0 );
            }",
            &["x"],
        );
    }

    #[test]
    fn test_method_call_scope() {
        do_check(
            r"
            fn quux() {
                z.f(|x| $0 );
            }",
            &["x"],
        );
    }

    #[test]
    fn test_loop_scope() {
        do_check(
            r"
            fn quux() {
                loop {
                    let x = ();
                    $0
                };
            }",
            &["x"],
        );
    }

    #[test]
    fn test_match() {
        do_check(
            r"
            fn quux() {
                match () {
                    Some(x) => {
                        $0
                    }
                };
            }",
            &["x"],
        );
    }

    #[test]
    fn test_shadow_variable() {
        do_check(
            r"
            fn foo(x: String) {
                let x : &str = &x$0;
            }",
            &["x"],
        );
    }

    #[test]
    fn test_bindings_after_at() {
        do_check(
            r"
fn foo() {
    match Some(()) {
        opt @ Some(unit) => {
            $0
        }
        _ => {}
    }
}
",
            &["opt", "unit"],
        );
    }

    #[test]
    fn macro_inner_item() {
        do_check(
            r"
            macro_rules! mac {
                () => {{
                    fn inner() {}
                    inner();
                }};
            }

            fn foo() {
                mac!();
                $0
            }
        ",
            &[],
        );
    }

    #[test]
    fn broken_inner_item() {
        do_check(
            r"
            fn foo() {
                trait {}
                $0
            }
        ",
            &[],
        );
    }

    fn do_check_local_name(#[rust_analyzer::rust_fixture] ra_fixture: &str, expected_offset: u32) {
        let fx = fixture::db(ra_fixture);
        let position = fx.position();
        let db = fx.into_db();
        let editioned_file_id = position.file_id;
        let offset = position.offset;

        let (file_id, _) = editioned_file_id.unpack(&db);

        let parse = editioned_file_id.parse(&db);
        assert!(parse.errors().is_empty(), "{:?}", parse.errors());
        let file = parse.root();
        let expected_name = find_at::<view::Name<'_>>(file, expected_offset.into())
            .expect("failed to find a name at the target offset");
        let name_ref = find_at::<view::NameRef<'_>>(file, offset).unwrap();

        let function = find_function(&db, file_id);

        let scopes = ExprScopes::body_expr_scopes(&db, DefWithBodyId::from(function));
        let (_, source_map) = Body::with_source_map(&db, function.into());

        let expr_scope = {
            let expr = name_ref.syntax().ancestors().find_map(view::Expr::cast).unwrap();
            let expr_id = source_map
                .node_expr_view(InFile::new(editioned_file_id.into(), expr))
                .unwrap()
                .as_expr()
                .unwrap();
            scopes.scope_for(expr_id).unwrap()
        };

        let name = Name::new_root(name_ref.text());
        let resolved = scopes.resolve_name_in_scope(expr_scope, &name).unwrap();
        let pat_src =
            source_map.pat_syntax(source_map.patterns_for_binding(resolved.binding())[0]).unwrap();

        let local_name = view::to_node(pat_src.value.syntax_node_ptr(), file);
        assert_eq!(local_name.text_range(), expected_name.syntax().text_range());
        check_model(&db, function);
    }

    #[test]
    fn test_resolve_local_name() {
        do_check_local_name(
            r#"
fn foo(x: i32, y: u32) {
    {
        let z = x * 2;
    }
    {
        let t = x$0 * 3;
    }
}
"#,
            7,
        );
    }

    #[test]
    fn a_const_pattern_block_keeps_its_names() {
        do_check(
            r"
            fn quux() {
                let w = 1;
                let (a, const { let w = (); w }, b) = (1, (), 3);
                $0
            }",
            &["a", "b", "w"],
        );
    }

    #[test]
    fn a_name_after_a_const_pattern_block_resolves_outside_it() {
        do_check_local_name(
            r#"
fn foo() {
    let w = 1;
    let (a, const { let w = (); w }, b) = (1, (), 3);
    w$0
}
"#,
            19,
        );
    }

    #[test]
    fn test_resolve_local_name_declaration() {
        do_check_local_name(
            r#"
fn foo(x: String) {
    let x : &str = &x$0;
}
"#,
            7,
        );
    }

    #[test]
    fn test_resolve_local_name_shadow() {
        do_check_local_name(
            r"
fn foo(x: String) {
    let x : &str = &x;
    x$0
}
",
            28,
        );
    }

    #[test]
    fn ref_patterns_contribute_bindings() {
        do_check_local_name(
            r"
fn foo() {
    if let Some(&from) = bar() {
        from$0;
    }
}
",
            28,
        );
    }

    #[test]
    fn while_let_adds_binding() {
        do_check_local_name(
            r#"
fn test() {
    let foo: Option<f32> = None;
    while let Option::Some(spam) = foo {
        spam$0
    }
}
"#,
            75,
        );
        do_check_local_name(
            r#"
fn test() {
    let foo: Option<f32> = None;
    while (((let Option::Some(_) = foo))) && let Option::Some(spam) = foo {
        spam$0
    }
}
"#,
            107,
        );
    }

    #[test]
    fn match_guard_if_let() {
        do_check_local_name(
            r#"
fn test() {
    let foo: Option<f32> = None;
    match foo {
        _ if let Option::Some(spam) = foo => spam$0,
    }
}
"#,
            93,
        );
    }

    #[test]
    fn let_chains_can_reference_previous_lets() {
        do_check_local_name(
            r#"
fn test() {
    let foo: Option<i32> = None;
    if let Some(spam) = foo && spa$0m > 1 && let Some(spam) = foo && spam > 1 {}
}
"#,
            61,
        );
        do_check_local_name(
            r#"
fn test() {
    let foo: Option<i32> = None;
    if let Some(spam) = foo && spam > 1 && let Some(spam) = foo && sp$0am > 1 {}
}
"#,
            100,
        );
    }
}
