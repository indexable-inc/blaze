//! The crate's definitions as the dead-code pass sees them: rustc's owners (items, associated
//! items, foreign items, the impls a builtin derive makes) with their variants and fields, the
//! free and foreign items of each module in the order `check_mod_deathness` visits them, each
//! definition's parent (`local_parent`), and the owners in `hir_crate_items` order.
//!
//! The crate is numbered from what name resolution already made, not from its syntax: each
//! module's items in the order its item tree has them ([`ItemTree::top_level_items`], a macro
//! call's or an attribute macro's expansion's tree in place of the call), each mapped to its id
//! by its scope ([`ItemScope::items_by_ast`]); an ADT's variants and fields, its derives', a
//! trait's or an impl's items as their own queries list them; and the items written in a body
//! as [`item_tree::body_items`] lists them, each after the definition whose body holds it. A
//! dense `u32` per definition; the rest of the pass indexes columns and bitsets by it. A
//! definition's children are one CSR list, its name the interned `Name` its signature has, and
//! what each impl is of is resolved once here. Where a definition is written is asked only
//! where a finding or a level is made ([`Defs::node`]).
//!
//! Follows `compiler/rustc_middle/src/hir/map.rs` (`ItemCollector`),
//! `compiler/rustc_middle/src/hir/mod.rs` (`hir_crate_items`, `hir_module_items`) and
//! `compiler/rustc_builtin_macros/src/deriving/` (where a derive puts its impl)
//! (MIT or Apache-2.0).

#[cfg(test)]
pub(super) mod reference;

use blaze_hash::{HashMap, HashSet};
use blaze_ids::{bits::BitSet, radix};
use either::Either;
use hir::{HirFileId, InFile, Name};
use hir_def::{
    AdtId, AssocItemId, BlockId, BuiltinDeriveImplId, ConstId, DefWithBodyId, EnumVariantId,
    ExpressionStoreOwnerId, FieldId, FunctionId, GenericDefId, HasModule, ImplId, Lookup as _,
    MacroId, ModuleDefId, ModuleId, StaticId, TraitId, TypeAliasId, UseId, VariantId,
    builtin_derive::BuiltinDeriveImplTrait,
    expr_store::{Body, ExpressionStore, StoreVisitor},
    hir::{ClosureKind, CoroutineSource, Expr, ExprId, PatId, type_ref::TypeRefId},
    item_scope::{ItemOfAst, ItemScope, ItemsByAst},
    item_tree::{self, ItemKind, ItemTree, ModItemId},
    nameres::{ModuleOrigin, block_def_map},
    signatures::{
        ConstSignature, FunctionSignature, ImplSignature, StaticFlags, StaticSignature, TraitFlags,
        TraitSignature, VariantFields,
    },
    src::HasChildSourceView as _,
};
use hir_expand::{ErasedAstId, MacroCallId, builtin::BuiltinDeriveExpander};
use intern::sym;
use smallvec::SmallVec;
use std::alloc::Global;
use syntax::{
    SyntaxKind, SyntaxNodePtr,
    view::{self as ast, AstNode as _},
};

use crate::cx::Cx;

/// A definition's dense number, its index in [`Defs`].
pub(super) type DefIdx = u32;

/// A set of definitions: one bit each, `len / 8` bytes, O(1) test and insert.
pub(super) type DefSet = BitSet<DefIdx>;

/// What a definition is, by the front end's ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Id {
    Fn(FunctionId),
    Const(ConstId),
    Static(StaticId),
    Adt(AdtId),
    Variant(EnumVariantId),
    Field(FieldId),
    Trait(TraitId),
    Alias(TypeAliasId),
    Impl(ImplId),
    /// The impl a builtin derive makes.
    Derive(BuiltinDeriveImplId),
    /// The method of that impl, for the traits that have one.
    DeriveMethod(BuiltinDeriveImplId),
    Use(UseId),
}

/// rustc's `DefKind`, as far as the pass tells them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Fn,
    AssocFn { method: bool },
    Const,
    AssocConst,
    Static,
    Struct,
    Union,
    Enum,
    Variant,
    Field,
    Trait,
    TraitAlias,
    TyAlias,
    AssocTy,
    ForeignTy,
    Impl { of_trait: bool },
    Use,
}

const _: () = assert!(size_of::<Kind>() <= 2);

impl Kind {
    /// rustc's `def_descr`.
    pub(super) fn descr(self) -> &'static str {
        match self {
            Kind::Fn => "function",
            Kind::AssocFn { method: true } => "method",
            Kind::AssocFn { method: false } => "associated function",
            Kind::Const => "constant",
            Kind::AssocConst => "associated constant",
            Kind::Static => "static",
            Kind::Struct => "struct",
            Kind::Union => "union",
            Kind::Enum => "enum",
            Kind::Variant => "variant",
            Kind::Field => "field",
            Kind::Trait => "trait",
            Kind::TraitAlias => "trait alias",
            Kind::TyAlias => "type alias",
            Kind::AssocTy => "associated type",
            Kind::ForeignTy => "foreign type",
            Kind::Impl { .. } => "implementation",
            Kind::Use => "import",
        }
    }

    /// The same kind of associated item: functions, methods or not, are one.
    fn same_assoc(self, other: Kind) -> bool {
        match (self, other) {
            (Kind::AssocFn { .. }, Kind::AssocFn { .. }) => true,
            _ => self == other,
        }
    }
}

/// A closure, a coroutine, a `const` block or an anonymous constant (rustc's definitions of
/// their own that items in them have as their parent), by the expression store it is in (a
/// body's, a signature's, a variant's fields') and its expression: the key both the numbering
/// and the walk of the store name it by, with no position in it.
pub(super) type Anon = (ExpressionStoreOwnerId, ExprId);

/// rustc's `local_parent` of a definition, as far as liveness asks: the module, a definition,
/// or a closure or anonymous constant, which is live once it is visited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Parent {
    Module,
    Def(DefIdx),
    Anon(Anon),
}

/// One definition: no heap data of its own (its children are a range of [`Defs`]'s list).
#[derive(Debug)]
pub(super) struct Def {
    pub(super) id: Id,
    pub(super) kind: Kind,
    /// rustc's `opt_item_name`: none for an impl or a `use`; a positional field's is its index.
    pub(super) name: Option<Name>,
    pub(super) parent: Parent,
    pub(super) foreign: bool,
}

impl Def {
    /// A positional field: `0`, `1`.
    pub(super) fn is_positional(&self) -> bool {
        self.name.as_ref().is_some_and(|it| it.as_tuple_index().is_some())
    }

    /// Its name starts with `_`.
    pub(super) fn is_underscore(&self) -> bool {
        self.name.as_ref().is_some_and(|it| it.as_str().starts_with('_'))
    }
}

/// What an impl is of, resolved once: each part the pass's definition when it is local.
#[derive(Clone, Copy, Debug)]
pub(super) struct ImplOf {
    pub(super) def: DefIdx,
    pub(super) trait_: Option<DefIdx>,
    /// Its self type, when it is an ADT (rustc's `ty::Adt`), not behind a reference or in
    /// another type.
    pub(super) self_adt: Option<DefIdx>,
    /// The principal trait of its self type, when it is a `dyn`.
    pub(super) self_dyn: Option<DefIdx>,
}

/// A module's items, as `hir_module_items` has them.
#[derive(Debug, Default)]
pub(super) struct ModuleItems {
    pub(super) free: Vec<DefIdx>,
    pub(super) foreign: Vec<DefIdx>,
}

#[derive(Debug)]
pub(super) struct Defs {
    pub(super) defs: Vec<Def>,
    ids: HashMap<Id, DefIdx, Global>,
    /// CSR: the children of definition `d` (a trait's or an impl's associated items, an enum's
    /// variants, a struct's, a union's or a variant's fields, in order) are
    /// `child_list[child_at[d]..child_at[d + 1]]`.
    child_at: Vec<u32>,
    child_list: Vec<DefIdx>,
    /// Every impl and builtin derive's impl, in definition order.
    impls: Vec<ImplOf>,
    /// In `crate::checks::modules` order.
    pub(super) modules: Vec<ModuleItems>,
    pub(super) module_ids: Vec<hir::Module>,
    /// The index in `modules` of the module each definition is in.
    pub(super) module_of: Vec<u32>,
    /// `hir_crate_items(()).owners()`: free items, trait items, impl items, foreign items.
    pub(super) owners: Vec<DefIdx>,
    /// What is live is not known: a `global_asm!` names an item in a `sym` or `const` operand
    /// (the front end keeps no `global_asm!`), or a `pub macro` makes the items of its module
    /// reachable (`update_reachability_from_macro`, not ported).
    pub(super) unported: bool,
    /// Some definition is in a closure or an anonymous constant ([`Parent::Anon`]), so which of
    /// those are walked matters.
    pub(super) in_anon: bool,
    /// The impls an expanded `#[derive(Clone)]` or `#[derive(Debug)]` makes, with the trait's
    /// name: `rustc_trivial_field_reads`, so their items read nothing (`should_ignore_item`).
    pub(super) trivial_derives: HashMap<DefIdx, &'static str, Global>,
}

/// Where a numbering puts the impls a builtin derive makes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Order {
    /// After their ADT, as rustc's `ItemCollector` meets them.
    Written,
    /// After the last item of their module: a planted bug of the numbering, which its test
    /// against the syntax walk catches.
    #[cfg(test)]
    DerivesLast,
}

impl Defs {
    pub(super) fn of(cx: &Cx<'_, '_>) -> Defs {
        Defs::numbered(cx, Order::Written)
    }

    pub(super) fn numbered(cx: &Cx<'_, '_>, order: Order) -> Defs {
        let db = cx.db;
        let mut walk = Walk {
            cx,
            krate: cx.krate.base(),
            defs: Vec::new(), // alloc: setup, the crate's tables, once per crate
            ids: HashMap::new_in(Global),
            edges: Vec::new(),          // alloc: setup, once per crate
            modules: Vec::new(),        // alloc: setup, once per crate
            module_of: Vec::new(),      // alloc: setup, once per crate
            trait_items: Vec::new(),    // alloc: setup, once per crate
            impl_items: Vec::new(),     // alloc: setup, once per crate
            free_owners: Vec::new(),    // alloc: setup, once per crate
            foreign_owners: Vec::new(), // alloc: setup, once per crate
            derives: HashMap::new_in(Global),
            scopes: HashSet::new_in(Global),
            unported: false,
            trivial_derives: HashMap::new_in(Global),
            block_now: None,
            order,
            later: Vec::new(), // alloc: empty unless a test plants a reorder
        };
        let module_ids = crate::checks::modules(cx);
        walk.modules.reserve_exact(module_ids.len());
        for &module in &module_ids {
            walk.modules.push(ModuleItems::default());
            let id: ModuleId = module.into();
            let data = id.def_map(db).module(db, id);
            let scope = Scope::of(db, data.scope(db));
            match data.origin {
                ModuleOrigin::CrateRoot { definition } | ModuleOrigin::File { definition, .. } => {
                    let file: HirFileId = definition.into();
                    let tree = item_tree::file_item_tree(db, file, walk.krate);
                    walk.tree(&scope, tree, file, tree.top_level_items(), Container::Module, Parent::Module);
                }
                ModuleOrigin::Inline { definition_tree_id, definition } => {
                    let file = definition_tree_id.file_id();
                    let tree = item_tree::file_item_tree(db, file, walk.krate);
                    let Some(item) = find(tree, tree.top_level_items(), definition.erase()) else {
                        panic!("{id:?}: its tree of {file:?} has no `mod` item {definition:?}")
                    };
                    walk.tree(&scope, tree, file, tree.nested_items(item), Container::Module, Parent::Module);
                }
                ModuleOrigin::BlockExpr { .. } => continue,
            }
            for (adt, parent) in std::mem::take(&mut walk.later) {
                walk.derived(adt, parent);
            }
        }
        let (child_at, child_list) = csr(walk.defs.len(), &walk.edges);
        let mut owners = walk.free_owners;
        owners.extend(walk.trait_items);
        owners.extend(walk.impl_items);
        owners.extend(walk.foreign_owners);
        let in_anon = walk.defs.iter().any(|it| matches!(it.parent, Parent::Anon(_)));
        let mut defs = Defs {
            defs: walk.defs,
            ids: walk.ids,
            child_at,
            child_list,
            impls: Vec::new(), // alloc: empty until `resolve_impls` fills it below
            modules: walk.modules,
            module_ids,
            module_of: walk.module_of,
            owners,
            unported: walk.unported,
            in_anon,
            trivial_derives: walk.trivial_derives,
        };
        defs.impls = defs.resolve_impls(cx);
        defs
    }

    /// What each impl's trait and self type are, one query each.
    fn resolve_impls(&self, cx: &Cx<'_, '_>) -> Vec<ImplOf> {
        let db = cx.db;
        let mut impls = Vec::new(); // alloc: setup, once per crate
        for def in self.all() {
            let of = match self[def].id {
                Id::Impl(id) => {
                    let impl_ = hir::Impl::from(id);
                    let self_ty = impl_.self_ty(db);
                    ImplOf {
                        def,
                        trait_: impl_.trait_(db).and_then(|it| self.get(Id::Trait(it.into()))),
                        self_adt: self_ty
                            .as_adt()
                            .and_then(|it| self.get(Id::Adt(AdtId::from(it)))),
                        self_dyn: self_ty
                            .as_dyn_trait()
                            .and_then(|it| self.get(Id::Trait(it.into()))),
                    }
                }
                Id::Derive(id) => ImplOf {
                    def,
                    trait_: None,
                    self_adt: self.get(Id::Adt(id.loc(db).adt)),
                    self_dyn: None,
                },
                _ => continue,
            };
            impls.push(of);
        }
        impls
    }

    /// Where `def` is written: its item's, variant's or field's node; none for what a builtin
    /// derive makes. Asked only where a finding or a level is made: the numbering holds no
    /// position.
    pub(super) fn node(&self, cx: &Cx<'_, '_>, def: DefIdx) -> Option<InFile<SyntaxNodePtr>> {
        let db = cx.db;
        let erased = |ast: ErasedAstId| InFile::new(ast.file_id, ast.to_ptr(db));
        Some(match self[def].id {
            Id::Fn(it) => erased(it.lookup(db).id.erase()),
            Id::Const(it) => erased(it.lookup(db).id.erase()),
            Id::Static(it) => erased(it.lookup(db).id.erase()),
            Id::Adt(AdtId::StructId(it)) => erased(it.lookup(db).id.erase()),
            Id::Adt(AdtId::UnionId(it)) => erased(it.lookup(db).id.erase()),
            Id::Adt(AdtId::EnumId(it)) => erased(it.lookup(db).id.erase()),
            Id::Variant(it) => erased(it.lookup(db).id.erase()),
            Id::Trait(it) => erased(it.lookup(db).id.erase()),
            Id::Alias(it) => erased(it.lookup(db).id.erase()),
            Id::Impl(it) => erased(it.lookup(db).id.erase()),
            Id::Use(it) => erased(it.lookup(db).id.erase()),
            Id::Field(FieldId { parent, local_id }) => {
                let source = parent.child_source_view(db, local_id);
                let node = source.value?.either(|it| it.syntax(), |it| it.syntax());
                InFile::new(source.file_id, ast::ptr(node))
            }
            Id::Derive(_) | Id::DeriveMethod(_) => return None,
        })
    }

    pub(super) fn get(&self, id: Id) -> Option<DefIdx> {
        self.ids.get(&id).copied()
    }

    pub(super) fn len(&self) -> usize {
        self.defs.len()
    }

    /// Every definition, in order.
    pub(super) fn all(&self) -> std::ops::Range<DefIdx> {
        0..self.defs.len() as DefIdx
    }

    pub(super) fn children(&self, def: DefIdx) -> &[DefIdx] {
        let at = def as usize;
        &self.child_list[self.child_at[at] as usize..self.child_at[at + 1] as usize]
    }

    /// Every impl, in definition order.
    pub(super) fn impls(&self) -> &[ImplOf] {
        &self.impls
    }

    /// What the impl `def` is of, by a binary search of the impls.
    pub(super) fn impl_of(&self, def: DefIdx) -> Option<&ImplOf> {
        let at = self.impls.binary_search_by_key(&def, |it| it.def).ok()?;
        Some(&self.impls[at])
    }

    /// The item of the local trait that `def`, an item of a trait impl, implements.
    pub(super) fn trait_item_of(&self, def: DefIdx) -> Option<DefIdx> {
        let Parent::Def(impl_) = self[def].parent else { return None };
        let trait_ = self.impl_of(impl_)?.trait_?;
        let (name, kind) = (self[def].name.as_ref()?, self[def].kind);
        self.children(trait_)
            .iter()
            .copied()
            .find(|&it| self[it].kind.same_assoc(kind) && self[it].name.as_ref() == Some(name))
    }
}

impl std::ops::Index<DefIdx> for Defs {
    type Output = Def;
    #[inline]
    fn index(&self, index: DefIdx) -> &Def {
        &self.defs[index as usize]
    }
}

/// The CSR of `edges` (owner, child), each owner's children in the order they were added: a
/// counting sort by owner, two passes.
fn csr(len: usize, edges: &[(DefIdx, DefIdx)]) -> (Vec<u32>, Vec<DefIdx>) {
    // alloc: setup, the CSR's offsets and list and the sort's cursors, once per crate
    radix::Csr::build(len, edges.iter().copied()).into_parts()
}

/// The item of `tree` among `items` (and the items of the inline modules and extern blocks
/// among them) whose ast id is `ast`.
fn find(tree: &ItemTree, items: &[ModItemId], ast: span::ErasedFileAstId) -> Option<ModItemId> {
    items.iter().copied().find_map(|item| {
        if item.ast_id(tree).erase() == ast {
            return Some(item);
        }
        find(tree, tree.nested_items(item), ast)
    })
}

/// What one scope (a module's, or a block's) made of the items of its trees, by the ast id of
/// each item: its ids, and the macro calls it expanded in place of items. Built once per scope
/// the numbering reads.
struct Scope {
    by_ast: ItemsByAst,
    /// The macro calls written as items.
    calls: HashMap<ErasedAstId, MacroCallId, Global>,
    /// The attribute macros of items: the item is its expansion.
    attrs: HashMap<ErasedAstId, MacroCallId, Global>,
    /// The derives of each ADT that are expanded (not builtin), in the order they are written.
    derives: HashMap<ErasedAstId, SmallVec<[MacroCallId; 2]>, Global>,
}

impl Scope {
    fn of(db: &base_db::Db, scope: &ItemScope) -> Scope {
        let mut calls = HashMap::new_in(Global); // alloc: setup, once per scope numbered
        for (call, &id) in scope.iter_macro_invoc() {
            calls.insert(call.erase(), id);
        }
        let mut attrs = HashMap::new_in(Global); // alloc: setup, once per scope numbered
        for (item, id) in scope.attr_macro_invocs() {
            attrs.insert(item.erase(), id);
        }
        let mut derives = HashMap::new_in(Global); // alloc: setup, once per scope numbered
        for (adt, invocs) in scope.derive_macro_invocs() {
            // (the attribute's index among the item's, the derive's among the attribute's)
            let mut written: SmallVec<[(u32, u32, MacroCallId); 2]> = SmallVec::new();
            for (attr, _, ids) in invocs {
                for (at, id) in ids.iter().enumerate() {
                    if let Some(Either::Left(call)) = *id {
                        written.push((attr.item_tree_index(), at as u32, call));
                    }
                }
            }
            if written.is_empty() {
                continue;
            }
            written.sort_unstable_by_key(|&(attr, at, _)| (attr, at));
            derives.insert(adt.erase(), written.into_iter().map(|(_, _, call)| call).collect());
        }
        Scope { by_ast: scope.items_by_ast(db), calls, attrs, derives }
    }

    /// The scope of the block `block`: its def map's root module's.
    fn of_block(db: &base_db::Db, block: BlockId) -> Scope {
        let map = block_def_map(db, block);
        Scope::of(db, map.module(db, map.root_module_id()).scope(db))
    }
}

/// What an item is an item of.
#[derive(Clone, Copy)]
enum Container {
    Module,
    Trait(DefIdx),
    Impl(DefIdx),
    Extern,
}

struct Walk<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    krate: base_db::Crate,
    defs: Vec<Def>,
    ids: HashMap<Id, DefIdx, Global>,
    /// (owner, child), in the order the children are met.
    edges: Vec<(DefIdx, DefIdx)>,
    modules: Vec<ModuleItems>,
    module_of: Vec<u32>,
    free_owners: Vec<DefIdx>,
    trait_items: Vec<DefIdx>,
    impl_items: Vec<DefIdx>,
    foreign_owners: Vec<DefIdx>,
    /// The builtin derives of each ADT, of the scopes read so far.
    derives: HashMap<AdtId, SmallVec<[BuiltinDeriveImplId; 4]>, Global>,
    scopes: HashSet<hir_def::ModuleId, Global>,
    unported: bool,
    trivial_derives: HashMap<DefIdx, &'static str, Global>,
    /// The block whose items are being defined, while a body's are: the tree an item of it
    /// is in ([`Walk::tree_of`]).
    block_now: Option<BlockId>,
    order: Order,
    /// The ADTs whose builtin derives [`Order::DerivesLast`] defines after their module.
    later: Vec<(AdtId, Parent)>,
}

impl<'db> Walk<'_, '_, 'db> {
    /// The items `items` of `tree` (of `file`), in order, each in `scope`.
    fn tree(
        &mut self,
        scope: &Scope,
        tree: &ItemTree,
        file: HirFileId,
        items: &[ModItemId],
        container: Container,
        parent: Parent,
    ) {
        for &item in items {
            let ast = InFile::new(file, item.ast_id(tree).erase());
            match tree.kind(item) {
                ItemKind::MacroCall => {
                    if let Some(&call) = scope.calls.get(&ast) {
                        self.expansion(scope, call, container, parent);
                    }
                }
                // A module's items are its own; a `macro_rules!` and an `extern crate` define
                // nothing the pass lints.
                ItemKind::Mod | ItemKind::MacroRules | ItemKind::ExternCrate => {}
                // A `cfg`'d-out block has no id.
                ItemKind::ExternBlock => {
                    if scope.by_ast.def_of_ast(ast).is_some() {
                        self.tree(scope, tree, file, tree.nested_items(item), Container::Extern, parent);
                    }
                }
                _ => self.item(scope, ast, container, parent),
            }
        }
    }

    /// The items of the expansion of `call`, in place of it.
    fn expansion(&mut self, scope: &Scope, call: MacroCallId, container: Container, parent: Parent) {
        let file = HirFileId::from(call);
        let tree = item_tree::file_item_tree(self.cx.db, file, self.krate);
        self.tree(scope, tree, file, tree.top_level_items(), container, parent);
    }

    /// The item `ast` of `scope` (no macro call, which is its expansion): its expansion when an
    /// attribute macro is on it, else its definition, if the collector made one.
    fn item(&mut self, scope: &Scope, ast: ErasedAstId, container: Container, parent: Parent) {
        if let Some(&call) = scope.attrs.get(&ast) {
            self.expansion(scope, call, container, parent);
            return;
        }
        let Some(of) = scope.by_ast.def_of_ast(ast) else { return };
        self.define(scope, ast, of, container, parent);
    }

    fn push(&mut self, def: Def) -> DefIdx {
        let index =
            DefIdx::try_from(self.defs.len()).expect("fewer than 2^32 definitions in a crate");
        self.ids.insert(def.id, index);
        self.defs.push(def);
        self.module_of.push((self.modules.len() - 1) as u32);
        index
    }

    /// Records the item `of` (written at `ast`) as a definition with its variants, fields and
    /// derives, then what is in it: a trait's or an impl's items, the items of its bodies.
    fn define(
        &mut self,
        scope: &Scope,
        ast: ErasedAstId,
        of: ItemOfAst,
        container: Container,
        parent: Parent,
    ) {
        let db = self.cx.db;
        let assoc = matches!(container, Container::Trait(_) | Container::Impl(_));
        let foreign = matches!(container, Container::Extern);
        let parent = match container {
            Container::Trait(it) | Container::Impl(it) => Parent::Def(it),
            Container::Module | Container::Extern => parent,
        };
        let def = |id, kind, name| Def { id, kind, name, parent, foreign };
        let (def, adt) = match of {
            ItemOfAst::Def(ModuleDefId::FunctionId(id)) => {
                let function = hir::Function::from(id);
                let kind = match assoc {
                    true => Kind::AssocFn { method: function.has_self_param(db) },
                    false => Kind::Fn,
                };
                (def(Id::Fn(id), kind, Some(function.name(db))), None)
            }
            ItemOfAst::Def(ModuleDefId::ConstId(id)) => {
                let kind = if assoc { Kind::AssocConst } else { Kind::Const };
                let name = hir::Const::from(id)
                    .name(db)
                    .unwrap_or_else(|| Name::new_symbol_root(sym::underscore));
                (def(Id::Const(id), kind, Some(name)), None)
            }
            ItemOfAst::Def(ModuleDefId::StaticId(id)) => {
                (def(Id::Static(id), Kind::Static, Some(hir::Static::from(id).name(db))), None)
            }
            ItemOfAst::Def(ModuleDefId::AdtId(id)) => {
                let (kind, name) = match id {
                    AdtId::StructId(it) => (Kind::Struct, hir::Struct::from(it).name(db)),
                    AdtId::UnionId(it) => (Kind::Union, hir::Union::from(it).name(db)),
                    AdtId::EnumId(it) => (Kind::Enum, hir::Enum::from(it).name(db)),
                };
                (def(Id::Adt(id), kind, Some(name)), Some(id))
            }
            ItemOfAst::Def(ModuleDefId::TraitId(id)) => {
                let kind = match TraitSignature::of(db, id).flags.contains(TraitFlags::ALIAS) {
                    true => Kind::TraitAlias,
                    false => Kind::Trait,
                };
                (def(Id::Trait(id), kind, Some(hir::Trait::from(id).name(db))), None)
            }
            ItemOfAst::Def(ModuleDefId::TypeAliasId(id)) => {
                let kind = match (assoc, foreign) {
                    (true, _) => Kind::AssocTy,
                    (_, true) => Kind::ForeignTy,
                    _ => Kind::TyAlias,
                };
                (def(Id::Alias(id), kind, Some(hir::TypeAlias::from(id).name(db))), None)
            }
            ItemOfAst::Impl(id) => {
                let of_trait = ImplSignature::of(db, id).target_trait.is_some();
                (def(Id::Impl(id), Kind::Impl { of_trait }, None), None)
            }
            ItemOfAst::Use(id) => (def(Id::Use(id), Kind::Use, None), None),
            ItemOfAst::Def(ModuleDefId::MacroId(MacroId::Macro2Id(_))) => {
                // A `pub macro` makes its module's items reachable, which is not ported.
                let node = self.cx.syntax_view(InFile::new(ast.file_id, ast.to_ptr(db)));
                if ast::MacroDef::cast(node.value)
                    .is_some_and(|it| ast::HasVisibility::visibility(&it).is_some())
                {
                    self.unported = true;
                }
                return;
            }
            // An extern block in a body (one of a module's is walked from its tree).
            ItemOfAst::ExternBlock(_) => {
                self.extern_in_block(scope, ast, parent);
                return;
            }
            ItemOfAst::Def(_) | ItemOfAst::ExternCrate(_) => return,
        };
        let id = def.id;
        let index = self.push(def);
        match container {
            Container::Trait(owner) => {
                self.edges.push((owner, index));
                self.trait_items.push(index);
            }
            Container::Impl(owner) => {
                self.edges.push((owner, index));
                self.impl_items.push(index);
            }
            Container::Extern => {
                self.foreign_owners.push(index);
                self.current().foreign.push(index);
            }
            Container::Module => {
                self.free_owners.push(index);
                self.current().free.push(index);
            }
        }
        if let Some(adt) = adt {
            self.fields_and_variants(adt, index);
            match self.order {
                Order::Written => self.derived(adt, parent),
                #[cfg(test)]
                Order::DerivesLast => self.later.push((adt, parent)),
            }
            self.expanded_derives(scope, ast, parent);
        }
        self.inside(index, id, foreign);
    }

    /// The items of an extern block written in a body, as foreign items of the enclosing module
    /// with the block's parent: found in the tree its ast id is of.
    fn extern_in_block(&mut self, scope: &Scope, ast: ErasedAstId, parent: Parent) {
        let (tree, file) = self.tree_of(ast.file_id);
        let Some(item) = find(tree, tree.top_level_items(), ast.value) else {
            panic!("extern block {ast:?}: its tree of {file:?} has no such item")
        };
        self.tree(scope, tree, file, tree.nested_items(item), Container::Extern, parent);
    }

    /// The tree of the items of `file` in the block being walked: the block's own when `file`
    /// is the block's, else the macro expansion's ([`Walk::block_now`]).
    fn tree_of(&self, file: HirFileId) -> (&'db ItemTree, HirFileId) {
        let db = self.cx.db;
        match self.block_now {
            Some(block) if block.ast_id(db).file_id == file => {
                (item_tree::block_item_tree(db, block, self.krate), file)
            }
            _ => (item_tree::file_item_tree(db, file, self.krate), file),
        }
    }

    /// What is in the definition `def` (of `id`): the items written in each expression store it
    /// owns, in rustc's order ([`stores`]: its signature's anonymous constants, its fields',
    /// its body or an enum's discriminants), then a trait's or an impl's items, each followed by
    /// what is in it. A foreign function or static has no body.
    fn inside(&mut self, def: DefIdx, id: Id, foreign: bool) {
        let db = self.cx.db;
        stores(db, id, |owner| match owner {
            ExpressionStoreOwnerId::Body(_) if foreign => {}
            ExpressionStoreOwnerId::Body(body) => self.body(body, def),
            ExpressionStoreOwnerId::Signature(_) | ExpressionStoreOwnerId::VariantFields(_) => {
                self.store_items(owner, def)
            }
        });
        match id {
            Id::Trait(it) => {
                for &(_, item) in it.trait_items(db).items.iter() {
                    self.assoc(item, Container::Trait(def));
                }
            }
            Id::Impl(it) => {
                for &(_, item) in it.impl_items(db).items.iter() {
                    self.assoc(item, Container::Impl(def));
                }
            }
            _ => {}
        }
    }

    /// An associated item of a trait or an impl, then what is in it.
    fn assoc(&mut self, item: AssocItemId, container: Container) {
        let db = self.cx.db;
        let (Container::Trait(owner) | Container::Impl(owner)) = container else {
            unreachable!("an associated item is a trait's or an impl's")
        };
        let parent = Parent::Def(owner);
        let (id, kind, name) = match item {
            AssocItemId::FunctionId(it) => {
                let function = hir::Function::from(it);
                (Id::Fn(it), Kind::AssocFn { method: function.has_self_param(db) }, function.name(db))
            }
            AssocItemId::ConstId(it) => {
                let name = hir::Const::from(it)
                    .name(db)
                    .unwrap_or_else(|| Name::new_symbol_root(sym::underscore));
                (Id::Const(it), Kind::AssocConst, name)
            }
            AssocItemId::TypeAliasId(it) => {
                (Id::Alias(it), Kind::AssocTy, hir::TypeAlias::from(it).name(db))
            }
        };
        let index = self.push(Def { id, kind, name: Some(name), parent, foreign: false });
        self.edges.push((owner, index));
        match container {
            Container::Trait(_) => self.trait_items.push(index),
            _ => self.impl_items.push(index),
        }
        self.inside(index, id, false);
    }

    /// The items written in the body of `owner` (the definition `def`), in source order, each
    /// a free item of the enclosing module whose parent is the closure or anonymous constant
    /// around it, else `def`; each followed by what is in it.
    fn body(&mut self, owner: DefWithBodyId, def: DefIdx) {
        let db = self.cx.db;
        let items = item_tree::body_items(db, owner);
        if items.is_empty() {
            return;
        }
        let body = Body::of(db, owner);
        let discriminant = matches!(owner, DefWithBodyId::VariantId(_));
        let anons = anons(&body.store, std::iter::once(body.root_expr()), discriminant);
        // alloc: setup, the scopes of the body's blocks, once per body holding items
        let mut scopes: HashMap<BlockId, Scope, Global> = HashMap::new_in(Global);
        for item in items {
            if !scopes.contains_key(&item.block) {
                scopes.insert(item.block, Scope::of_block(db, item.block));
            }
        }
        let outer = self.block_now;
        for item in items {
            let Some(&anon) = anons.get(&item.block) else {
                panic!(
                    "{owner:?}: block {:?} of item {:?} is in no expression of the body's store",
                    item.block, item.item
                )
            };
            let parent = match anon {
                Some(expr) => Parent::Anon((ExpressionStoreOwnerId::Body(owner), expr)),
                None => Parent::Def(def),
            };
            let scope = scopes.get(&item.block).expect("a scope was made for each block above");
            self.block_now = Some(item.block);
            if scope.by_ast.def_of_ast(item.item).is_none() && !scope.attrs.contains_key(&item.item) {
                self.global_asm(item.item);
                continue;
            }
            self.item(scope, item.item, Container::Module, parent);
        }
        self.block_now = outer;
    }

    /// The items written in the anonymous constants of the store of `owner` (a signature's or
    /// a variant's fields', where every expression root is one), block by block as the store
    /// lowered them, each a free item of the enclosing module whose parent is the closure or
    /// anonymous constant innermost around it; each followed by what is in it. A block nested
    /// in another's statement has its items after the outer block's (rustc interleaves them).
    fn store_items(&mut self, owner: ExpressionStoreOwnerId, def: DefIdx) {
        let db = self.cx.db;
        let store = ExpressionStore::of(db, owner);
        let mut blocks = store.blocks(db).map(|(block, _)| block).peekable();
        if blocks.peek().is_none() {
            return;
        }
        let anons = anons(store, store.expr_roots(), true);
        let outer = self.block_now;
        for block in blocks {
            let Some(&anon) = anons.get(&block) else {
                panic!("{owner:?}: block {block:?} is in no expression of the store")
            };
            let parent = match anon {
                Some(expr) => Parent::Anon((owner, expr)),
                None => Parent::Def(def),
            };
            let tree = item_tree::block_item_tree(db, block, self.krate);
            let scope = Scope::of_block(db, block);
            self.block_now = Some(block);
            let file = block.ast_id(db).file_id;
            self.tree(&scope, tree, file, tree.top_level_items(), Container::Module, parent);
        }
        self.block_now = outer;
    }

    /// A `global_asm!` naming an item in a `sym` or `const` operand, written in a body: what is
    /// live is then not known ([`Defs::unported`]). Any other item with no id (a `mod`, a
    /// `macro_rules!`, an `asm!` expression) is nothing here.
    fn global_asm(&mut self, ast: ErasedAstId) {
        let db = self.cx.db;
        let ptr = ast.to_ptr(db);
        if ptr.kind() != SyntaxKind::ASM_EXPR {
            return;
        }
        let node = self.cx.syntax_view(InFile::new(ast.file_id, ptr)).value;
        let item = matches!(
            node.parent().map(|it| it.kind()),
            Some(SyntaxKind::SOURCE_FILE | SyntaxKind::ITEM_LIST | SyntaxKind::MACRO_ITEMS)
        );
        self.unported |= item
            && node
                .descendants()
                .any(|it| matches!(it.kind(), SyntaxKind::ASM_SYM | SyntaxKind::ASM_CONST));
    }

    /// The items the expanded derives of the ADT at `ast` make, after it, where they are
    /// expanded (a build; the front end records the builtin ones of a check instead,
    /// [`Walk::derived`]): free items of its module. The impls of `Clone`'s and `Debug`'s are
    /// [`Defs::trivial_derives`].
    fn expanded_derives(&mut self, scope: &Scope, ast: ErasedAstId, parent: Parent) {
        let db = self.cx.db;
        let Some(calls) = scope.derives.get(&ast) else { return };
        for &call in calls {
            let trivial = match call.loc(db).def.kind {
                hir_expand::MacroDefKind::BuiltInDerive(_, BuiltinDeriveExpander::Clone) => {
                    Some("Clone")
                }
                hir_expand::MacroDefKind::BuiltInDerive(_, BuiltinDeriveExpander::Debug) => {
                    Some("Debug")
                }
                _ => None,
            };
            let first = self.defs.len();
            self.expansion(scope, call, Container::Module, parent);
            let Some(name) = trivial else { continue };
            for at in first..self.defs.len() {
                if self.defs[at].kind == (Kind::Impl { of_trait: true }) {
                    self.trivial_derives.insert(at as DefIdx, name);
                }
            }
        }
    }

    fn current(&mut self) -> &mut ModuleItems {
        self.modules.last_mut().expect("items are walked within a module")
    }

    /// An ADT's variants, each with its fields, or its fields, in order (those `cfg`'d out
    /// have no id).
    fn fields_and_variants(&mut self, adt: AdtId, def: DefIdx) {
        let db = self.cx.db;
        match adt {
            AdtId::StructId(it) => self.fields(VariantId::StructId(it), def),
            AdtId::UnionId(it) => self.fields(VariantId::UnionId(it), def),
            AdtId::EnumId(it) => {
                for &(variant, _) in it.enum_variants(db).variants.values() {
                    let index = self.push(Def {
                        id: Id::Variant(variant),
                        kind: Kind::Variant,
                        name: Some(hir::EnumVariant::from(variant).name(db)),
                        parent: Parent::Def(def),
                        foreign: false,
                    });
                    self.edges.push((def, index));
                    self.fields(VariantId::EnumVariantId(variant), index);
                }
            }
        }
    }

    fn fields(&mut self, variant: VariantId, owner: DefIdx) {
        let db = self.cx.db;
        for (local_id, field) in VariantFields::of(db, variant).fields().iter() {
            let index = self.push(Def {
                id: Id::Field(FieldId { parent: variant, local_id }),
                kind: Kind::Field,
                // clone: an interned name, a count bump
                name: Some(field.name.clone()),
                parent: Parent::Def(owner),
                foreign: false,
            });
            self.edges.push((owner, index));
        }
    }

    /// The impls the builtin derives of `adt` make, after it, and their methods.
    fn derived(&mut self, adt: AdtId, parent: Parent) {
        let db = self.cx.db;
        let module = adt.module(db);
        if self.scopes.insert(module) {
            let scope = module.def_map(db).module(db, module).scope(db);
            for derive in scope.builtin_derive_impls() {
                self.derives.entry(derive.loc(db).adt).or_default().push(derive);
            }
        }
        let Some(mut derives) = self.derives.remove(&adt) else { return };
        radix::sort_by_key(&mut derives[..], |it| {
            let loc = it.loc(db);
            radix::pack_32(loc.derive_attr_id.item_tree_index(), loc.derive_index)
        });
        for derive in derives {
            let trait_ = derive.loc(db).trait_;
            let index = self.push(Def {
                id: Id::Derive(derive),
                kind: Kind::Impl { of_trait: true },
                name: None,
                parent,
                foreign: false,
            });
            self.free_owners.push(index);
            self.current().free.push(index);
            if let Some(method) = trait_.all_methods().first() {
                let method = self.push(Def {
                    id: Id::DeriveMethod(derive),
                    kind: Kind::AssocFn {
                        method: !matches!(trait_, BuiltinDeriveImplTrait::Default),
                    },
                    name: Some(Name::new_symbol_root(method.name())),
                    parent: Parent::Def(index),
                    foreign: false,
                });
                self.edges.push((index, method));
                self.impl_items.push(method);
            }
        }
    }
}

/// Of each block of `store` under the expressions `roots`, the closure, coroutine, `const`
/// block or anonymous constant innermost around it, if any: what the items in the block have
/// as their parent. The same expressions the walk of the store (`mark::body::Walk::anon`)
/// marks visited; each root is one itself when `anon_roots` (a variant's discriminant, every
/// root of a signature's or of fields' store).
fn anons(
    store: &ExpressionStore,
    roots: impl Iterator<Item = ExprId>,
    anon_roots: bool,
) -> HashMap<BlockId, Option<ExprId>, Global> {
    struct Anons<'s> {
        store: &'s ExpressionStore,
        at: Option<ExprId>,
        blocks: HashMap<BlockId, Option<ExprId>, Global>,
    }
    impl Anons<'_> {
        fn within(&mut self, anon: ExprId, walk: impl FnOnce(&mut Self)) {
            let outer = self.at.replace(anon);
            walk(self);
            self.at = outer;
        }
    }
    impl StoreVisitor for Anons<'_> {
        fn on_expr(&mut self, expr: ExprId) {
            let store = self.store;
            match store.expr(expr) {
                Expr::Closure { closure_kind, .. }
                    if !matches!(
                        closure_kind,
                        ClosureKind::Coroutine { source: CoroutineSource::Fn, .. }
                    ) =>
                {
                    self.within(expr, |this| store.visit_expr_children(expr, &mut *this));
                    return;
                }
                Expr::Const(inner) => {
                    self.within(expr, |this| this.on_expr(inner));
                    return;
                }
                Expr::Block { id: Some(block), .. } => {
                    self.blocks.insert(block, self.at);
                }
                _ => {}
            }
            stdx::ensure_sufficient_stack(|| store.visit_expr_children(expr, &mut *self));
        }

        fn on_anon_const_expr(&mut self, expr: ExprId) {
            self.within(expr, |this| this.on_expr(expr));
        }

        fn on_pat(&mut self, pat: PatId) {
            let store = self.store;
            store.visit_pat_children(pat, &mut *self);
        }

        fn on_type(&mut self, ty: TypeRefId) {
            let store = self.store;
            store.visit_type_ref_children(ty, &mut *self);
        }
    }
    // alloc: setup, one entry per block of a store holding items, once per such store
    let mut anons = Anons { store, at: None, blocks: HashMap::new_in(Global) };
    for root in roots {
        anons.at = anon_roots.then_some(root);
        anons.on_expr(root);
    }
    anons.blocks
}

/// Each expression store the definition `id` owns, in rustc's `intravisit` order: its signature,
/// then a struct's or a union's fields, each variant's fields and then its discriminant, or the
/// body a function, a constant or a static has written. A `use`, a variant and a field own
/// none (a variant's are its enum's). The one list both the numbering ([`Walk::inside`]) and
/// the walk (`mark::Mark::visit`) go by.
pub(super) fn stores(db: &base_db::Db, id: Id, mut f: impl FnMut(ExpressionStoreOwnerId)) {
    let (def, body) = match id {
        Id::Fn(it) => (GenericDefId::FunctionId(it), {
            FunctionSignature::of(db, it).has_body().then_some(DefWithBodyId::FunctionId(it))
        }),
        Id::Const(it) => (GenericDefId::ConstId(it), {
            ConstSignature::of(db, it).has_body().then_some(DefWithBodyId::ConstId(it))
        }),
        Id::Static(it) => (GenericDefId::StaticId(it), {
            let written = StaticSignature::of(db, it).flags.contains(StaticFlags::HAS_BODY);
            written.then_some(DefWithBodyId::StaticId(it))
        }),
        Id::Adt(it) => (GenericDefId::AdtId(it), None),
        Id::Trait(it) => (GenericDefId::TraitId(it), None),
        Id::Alias(it) => (GenericDefId::TypeAliasId(it), None),
        Id::Impl(it) => (GenericDefId::ImplId(it), None),
        Id::Variant(_) | Id::Field(_) | Id::Derive(_) | Id::DeriveMethod(_) | Id::Use(_) => return,
    };
    f(ExpressionStoreOwnerId::Signature(def));
    match id {
        Id::Adt(AdtId::StructId(it)) => f(ExpressionStoreOwnerId::VariantFields(it.into())),
        Id::Adt(AdtId::UnionId(it)) => f(ExpressionStoreOwnerId::VariantFields(it.into())),
        Id::Adt(AdtId::EnumId(it)) => {
            let variants = it.enum_variants(db);
            for &(variant, _) in variants.variants.values() {
                f(ExpressionStoreOwnerId::VariantFields(VariantId::EnumVariantId(variant)));
                if variants.has_explicit_discriminant(variant) {
                    f(ExpressionStoreOwnerId::Body(DefWithBodyId::VariantId(variant)));
                }
            }
        }
        _ => {}
    }
    if let Some(body) = body {
        f(ExpressionStoreOwnerId::Body(body));
    }
}
