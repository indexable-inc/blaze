//! What is live: from the seeds, each live definition's signature and body walked, every path,
//! method call, field read and pattern in them making what it names live, with rustc's rules: a
//! variant in a pattern is not constructed, nor one a match arm's pattern needs within that arm;
//! a field written is not read, nor one of a struct expression (but of a union's); the fields of
//! a `repr(C)` or `repr(transparent)` type, and the public fields of a public type, are read; a
//! trait's associated types are live with it, and an inherent impl's items make its self type
//! live; the items of a trait impl are live once their trait's item and self type are, and the
//! impls of `#[derive(Clone)]` and `#[derive(Debug)]` read nothing. On the way, an assignment of
//! a variable or a field to itself is linted.
//!
//! A body (of a function, a constant or a static) is walked as the front end lowered it
//! ([`body`]): each name read from what inference recorded, or resolved with one resolver kept
//! at the scope being walked. A signature, and what is in it (an enum's discriminants, the
//! constants in its types), is walked over its syntax, as rustc's `intravisit` walks its HIR,
//! its macro calls expanded and names resolved through `hir::view::Semantics`. Either way a trait
//! impl's item named through a trait is mapped back to the trait's item as typeck has it, each
//! path is resolved once, and what is live and what was walked are bitsets over the
//! definitions.
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`MarkSymbolVisitor`, `mark_live_symbols`,
//! `check_impl_or_impl_item_live`, `live_symbols_and_ignored_derived_traits`,
//! `check_for_self_assign`), `compiler/rustc_hir/src/pat_util.rs` (`necessary_variants`) and
//! `compiler/rustc_builtin_macros/src/deriving/` (what each derive's method reads)
//! (MIT or Apache-2.0).

mod body;

use blaze_hash::{HashMap, HashSet};
use either::Either;
use hir::{HirFileId, InFile, ModuleDef, PathResolution};
use hir_def::{
    AdtId, AttrDefId, DefWithBodyId, FieldId, attrs::AttrFlags,
    builtin_derive::BuiltinDeriveImplTrait, expr_store::Body,
};
use smallvec::SmallVec;
use syntax::{
    SyntaxKind, SyntaxNodePtr, T,
    view::{self as ast, AstNode as _, BinaryOp, HasArgList as _, RangeItem as _},
};

use super::{
    defs::{DefIdx, DefSet, Id, Kind, Parent, anon_key, is_anon},
    res::Res,
    seed::{Crate, Seeds, Unsolved, WorkItem},
};
use crate::{
    lower::{Found, Phase},
    model::Diag,
};

/// What is live at one point of the pass.
#[derive(Debug, Default)]
pub(super) struct Live {
    live: DefSet,
    /// The closures and anonymous constants walked, which items in them have as their parent.
    anon: HashSet<InFile<SyntaxNodePtr>>,
    /// rustc's `ignored_derived_traits`: of each ADT, the traits of the derives whose impls
    /// were not walked, in the order they were met.
    pub(super) ignored: HashMap<DefIdx, SmallVec<[&'static str; 2]>>,
}

impl Live {
    /// A copy, for rustc's `pre_deferred_seeding`: taken once per crate.
    fn snapshot(&self) -> Live {
        Live {
            live: self.live.clone(), // clone: once per crate, rustc's pre-deferred summary
            anon: self.anon.clone(), // clone: once per crate, rustc's pre-deferred summary
            ignored: self.ignored.clone(), // clone: once per crate, rustc's pre-deferred summary
        }
    }

    pub(super) fn contains(&self, def: DefIdx) -> bool {
        self.live.contains(def)
    }

    pub(super) fn parent_live(&self, parent: Parent) -> bool {
        match parent {
            Parent::Module => true,
            Parent::Def(it) => self.live.contains(it),
            Parent::Anon(it) => self.anon.contains(&it),
        }
    }
}

/// rustc's `DeadCodeLivenessSummary`: what is live before what is public and what an `allow` or
/// `expect` covers are walked (for `dead_code_pub_in_binary`), and at the end.
pub(super) struct Liveness {
    pub(super) pre_deferred: Live,
    pub(super) last: Live,
}

/// rustc's `live_symbols_and_ignored_derived_traits`, with the self-assignments it lints.
pub(super) fn live(k: &Crate<'_, '_, '_>, seeds: Seeds) -> (Liveness, Vec<Found>) {
    let len = k.defs.len();
    let mut m = Mark {
        k,
        worklist: seeds.worklist,
        scanned: [DefSet::new(len), DefSet::new(len)],
        live: Live { live: DefSet::new(len), ..Live::default() },
        propagated: false,
        in_pat: false,
        ignore_variants: Vec::new(), // alloc: setup, one stack for the whole walk
        repr_all_live: false,
        repr_simd: false,
        self_assigns: HashSet::default(),
        found: Vec::new(), // alloc: the pass's output, once per crate
    };
    let mut unsolved = seeds.unsolved;
    m.mark_with_unsolved(&mut unsolved);
    let pre_deferred = m.live.snapshot();
    m.worklist.extend(seeds.deferred);
    m.mark_with_unsolved(&mut unsolved);
    (Liveness { pre_deferred, last: m.live }, m.found)
}

struct Mark<'k, 'c, 'a, 'db> {
    k: &'k Crate<'c, 'a, 'db>,
    worklist: Vec<WorkItem>,
    /// rustc's `scanned` of `(id, comes_from_allow)`: one bitset for each `comes_from_allow`.
    scanned: [DefSet; 2],
    live: Live,
    propagated: bool,
    in_pat: bool,
    ignore_variants: Vec<DefIdx>,
    repr_all_live: bool,
    repr_simd: bool,
    self_assigns: HashSet<InFile<SyntaxNodePtr>>,
    found: Vec<Found>,
}

impl<'db> Mark<'_, '_, '_, 'db> {
    /// rustc's `own_comes_from_allow_expect`.
    fn own(&self, def: DefIdx) -> bool {
        self.propagated && self.k.allow_or_lang(def) == Some(true)
    }

    fn check_def_id(&mut self, def: DefIdx) {
        let own = self.own(def);
        if self.k.defs[def].kind != Kind::Field {
            self.worklist.push(WorkItem { id: def, propagated: self.propagated, own });
        }
        if !own {
            self.live.live.insert(def);
        }
    }

    fn insert_def_id(&mut self, def: DefIdx) {
        if !self.own(def) {
            self.live.live.insert(def);
        }
    }

    /// rustc's `handle_res`: in a pattern, only a constant or a type alias it names is used.
    fn handle_res(&mut self, res: Res) {
        match res {
            Res::Def(def) => {
                let in_pat_used = matches!(self.k.defs[def].kind, Kind::Const | Kind::AssocConst | Kind::TyAlias);
                if !self.in_pat || in_pat_used {
                    self.check_def_id(def);
                }
            }
            Res::Variant(variant) => {
                if self.in_pat {
                    return;
                }
                if let Parent::Def(adt) = self.k.defs[variant].parent {
                    self.check_def_id(adt);
                }
                if !self.ignore_variants.contains(&variant) {
                    self.check_def_id(variant);
                }
            }
            Res::SelfCtor | Res::None => {}
        }
    }

    /// `mark_live_symbols_and_ignored_derived_traits`: the worklist, then the trait impls and
    /// their items whose trait and self type are live, until none is left that is.
    fn mark_with_unsolved(&mut self, unsolved: &mut Vec<Unsolved>) {
        self.mark_live_symbols();
        let mut to_check = Vec::new(); // alloc: setup, one buffer per fixpoint, reused each round
        self.extract_live(unsolved, &mut to_check);
        while !to_check.is_empty() {
            self.worklist.extend(to_check.drain(..).map(|id| WorkItem::new(id, false)));
            self.mark_live_symbols();
            self.extract_live(unsolved, &mut to_check);
        }
    }

    /// rustc's `check_impl_or_impl_item_live` of each unsolved item, the live ones moved to
    /// `out`: two bit tests each.
    fn extract_live(&self, unsolved: &mut Vec<Unsolved>, out: &mut Vec<DefIdx>) {
        let live = &self.live.live;
        unsolved.retain(|it| {
            let is_live = it.trait_def.is_none_or(|it| live.contains(it)) && it.self_adt.is_none_or(|it| live.contains(it));
            if is_live {
                out.push(it.def);
            }
            !is_live
        });
    }

    fn mark_live_symbols(&mut self) {
        while let Some(work) = self.worklist.pop() {
            self.propagated = work.propagated;
            if !work.own {
                self.live.live.insert(work.id);
            }
            if !self.scanned[work.propagated as usize].insert(work.id) {
                continue;
            }
            self.visit(work.id);
        }
    }

    /// rustc's `visit_node` of a definition taken from the worklist.
    fn visit(&mut self, def: DefIdx) {
        let k = self.k;
        let db = k.cx.db;
        let defs = k.defs;
        let d = &defs[def];
        match d.id {
            Id::DeriveMethod(derive) => {
                let loc = derive.loc(db);
                let Some(adt) = defs.get(Id::Adt(loc.adt)) else { return };
                self.derived(adt, loc.trait_);
                return;
            }
            Id::Derive(_) => {
                if let Some(adt) = defs.impl_of(def).and_then(|it| it.self_adt) {
                    self.check_def_id(adt);
                }
                return;
            }
            Id::Variant(_) | Id::Field(_) => return,
            _ => {}
        }
        if let Parent::Def(impl_) = d.parent
            && let Some(&name) = defs.trivial_derives.get(&impl_)
        {
            if let Some(adt) = defs.impl_of(impl_).and_then(|it| it.self_adt) {
                self.ignore(adt, name);
            }
            return;
        }
        let Some(ptr) = d.node else { return };
        let saved = (self.repr_all_live, self.repr_simd);
        self.repr_all_live = false;
        self.repr_simd = false;
        match (d.kind, d.id) {
            (Kind::Struct | Kind::Union, Id::Adt(adt)) => {
                if let Some(repr) = AttrFlags::repr(db, adt) {
                    self.repr_all_live = repr.c() || repr.transparent();
                    self.repr_simd = repr.simd();
                }
                self.variant_data(def, def);
            }
            (Kind::Enum, _) => {
                for &variant in defs.children(def) {
                    self.variant_data(def, variant);
                }
            }
            (Kind::Trait, _) => {
                for &item in defs.children(def) {
                    if defs[item].kind == Kind::AssocTy {
                        self.check_def_id(item);
                    }
                }
            }
            _ => {}
        }
        if let Parent::Def(parent) = d.parent {
            match defs[parent].kind {
                Kind::Trait => self.check_def_id(parent),
                Kind::Impl { of_trait: false } => self.inherent_self_ty(parent),
                _ => {}
            }
        }
        let node = k.cx.syntax_view(ptr);
        if let Some(owner) = self.signature(node.file_id, node.value, d.id) {
            self.body(owner);
        }
        (self.repr_all_live, self.repr_simd) = saved;
    }

    /// The walk of a definition's signature, over its syntax. The body of a function, a
    /// constant or a static, when one is written, is left out and its owner given, for
    /// [`Mark::body`].
    fn signature(&mut self, file: HirFileId, node: ast::Node<'db>, id: Id) -> Option<DefWithBodyId> {
        let written = |kind: fn(SyntaxKind) -> bool| node.children().find(|it| kind(it.kind()));
        let found = match id {
            Id::Fn(it) => {
                written(|kind| kind == SyntaxKind::BLOCK_EXPR).map(|body| (DefWithBodyId::FunctionId(it), body))
            }
            Id::Const(it) => written(ast::Expr::can_cast).map(|body| (DefWithBodyId::ConstId(it), body)),
            Id::Static(it) => written(ast::Expr::can_cast).map(|body| (DefWithBodyId::StaticId(it), body)),
            _ => None,
        };
        let Some((owner, body)) = found else {
            self.walk(file, node, true);
            return None;
        };
        for child in node.children() {
            if child == body {
                continue;
            }
            if child.kind() != SyntaxKind::PARAM_LIST {
                stdx::ensure_sufficient_stack(|| self.walk(file, child, false));
                continue;
            }
            for param in child.children() {
                if param.kind() != SyntaxKind::PARAM {
                    self.walk(file, param, false);
                    continue;
                }
                for part in param.children() {
                    if !ast::Pat::can_cast(part.kind()) {
                        stdx::ensure_sufficient_stack(|| self.walk(file, part, false));
                    }
                }
            }
        }
        Some(owner)
    }

    /// The walk of a body as the front end lowered it ([`body::Walk`]).
    fn body(&mut self, owner: DefWithBodyId) {
        let (body, source_map) = Body::with_source_map(self.k.cx.db, owner);
        let body: &Body = body;
        body::Walk::new(self, owner, body, source_map).run(body);
    }

    /// An inherent impl's item makes its self type live: an ADT, a foreign type, the principal
    /// trait of a `dyn`.
    fn inherent_self_ty(&mut self, impl_: DefIdx) {
        let named = self.k.defs.impl_of(impl_).and_then(|it| it.self_adt.or(it.self_dyn));
        if let Some(it) = named {
            self.check_def_id(it);
        }
    }

    /// rustc's `visit_variant_data`: the fields of `variant` (of the ADT `adt`) live whatever
    /// reads them: all of a `repr(C)` or `repr(transparent)` type's, the positional ones of a
    /// `repr(simd)` type's, the public ones of a public type's.
    fn variant_data(&mut self, adt: DefIdx, variant: DefIdx) {
        let defs = self.k.defs;
        for &field in defs.children(variant) {
            let live = self.repr_all_live
                || (self.repr_simd && defs[field].is_positional())
                || (self.k.reach.is_reachable(adt) && self.k.reach.is_reachable(field));
            if live {
                self.live.live.insert(field);
            }
        }
    }

    /// What the method of a builtin derive's impl reads, as the code the derive writes does:
    /// `Clone` and `Debug` are `rustc_trivial_field_reads` and not walked; `PartialEq`,
    /// `PartialOrd`, `Ord` and `Hash` read every field; `Default` constructs the struct, or the
    /// enum's `#[default]` variant.
    fn derived(&mut self, adt: DefIdx, trait_: BuiltinDeriveImplTrait) {
        let defs = self.k.defs;
        match trait_ {
            BuiltinDeriveImplTrait::Clone | BuiltinDeriveImplTrait::Debug => {
                let name = match trait_ {
                    BuiltinDeriveImplTrait::Clone => "Clone",
                    _ => "Debug",
                };
                self.ignore(adt, name);
            }
            BuiltinDeriveImplTrait::PartialEq
            | BuiltinDeriveImplTrait::PartialOrd
            | BuiltinDeriveImplTrait::Ord
            | BuiltinDeriveImplTrait::Hash => match defs[adt].kind {
                Kind::Enum => {
                    for &variant in defs.children(adt) {
                        for &field in defs.children(variant) {
                            self.insert_def_id(field);
                        }
                    }
                }
                _ => {
                    for &field in defs.children(adt) {
                        self.insert_def_id(field);
                    }
                }
            },
            BuiltinDeriveImplTrait::Default => {
                let db = self.k.cx.db;
                let default = defs.children(adt).iter().copied().find(|&variant| match defs[variant].id {
                    Id::Variant(id) => AttrFlags::has_word(db, AttrDefId::EnumVariantId(id), "default"),
                    _ => false,
                });
                match default {
                    Some(variant) => self.handle_res(Res::Variant(variant)),
                    None => self.check_def_id(adt),
                }
            }
            BuiltinDeriveImplTrait::Copy
            | BuiltinDeriveImplTrait::Eq
            | BuiltinDeriveImplTrait::CoerceUnsized
            | BuiltinDeriveImplTrait::DispatchFromDyn
            | BuiltinDeriveImplTrait::Reborrow => {}
        }
    }

    /// `should_ignore_item` of a method of a derive of a `rustc_trivial_field_reads` trait: not
    /// walked, the trait kept for `adt`'s note.
    fn ignore(&mut self, adt: DefIdx, name: &'static str) {
        let ignored = self.live.ignored.entry(adt).or_default();
        if !ignored.contains(&name) {
            ignored.push(name);
        }
    }

    /// rustc's `intravisit` walk of `node` with the visitor's methods: the items in it are
    /// definitions of their own and not walked, nor attributes; macro calls are walked as what
    /// they expand to.
    fn walk(&mut self, file: HirFileId, node: ast::Node<'db>, root: bool) {
        let kind = node.kind();
        match kind {
            SyntaxKind::ATTR => return,
            SyntaxKind::MACRO_CALL => {
                let in_item_list = node.parent().is_some_and(|it| {
                    matches!(
                        it.kind(),
                        SyntaxKind::SOURCE_FILE
                            | SyntaxKind::ITEM_LIST
                            | SyntaxKind::ASSOC_ITEM_LIST
                            | SyntaxKind::EXTERN_ITEM_LIST
                            | SyntaxKind::MACRO_ITEMS
                    )
                });
                if in_item_list && !root {
                    return;
                }
                if let Some(call) = ast::MacroCall::cast(node)
                    && let Some(expansion) = self.k.cx.vsema.expand_macro_call(call)
                {
                    stdx::ensure_sufficient_stack(|| self.walk(expansion.file_id, expansion.value, false));
                }
                return;
            }
            _ if !root && ast::Item::can_cast(kind) => return,
            _ => {}
        }
        if is_anon(node) {
            self.anon(file, node);
            return;
        }
        if ast::Pat::can_cast(kind) {
            self.pat(file, node);
            return;
        }
        match kind {
            SyntaxKind::PATH => {
                if let Some(path) = ast::Path::cast(node) {
                    self.path(path);
                }
            }
            SyntaxKind::METHOD_CALL_EXPR => {
                if let Some(call) = ast::MethodCallExpr::cast(node)
                    && let Some(function) = self.k.cx.vsema.resolve_method_call(call)
                {
                    self.handle_res(Res::function(self.k.defs, function));
                }
            }
            SyntaxKind::FIELD_EXPR => {
                if let Some(field) = ast::FieldExpr::cast(node) {
                    self.field_read(field);
                }
            }
            SyntaxKind::RECORD_EXPR => {
                if let Some(expr) = ast::RecordExpr::cast(node) {
                    self.union_fields(expr);
                }
            }
            SyntaxKind::OFFSET_OF_EXPR => {
                if let Some(expr) = ast::OffsetOfExpr::cast(node) {
                    for name in expr.fields() {
                        if let Some((Either::Right(field), _)) = self.k.cx.vsema.resolve_offset_of_field(name)
                            && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
                        {
                            self.insert_def_id(field);
                        }
                    }
                }
            }
            SyntaxKind::BIN_EXPR => {
                if let Some(expr) = ast::BinExpr::cast(node)
                    && let Some(BinaryOp::Assignment { op: None }) = expr.op_kind()
                {
                    self.assign(file, expr);
                    return;
                }
            }
            SyntaxKind::MATCH_ARM => {
                let len = self.ignore_variants.len();
                if let Some(pat) = ast::MatchArm::cast(node).and_then(|it| it.pat()) {
                    let mut stack = std::mem::take(&mut self.ignore_variants);
                    self.necessary_variants(pat.syntax(), &mut stack, len);
                    self.ignore_variants = stack;
                }
                self.children(file, node);
                self.ignore_variants.truncate(len);
                return;
            }
            _ => {}
        }
        self.children(file, node);
    }

    fn children(&mut self, file: HirFileId, node: ast::Node<'db>) {
        for child in node.children() {
            stdx::ensure_sufficient_stack(|| self.walk(file, child, false));
        }
    }

    /// A closure (live unless an `allow` it has came with what reached it) or an anonymous
    /// constant (live), walked out of any pattern it is in.
    fn anon(&mut self, file: HirFileId, node: ast::Node<'db>) {
        let closure = match node.kind() {
            SyntaxKind::CLOSURE_EXPR => true,
            SyntaxKind::BLOCK_EXPR => !node.children_with_tokens().any(|it| it.kind() == T![const]),
            _ => false,
        };
        if self.k.defs.in_anon {
            let own = closure
                && self.propagated
                && self.k.levels.at(self.k.cx, InFile::new(file, node)).is_allow_or_expect();
            if !own {
                self.live.anon.insert(anon_key(file, node));
            }
        }
        let in_pat = std::mem::replace(&mut self.in_pat, false);
        self.children(file, node);
        self.in_pat = in_pat;
    }

    /// rustc's `visit_path` (and `visit_expr` of a type-relative path): what `path` resolves
    /// to is live. Gives what it resolved to, for the pattern that has it.
    fn path(&mut self, path: ast::Path<'db>) -> Res {
        let cx = self.k.cx;
        let parent = path.syntax().parent().map(|it| it.kind());
        if matches!(
            parent,
            Some(SyntaxKind::MACRO_CALL | SyntaxKind::PATH_META | SyntaxKind::KEY_VALUE_META | SyntaxKind::TOKEN_TREE_META)
        ) {
            return Res::None;
        }
        let Some(res) = cx.vsema.resolve_path(path) else { return Res::None };
        let value_or_pat = matches!(
            parent,
            Some(SyntaxKind::PATH_EXPR | SyntaxKind::PATH_PAT | SyntaxKind::TUPLE_STRUCT_PAT)
        );
        let self_ctor = value_or_pat && path.qualifier().is_none() && is_self(path);
        let res = Res::path(cx, self.k.defs, res, self_ctor);
        self.handle_res(res);
        res
    }

    /// rustc's `handle_field_access`: a field of a struct or a union read.
    fn field_read(&mut self, field: ast::FieldExpr<'db>) {
        if let Some(Either::Left(field)) = self.k.cx.vsema.resolve_field(field)
            && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
        {
            self.insert_def_id(field);
        }
    }

    /// rustc's `mark_as_used_if_union`: the fields a union expression names, of a union of
    /// more than one field, are read. A literal naming no field is no union's, and is not typed.
    fn union_fields(&mut self, expr: ast::RecordExpr<'db>) {
        let cx = self.k.cx;
        let Some(list) = expr.record_expr_field_list() else { return };
        if list.fields().next().is_none() {
            return;
        }
        let Some(ty) = cx.vsema.type_of_expr(ast::Expr::RecordExpr(expr)) else { return };
        let Some(hir::Adt::Union(union)) = ty.original.as_adt() else { return };
        if union.fields(cx.db).len() <= 1 {
            return;
        }
        for field in list.fields() {
            if let Some((field, _, _)) = cx.vsema.resolve_record_field(field)
                && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
            {
                self.insert_def_id(field);
            }
        }
    }

    /// rustc's `visit_pat`: what a pattern's paths name, its ADT, and the fields it binds.
    fn pat(&mut self, file: HirFileId, node: ast::Node<'db>) {
        let cx = self.k.cx;
        self.in_pat = true;
        let Some(pat) = ast::Pat::cast(node) else {
            self.children(file, node);
            self.in_pat = false;
            return;
        };
        match pat {
            ast::Pat::RecordPat(it) => {
                self.pat_adt(pat);
                if let Some(path) = it.path() {
                    self.path(path);
                }
                if let Some(list) = it.record_pat_field_list() {
                    for field in list.fields() {
                        if matches!(field.pat(), Some(ast::Pat::WildcardPat(_))) {
                            continue;
                        }
                        if let Some((field, _)) = cx.vsema.resolve_record_pat_field(field)
                            && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
                        {
                            self.insert_def_id(field);
                        }
                    }
                    self.children(file, list.syntax());
                }
            }
            ast::Pat::TupleStructPat(it) => {
                let adt = self.pat_adt(pat);
                let res = it.path().map_or(Res::None, |path| self.path(path));
                if let Some(variant) = self.variant_of(res, adt) {
                    let rest = it.fields().position(|it| matches!(it, ast::Pat::RestPat(_)));
                    let bound = it.fields().count() - usize::from(rest.is_some());
                    let wildcard = it
                        .fields()
                        .filter(|it| !matches!(it, ast::Pat::RestPat(_)))
                        .map(|it| matches!(it, ast::Pat::WildcardPat(_)));
                    self.tuple_fields(variant, wildcard, bound, rest);
                }
                for sub in it.fields() {
                    stdx::ensure_sufficient_stack(|| self.walk(file, sub.syntax(), false));
                    self.in_pat = true;
                }
            }
            ast::Pat::PathPat(_) => {
                self.pat_adt(pat);
                self.children(file, node);
            }
            ast::Pat::IdentPat(it) => {
                if let Some(def) = cx.vsema.resolve_bind_pat_to_const(it) {
                    self.pat_adt(pat);
                    self.handle_res(Res::def(self.k.defs, def));
                }
                self.children(file, node);
            }
            _ => self.children(file, node),
        }
        self.in_pat = false;
    }

    /// The variant a tuple struct pattern (or the call a destructuring assignment lowers to one)
    /// binds the fields of: rustc's `adt.variant_of_res(res)` of the pattern's type, the variant
    /// its path names, else the struct (also for `Self`).
    fn variant_of(&self, res: Res, adt: Option<DefIdx>) -> Option<DefIdx> {
        match res {
            Res::Variant(it) => Some(it),
            Res::Def(_) | Res::SelfCtor => adt.filter(|&it| self.k.defs[it].kind == Kind::Struct),
            Res::None => None,
        }
    }

    /// The fields of `variant` that the subpatterns of a tuple pattern bind: whether each of its
    /// `bound` subpatterns other than `..` is a `_`, in order, and how many come before its `..`.
    fn tuple_fields(&mut self, variant: DefIdx, wildcard: impl Iterator<Item = bool>, bound: usize, dotdot: Option<usize>) {
        let defs = self.k.defs;
        let fields = defs.children(variant);
        let dotdot = dotdot.unwrap_or(bound);
        let missing = fields.len().saturating_sub(bound);
        for (index, is_wildcard) in wildcard.enumerate() {
            if is_wildcard {
                continue;
            }
            let index = if index < dotdot { index } else { index + missing };
            if let Some(&field) = fields.get(index) {
                self.insert_def_id(field);
            }
        }
    }

    /// The ADT a pattern is of is live (rustc's `check_def_id(adt.did())` of a struct, tuple
    /// struct or path pattern).
    fn pat_adt(&mut self, pat: ast::Pat<'db>) -> Option<DefIdx> {
        let ty = self.k.cx.vsema.type_of_pat(pat)?;
        let def = self.k.defs.get(Id::Adt(AdtId::from(ty.original.as_adt()?)))?;
        self.check_def_id(def);
        Some(def)
    }

    /// rustc's `necessary_variants` of a match arm's pattern: the variants its paths name
    /// (resolved, not through a type), outside `|` patterns, added to `out` past `from` once
    /// each.
    fn necessary_variants(&self, node: ast::Node<'db>, out: &mut Vec<DefIdx>, from: usize) {
        let cx = self.k.cx;
        let path = match node.kind() {
            SyntaxKind::OR_PAT => return,
            SyntaxKind::PATH_PAT => ast::PathPat::cast(node).and_then(|it| it.path()),
            SyntaxKind::TUPLE_STRUCT_PAT => ast::TupleStructPat::cast(node).and_then(|it| it.path()),
            SyntaxKind::RECORD_PAT => ast::RecordPat::cast(node).and_then(|it| it.path()),
            SyntaxKind::IDENT_PAT => {
                if let Some(pat) = ast::IdentPat::cast(node)
                    && let Some(ModuleDef::EnumVariant(it)) = cx.vsema.resolve_bind_pat_to_const(pat)
                    && let Some(variant) = self.k.defs.get(Id::Variant(it.into()))
                    && !out[from..].contains(&variant)
                {
                    out.push(variant);
                }
                None
            }
            _ => None,
        };
        if let Some(path) = path
            && self.is_resolved(path)
            && let Some(res) = cx.vsema.resolve_path(path)
            && let Res::Variant(variant) = Res::path(cx, self.k.defs, res, false)
            && !out[from..].contains(&variant)
        {
            out.push(variant);
        }
        for child in node.children() {
            let kind = child.kind();
            if ast::Pat::can_cast(kind) || kind == SyntaxKind::RECORD_PAT_FIELD_LIST {
                self.necessary_variants(child, out, from);
            } else if kind == SyntaxKind::RECORD_PAT_FIELD {
                for it in child.children() {
                    self.necessary_variants(it, out, from);
                }
            }
        }
    }

    /// Whether rustc's resolver resolves `path` (`QPath::Resolved`), rather than typeck through
    /// a type (`QPath::TypeRelative`: `Self::A`, `Alias::A`, `<E>::A`).
    fn is_resolved(&self, path: ast::Path<'db>) -> bool {
        let Some(qualifier) = path.qualifier() else { return true };
        if is_self(qualifier) || qualifier.segments().any(|it| it.type_anchor().is_some()) {
            return false;
        }
        matches!(
            self.k.cx.vsema.resolve_path(qualifier),
            Some(PathResolution::Def(ModuleDef::Module(_) | ModuleDef::Adt(hir::Adt::Enum(_))))
        )
    }

    /// `lhs = rhs`: rustc's `handle_assign` of the place written (its fields not read), or the
    /// pattern a destructuring assignment is lowered to; then `rhs`, and the self-assignment
    /// lint.
    fn assign(&mut self, file: HirFileId, expr: ast::BinExpr<'db>) {
        let Some(lhs) = expr.lhs() else {
            if let Some(rhs) = expr.rhs() {
                self.walk(file, rhs.syntax(), false);
            }
            return;
        };
        let destructuring = destructures(lhs);
        match destructuring {
            true => self.assign_pattern(file, lhs),
            false => self.handle_assign(file, lhs),
        }
        if let Some(rhs) = expr.rhs() {
            if !destructuring {
                self.self_assign(file, expr, lhs, rhs);
            }
            stdx::ensure_sufficient_stack(|| self.walk(file, rhs.syntax(), false));
        }
    }

    fn handle_assign(&mut self, file: HirFileId, place: ast::Expr<'db>) {
        let through_field = matches!(place, ast::Expr::FieldExpr(_) | ast::Expr::ParenExpr(_));
        let derefs = through_field
            && self
                .k
                .cx
                .vsema
                .expr_adjustments(place)
                .is_some_and(|it| it.iter().any(|it| matches!(it.kind, hir_ty::Adjust::Deref(_))));
        match place {
            ast::Expr::FieldExpr(field) if !derefs => {
                if let Some(base) = field.expr() {
                    self.handle_assign(file, base);
                }
            }
            ast::Expr::ParenExpr(it) if !derefs => {
                if let Some(inner) = it.expr() {
                    self.handle_assign(file, inner);
                }
            }
            _ => stdx::ensure_sufficient_stack(|| self.walk(file, place.syntax(), false)),
        }
    }

    /// The left of a destructuring assignment as the pattern rustc lowers it to, each place in
    /// it assigned.
    fn assign_pattern(&mut self, file: HirFileId, lhs: ast::Expr<'db>) {
        let cx = self.k.cx;
        match lhs {
            ast::Expr::ParenExpr(it) => {
                if let Some(inner) = it.expr() {
                    self.assign_pattern(file, inner);
                }
            }
            ast::Expr::TupleExpr(it) => {
                for field in it.fields() {
                    self.assign_pattern(file, field);
                }
            }
            ast::Expr::ArrayExpr(it) => {
                for field in it.exprs() {
                    self.assign_pattern(file, field);
                }
            }
            ast::Expr::UnderscoreExpr(_) => {}
            ast::Expr::RangeExpr(it) if it.start().is_none() && it.end().is_none() => {}
            ast::Expr::RecordExpr(it) => {
                self.expr_adt(lhs);
                if let Some(path) = it.path() {
                    self.in_pat = true;
                    self.path(path);
                    self.in_pat = false;
                }
                for field in it.record_expr_field_list().into_iter().flat_map(|it| it.fields()) {
                    let value = field.expr();
                    if matches!(value, Some(ast::Expr::UnderscoreExpr(_))) {
                        continue;
                    }
                    if let Some((def, _, _)) = cx.vsema.resolve_record_field(field)
                        && let Some(def) = self.k.defs.get(Id::Field(FieldId::from(def)))
                    {
                        self.insert_def_id(def);
                    }
                    if let Some(value) = value {
                        self.assign_pattern(file, value);
                    }
                }
            }
            ast::Expr::CallExpr(it) => {
                let path = match it.expr() {
                    Some(ast::Expr::PathExpr(it)) => it.path(),
                    _ => None,
                };
                let Some(path) = path else {
                    self.handle_assign(file, lhs);
                    return;
                };
                let adt = self.expr_adt(lhs);
                self.in_pat = true;
                let res = self.path(path);
                self.in_pat = false;
                let args = || it.arg_list().into_iter().flat_map(|it| it.args());
                if let Some(variant) = self.variant_of(res, adt) {
                    let rest = args().position(is_rest);
                    let bound = args().count() - usize::from(rest.is_some());
                    let wildcard = args().filter(|&it| !is_rest(it)).map(|it| matches!(it, ast::Expr::UnderscoreExpr(_)));
                    self.tuple_fields(variant, wildcard, bound, rest);
                }
                for arg in args() {
                    self.assign_pattern(file, arg);
                }
            }
            ast::Expr::PathExpr(it) => {
                let path = it.path();
                let constructs = path.and_then(|path| cx.vsema.resolve_path(path)).is_some_and(|it| {
                    matches!(it, PathResolution::Def(ModuleDef::Adt(_) | ModuleDef::EnumVariant(_)))
                });
                match (constructs, path) {
                    (true, Some(path)) => {
                        self.expr_adt(lhs);
                        self.in_pat = true;
                        self.path(path);
                        self.in_pat = false;
                    }
                    _ => self.handle_assign(file, lhs),
                }
            }
            _ => self.handle_assign(file, lhs),
        }
    }

    fn expr_adt(&mut self, expr: ast::Expr<'db>) -> Option<DefIdx> {
        let ty = self.k.cx.vsema.type_of_expr(expr)?;
        let def = self.k.defs.get(Id::Adt(AdtId::from(ty.original.as_adt()?)))?;
        self.check_def_id(def);
        Some(def)
    }

    /// rustc's `check_for_self_assign`: `x = x` or `a.b = a.b`, not from a macro's expansion.
    /// The two sides' shapes and names are compared before anything is resolved.
    fn self_assign(&mut self, file: HirFileId, expr: ast::BinExpr<'db>, lhs: ast::Expr<'db>, rhs: ast::Expr<'db>) {
        let cx = self.k.cx;
        if !same_place(cx, lhs, rhs) {
            return;
        }
        if crate::lints::syntax::from_expansion(cx, InFile::new(file, expr.syntax())) {
            return;
        }
        let at = InFile::new(file, ast::ptr(expr.syntax()));
        if !self.self_assigns.insert(at) {
            return;
        }
        let Some(ty) = cx.vsema.type_of_expr(lhs) else { return };
        let what = match unparen(lhs) {
            ast::Expr::FieldExpr(_) => "field",
            _ => "variable",
        };
        // alloc: the edge, a lint that fires
        let message = format!("useless assignment of {what} of type `{}` to itself", cx.ty(&ty.original));
        let mut diag = Diag::lint("dead_code", message, cx.span_of(at));
        crate::lints::code_fact(cx, &mut diag, at);
        self.found.push(Found::lint(diag, Phase::Late, at));
    }
}

/// Whether the left of an assignment is a pattern rustc lowers a destructuring assignment to.
fn destructures(lhs: ast::Expr<'_>) -> bool {
    match lhs {
        ast::Expr::ParenExpr(it) => it.expr().is_some_and(destructures),
        ast::Expr::TupleExpr(_)
        | ast::Expr::ArrayExpr(_)
        | ast::Expr::RecordExpr(_)
        | ast::Expr::CallExpr(_)
        | ast::Expr::UnderscoreExpr(_) => true,
        _ => false,
    }
}

/// `..` as an argument of a destructuring assignment.
fn is_rest(expr: ast::Expr<'_>) -> bool {
    matches!(expr, ast::Expr::RangeExpr(it) if it.start().is_none() && it.end().is_none())
}

fn unparen(expr: ast::Expr<'_>) -> ast::Expr<'_> {
    match expr {
        ast::Expr::ParenExpr(it) => it.expr().map_or(expr, unparen),
        _ => expr,
    }
}

/// `check_for_self_assign_helper`: the same local (a one-segment path on each side with the same
/// name, resolved only then), or the same field of the same place.
fn same_place<'db>(cx: &crate::cx::Cx<'_, 'db>, lhs: ast::Expr<'db>, rhs: ast::Expr<'db>) -> bool {
    match (unparen(lhs), unparen(rhs)) {
        (ast::Expr::PathExpr(l), ast::Expr::PathExpr(r)) => {
            let (Some(l), Some(r)) = (l.path(), r.path()) else { return false };
            let same_name = match (l.as_single_name_ref(), r.as_single_name_ref()) {
                (Some(a), Some(b)) => unraw(a.text()) == unraw(b.text()),
                _ => false,
            };
            let local = |it: ast::Path<'db>| match cx.vsema.resolve_path(it)? {
                PathResolution::Local(it) => Some(it),
                _ => None,
            };
            same_name && matches!((local(l), local(r)), (Some(l), Some(r)) if l == r)
        }
        (ast::Expr::FieldExpr(l), ast::Expr::FieldExpr(r)) => {
            let same_name = match (l.name_ref(), r.name_ref()) {
                (Some(a), Some(b)) => unraw(a.text()) == unraw(b.text()),
                _ => false,
            };
            match (l.expr(), r.expr()) {
                (Some(l), Some(r)) if same_name => same_place(cx, l, r),
                _ => false,
            }
        }
        _ => false,
    }
}

/// A name's text as its symbol is: `r#` stripped.
fn unraw(text: &str) -> &str {
    text.strip_prefix("r#").unwrap_or(text)
}

/// Whether `path` is the one segment `Self`.
fn is_self(path: ast::Path<'_>) -> bool {
    path.segment().is_some_and(|it| it.self_type_token().is_some())
}
