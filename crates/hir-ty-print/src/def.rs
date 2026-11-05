//! What rustc names a definition by: the definitions a path is made of and their parents
//! (rustc's `def_key`). An item written in a body is a child of the body's owner, under the
//! closures it is written in, each numbered as rustc's `{closure#N}` is. Ported from
//! `compiler/rustc_middle/src/ty/print/pretty.rs` and `compiler/rustc_hir/src/definitions.rs`
//! (MIT or Apache-2.0).

use base_db::Db;
use hir_def::{
    AdtId, AssocItemId, AttrDefId, BlockIdLt, ConstId, DefWithBodyId, EnumVariantId, ExternCrateId,
    FunctionId, HasModule, ImplId, ItemContainerId, Lookup, MacroId, ModuleDefId, ModuleId,
    ModuleIdLt, StaticId, TraitId, TypeAliasId,
    attrs::AttrFlags,
    expr_store::{Body, BodySourceMap, StoreVisitor},
    hir::{ClosureKind, Expr, ExprId, PatId},
    signatures::{
        ConstSignature, EnumSignature, FunctionSignature, StaticSignature, StructSignature,
        TraitSignature, TypeAliasSignature, UnionSignature,
    },
};
use hir_expand::name::Name;
use intern::sym;

/// A definition a path names: rustc's `DefId`, of the kinds a path is made of.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Def {
    Module(ModuleId),
    Adt(AdtId),
    Variant(EnumVariantId),
    Function(FunctionId),
    Const(ConstId),
    Static(StaticId),
    Trait(TraitId),
    TypeAlias(TypeAliasId),
    Macro(MacroId),
    Impl(ImplId),
    ExternCrate(ExternCrateId),
}

/// rustc's `Namespace` of a definition.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Ns {
    Type,
    Value,
    Macro,
}

impl Def {
    /// None for a primitive type, which has no definition.
    pub fn of(def: ModuleDefId) -> Option<Def> {
        Some(match def {
            ModuleDefId::ModuleId(it) => Def::Module(it),
            ModuleDefId::FunctionId(it) => Def::Function(it),
            ModuleDefId::AdtId(it) => Def::Adt(it),
            ModuleDefId::EnumVariantId(it) => Def::Variant(it),
            ModuleDefId::ConstId(it) => Def::Const(it),
            ModuleDefId::StaticId(it) => Def::Static(it),
            ModuleDefId::TraitId(it) => Def::Trait(it),
            ModuleDefId::TypeAliasId(it) => Def::TypeAlias(it),
            ModuleDefId::MacroId(it) => Def::Macro(it),
            ModuleDefId::BuiltinType(_) => return None,
        })
    }

    pub fn of_assoc(item: AssocItemId) -> Def {
        match item {
            AssocItemId::FunctionId(it) => Def::Function(it),
            AssocItemId::ConstId(it) => Def::Const(it),
            AssocItemId::TypeAliasId(it) => Def::TypeAlias(it),
        }
    }

    pub fn attr_owner(self) -> AttrDefId {
        match self {
            Def::Module(it) => AttrDefId::ModuleId(it),
            Def::Adt(it) => AttrDefId::AdtId(it),
            Def::Variant(it) => AttrDefId::EnumVariantId(it),
            Def::Function(it) => AttrDefId::FunctionId(it),
            Def::Const(it) => AttrDefId::ConstId(it),
            Def::Static(it) => AttrDefId::StaticId(it),
            Def::Trait(it) => AttrDefId::TraitId(it),
            Def::TypeAlias(it) => AttrDefId::TypeAliasId(it),
            Def::Macro(it) => AttrDefId::MacroId(it),
            Def::Impl(it) => AttrDefId::ImplId(it),
            Def::ExternCrate(it) => AttrDefId::ExternCrateId(it),
        }
    }

    /// The module the definition is written in (itself, for a module).
    pub fn module(self, db: &Db) -> ModuleId {
        self.attr_owner().module(db)
    }

    pub fn krate(self, db: &Db) -> base_db::Crate {
        self.module(db).krate(db)
    }

    pub fn is_doc_hidden(self, db: &Db) -> bool {
        AttrFlags::query(db, self.attr_owner()).contains(AttrFlags::IS_DOC_HIDDEN)
    }

    /// rustc's `DefKind::ns`: None for an impl.
    pub fn ns(self) -> Option<Ns> {
        match self {
            Def::Module(_)
            | Def::Adt(_)
            | Def::Variant(_)
            | Def::Trait(_)
            | Def::TypeAlias(_)
            | Def::ExternCrate(_) => Some(Ns::Type),
            Def::Function(_) | Def::Const(_) | Def::Static(_) => Some(Ns::Value),
            Def::Macro(_) => Some(Ns::Macro),
            Def::Impl(_) => None,
        }
    }

    /// rustc's `DefKind::is_module_like`, with the enums and traits whose children
    /// `module_children` lists.
    pub fn is_module_like(self) -> bool {
        matches!(self, Def::Module(_) | Def::Adt(AdtId::EnumId(_)) | Def::Trait(_))
    }

    /// Whether it is in value namespace: what `def_path_str` prints with `::<..>`.
    pub fn is_value(self) -> bool {
        self.ns() == Some(Ns::Value)
    }

    /// Whether its path segment is rustc's `DefPathData::TypeNs`, the segments a re-export
    /// can rename.
    pub fn is_type_ns(self) -> bool {
        self.ns() == Some(Ns::Type)
    }

    /// The name of the definition's path segment; None for a crate root, a block module, an
    /// impl, an unnamed constant and an `extern crate .. as _`.
    pub fn name(self, db: &Db) -> Option<Name> {
        match self {
            Def::Module(it) => it.name(db),
            Def::Impl(_) => None,
            Def::ExternCrate(it) => it.alias_or_name(db),
            Def::Variant(it) => Some(it.lookup(db).name),
            Def::Macro(it) => Some(it.name(db)),
            Def::Adt(AdtId::StructId(it)) => Some(StructSignature::of(db, it).name),
            Def::Adt(AdtId::UnionId(it)) => Some(UnionSignature::of(db, it).name),
            Def::Adt(AdtId::EnumId(it)) => Some(EnumSignature::of(db, it).name),
            Def::Function(it) => Some(FunctionSignature::of(db, it).name),
            Def::Const(it) => ConstSignature::of(db, it).name,
            Def::Static(it) => Some(StaticSignature::of(db, it).name),
            Def::Trait(it) => Some(TraitSignature::of(db, it).name),
            Def::TypeAlias(it) => Some(TypeAliasSignature::of(db, it).name),
        }
    }

    /// The segment rustc's `def_path_str` prints for the definition: its name, or `_` for an
    /// unnamed one (`const _`, `extern crate krate as _`; rustc's `DefPathData::ValueNs(_)` and
    /// `TypeNs(_)`). None only for a crate root, an impl and a block module, which have no
    /// segment of their own: an item in a block is printed under the body's owner.
    pub fn segment(self, db: &Db) -> Option<Name> {
        match self {
            Def::Const(_) | Def::ExternCrate(_) => {
                Some(self.name(db).unwrap_or_else(|| Name::new_symbol_root(sym::underscore)))
            }
            _ => self.name(db),
        }
    }

    /// The container of an associated or foreign item.
    pub fn container(self, db: &Db) -> Option<ItemContainerId> {
        Some(match self {
            Def::Function(it) => it.lookup(db).container,
            Def::Const(it) => it.lookup(db).container,
            Def::Static(it) => it.lookup(db).container,
            Def::TypeAlias(it) => it.lookup(db).container,
            _ => return None,
        })
    }

    /// Whether it is written in an `extern` block: a foreign item, not one of rustc's free
    /// items.
    pub fn is_foreign(self, db: &Db) -> bool {
        matches!(self.container(db), Some(ItemContainerId::ExternBlockId(_)))
    }

    /// The definition of a body's owner.
    pub fn of_body(owner: DefWithBodyId) -> Option<Def> {
        Some(match owner {
            DefWithBodyId::FunctionId(it) => Def::Function(it),
            DefWithBodyId::ConstId(it) => Def::Const(it),
            DefWithBodyId::StaticId(it) => Def::Static(it),
            DefWithBodyId::VariantId(_) => return None,
        })
    }
}

/// What a definition's path is printed under: rustc's `def_key(def).parent`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Parent<'db> {
    Def(Def),
    /// An item or closure written in a body: the body's owner, then the closures it is written
    /// in, outermost first, each by its place among the closures of the one before it and its
    /// kind. `in_anon_const` when it is in an anonymous constant of the body (an array length,
    /// a const argument, `const { .. }`): rustc's `{constant#N}`, which is not numbered here.
    Body {
        owner: Def,
        closures: &'db [(usize, ClosureKind)],
        in_anon_const: bool,
    },
}

/// The definition's parent; None for a crate root, and for an item in a block whose body
/// has no owner a path can go through (an enum's discriminant, a block of a signature).
pub fn parent(db: &Db, def: Def) -> Option<Parent<'_>> {
    match def {
        Def::Module(it) => module_parent(db, it.containing_module(db)?.to_static()),
        Def::Variant(it) => Some(Parent::Def(Def::Adt(it.lookup(db).parent.into()))),
        Def::Function(_) | Def::Const(_) | Def::Static(_) | Def::TypeAlias(_) => {
            match def.container(db)? {
                ItemContainerId::ModuleId(it) => module_parent(db, it),
                ItemContainerId::ExternBlockId(it) => module_parent(db, it.lookup(db).container),
                ItemContainerId::ImplId(it) => Some(Parent::Def(Def::Impl(it))),
                ItemContainerId::TraitId(it) => Some(Parent::Def(Def::Trait(it))),
            }
        }
        Def::Adt(_) | Def::Trait(_) | Def::Macro(_) | Def::Impl(_) | Def::ExternCrate(_) => {
            module_parent(db, def.module(db))
        }
    }
}

/// What an item of `module` is a child of: the module, or for a block's module the body the
/// block is in.
pub fn module_parent(db: &Db, module: ModuleId) -> Option<Parent<'_>> {
    let module = module.to_db(db);
    if !module.is_block_module(db) {
        return Some(Parent::Def(Def::Module(module.to_static())));
    }
    let block = module.block(db)?;
    let (owner, _) = block_owner(db, block)?;
    let def = Def::of_body(owner)?;
    let closures =
        block_expr(Body::of(db, owner), block).and_then(|at| closures_around(db, owner, at));
    Some(Parent::Body {
        owner: def,
        closures: closures.unwrap_or(&[]),
        in_anon_const: closures.is_none(),
    })
}

/// The body a closure, `async` block or coroutine written at `expr` of `owner` is in, with
/// the closures around it and itself last. None for one of an enum's discriminant.
pub fn closure_parent(db: &Db, owner: DefWithBodyId, expr: ExprId) -> Option<Parent<'_>> {
    let def = Def::of_body(owner)?;
    let closures = closures_around(db, owner, expr);
    Some(Parent::Body {
        owner: def,
        closures: closures.unwrap_or(&[]),
        in_anon_const: closures.is_none(),
    })
}

/// The body a block is written in, and the module that body's owner is declared in (the
/// module of the `impl` or trait for an associated item).
pub fn block_owner<'db>(
    db: &'db Db,
    block: BlockIdLt<'db>,
) -> Option<(DefWithBodyId, ModuleIdLt<'db>)> {
    let owner = match block.owner(db) {
        Some(it) => it,
        None => hir_def::nameres::demand::block_owner(db, block.to_static()).ok()?,
    };
    Some((owner, owner.module(db).to_db(db)))
}

/// The expression that is `block` in `body`.
fn block_expr(body: &Body, block: BlockIdLt<'_>) -> Option<ExprId> {
    let (at, _) = body
        .exprs()
        .find(|(_, expr)| matches!(expr, Expr::Block { id: Some(id), .. } if *id == block))?;
    Some(at)
}

/// The closures and `async` blocks `target` is written in, outermost first and with `target`
/// last when it is one: each by its place among those written in the one before it. None when
/// `target` is in an anonymous constant of the body, or not in it.
pub fn closures_around(
    db: &Db,
    owner: DefWithBodyId,
    target: ExprId,
) -> Option<&[(usize, ClosureKind)]> {
    closures_around_query(db, owner, target).as_deref()
}

/// [`closures_around`], kept: it may read the source map, which changes with every edit above
/// the body, and what it finds does not. The map orders closures by the macro calls they are
/// in, so a body that called no macro is ordered without it, as every closure of it is in
/// the same file.
#[blaze_query::tracked(returns(ref))]
fn closures_around_query(
    db: &Db,
    owner: DefWithBodyId,
    target: ExprId,
) -> Option<Vec<(usize, ClosureKind)>> {
    let body = Body::of(db, owner);
    // alloc: once per closure, in the query that keeps its result
    let mut around = Vec::new();
    if !find_closures(body, body.root_expr(), target, &mut around) {
        return None;
    }
    let expanded = !body.macro_calls().is_empty();
    let order =
        |closure: ExprId| MacroCalls::around(db, Body::unmoved_source_map(db, owner), closure);
    let mut inside = None;
    let places = around.iter().map(|&closure| {
        // alloc: once per closure, in the query that keeps its result
        let mut beside = Vec::new();
        match inside.replace(closure) {
            Some(it) => {
                walk_outside_consts(body, it, |child| closures_in(body, child, &mut beside))
            }
            None => closures_in(body, body.root_expr(), &mut beside),
        }
        // Stable, so those of one expansion stay in the order they are written in.
        if expanded && beside.len() > 1 {
            beside.sort_by_cached_key(|&it| order(it));
        }
        let Expr::Closure { closure_kind, .. } = body.expr(closure) else { return None };
        Some((beside.iter().position(|&it| it == closure)?, closure_kind))
    });
    // alloc: the result the query keeps
    places.collect()
}

/// The closures `target` is written in, looking for it from `at` down.
fn find_closures(body: &Body, at: ExprId, target: ExprId, around: &mut Vec<ExprId>) -> bool {
    let is_closure = matches!(body.expr(at), Expr::Closure { .. });
    if is_closure {
        around.push(at);
    }
    let mut found = at == target;
    if !found {
        walk_outside_consts(body, at, |child| {
            found = found || find_closures(body, child, target, around);
        });
    }
    if is_closure && !found {
        around.pop();
    }
    found
}

/// The closures from `at` down that are written in no other closure on the way.
fn closures_in(body: &Body, at: ExprId, found: &mut Vec<ExprId>) {
    match body.expr(at) {
        Expr::Closure { .. } => found.push(at),
        _ => walk_outside_consts(body, at, |child| closures_in(body, child, found)),
    }
}

/// The expressions right in `at`, in the patterns too, but not those of an anonymous constant
/// (`const { .. }`, the length of `[x; n]`, a const argument): rustc makes each of those a
/// definition of its own and numbers the closures in it under that, not under the body. So a
/// closure in one is not found (its place is refused) and does not count among the others.
fn walk_outside_consts(body: &Body, at: ExprId, callback: impl FnMut(ExprId)) {
    return body.visit_expr_children(at, Visitor { callback, body });

    struct Visitor<'a, F> {
        callback: F,
        body: &'a Body,
    }

    impl<F: FnMut(ExprId)> StoreVisitor for Visitor<'_, F> {
        fn on_expr(&mut self, expr: ExprId) {
            (self.callback)(expr);
        }

        fn on_anon_const_expr(&mut self, _: ExprId) {}

        fn on_pat(&mut self, pat: PatId) {
            let body = self.body;
            body.visit_pat_children(pat, self);
        }
    }
}

/// The macro calls something is in the expansion of, outermost first: each by where in the
/// text it is expanded from the call starts.
///
/// rustc numbers what it finds in a body before it expands the macros of it, and then what
/// each expansion holds as it gets to it, the macros of an expansion first. That is the
/// order these sort in.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct MacroCalls(Vec<u32>);

impl MacroCalls {
    fn around(db: &Db, source_map: &BodySourceMap, expr: ExprId) -> Self {
        // alloc: a sort key, only in a body that called a macro, once per query run
        let mut calls = Vec::new();
        let mut file = source_map.expr_syntax(expr).ok().map(|it| it.file_id);
        while let Some(call) = file.and_then(|it| it.macro_file()) {
            let node = call.call_node_view(db);
            calls.push(node.value.text_range().start().into());
            file = Some(node.file_id);
        }
        calls.reverse();
        MacroCalls(calls)
    }
}
