//! The crate's numbering as the dead-code pass made it from the crate's syntax before it was
//! made from def maps and item trees ([`super::Defs::of`]): one walk of every module's syntax,
//! pre-order, macro calls expanded in place, every item a definition when `Semantics` maps it
//! to one. Kept as the reference the numbering is checked equal to
//! (`dead::tests::the_numbering_equals_the_syntax_walk`), on every fixture of the pass.

use blaze_hash::{HashMap, HashSet};
use blaze_ids::radix;
use hir::{HirFileId, InFile, ModuleSourceView, Name};
use hir_def::{
    AdtId, BuiltinDeriveImplId, ConstId, FieldId, FunctionId, HasModule, ImplId, StaticId,
    TraitId, TypeAliasId, builtin_derive::BuiltinDeriveImplTrait,
};
use hir_expand::builtin::BuiltinDeriveExpander;
use intern::sym;
use smallvec::SmallVec;
use std::alloc::Global;
use syntax::{
    SyntaxKind, SyntaxNodePtr, T,
    view::{self as ast, AstNode as _, HasModuleItem as _},
};

use super::{DefIdx, Kind, ModuleItems};
use crate::cx::Cx;

/// What a definition is: the numbering's ids, a `use` by its node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::lints::names::dead) enum Id {
    Fn(FunctionId),
    Const(ConstId),
    Static(StaticId),
    Adt(AdtId),
    Variant(hir_def::EnumVariantId),
    Field(FieldId),
    Trait(TraitId),
    Alias(TypeAliasId),
    Impl(ImplId),
    Derive(BuiltinDeriveImplId),
    DeriveMethod(BuiltinDeriveImplId),
    Use(InFile<SyntaxNodePtr>),
}

/// A definition's parent, a closure or an anonymous constant by its node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::lints::names::dead) enum Parent {
    Module,
    Def(DefIdx),
    Anon(InFile<SyntaxNodePtr>),
}

#[derive(Debug)]
pub(in crate::lints::names::dead) struct Def {
    pub(in crate::lints::names::dead) id: Id,
    pub(in crate::lints::names::dead) kind: Kind,
    pub(in crate::lints::names::dead) node: Option<InFile<SyntaxNodePtr>>,
    pub(in crate::lints::names::dead) name: Option<Name>,
    pub(in crate::lints::names::dead) parent: Parent,
    pub(in crate::lints::names::dead) foreign: bool,
}

/// The syntax walk's numbering: what [`super::Defs`] holds, before its impls are resolved.
pub(in crate::lints::names::dead) struct Numbering {
    pub(in crate::lints::names::dead) defs: Vec<Def>,
    /// (owner, child), in the order the children are met.
    pub(in crate::lints::names::dead) edges: Vec<(DefIdx, DefIdx)>,
    pub(in crate::lints::names::dead) modules: Vec<ModuleItems>,
    pub(in crate::lints::names::dead) module_of: Vec<u32>,
    pub(in crate::lints::names::dead) owners: Vec<DefIdx>,
    pub(in crate::lints::names::dead) unported: bool,
    pub(in crate::lints::names::dead) trivial_derives: HashMap<DefIdx, &'static str, Global>,
}

impl Numbering {
    pub(in crate::lints::names::dead) fn of(cx: &Cx<'_, '_>) -> Numbering {
        let mut walk = Walk {
            cx,
            defs: Vec::new(), // alloc: a test's reference, once per fixture
            ids: HashMap::new_in(Global),
            edges: Vec::new(),          // alloc: as above
            modules: Vec::new(),        // alloc: as above
            module_of: Vec::new(),      // alloc: as above
            trait_items: Vec::new(),    // alloc: as above
            impl_items: Vec::new(),     // alloc: as above
            free_owners: Vec::new(),    // alloc: as above
            foreign_owners: Vec::new(), // alloc: as above
            derives: HashMap::new_in(Global),
            scopes: HashSet::new_in(Global),
            unported: false,
            trivial_derives: HashMap::new_in(Global),
        };
        let module_ids = crate::checks::modules(cx);
        for &module in &module_ids {
            walk.modules.push(ModuleItems::default());
            let source = module.definition_source_view(cx.db);
            let file = source.file_id;
            let root = cx.vsema.parse_or_expand(file);
            let items = match source.value {
                ModuleSourceView::SourceFile(it) => {
                    ast::SourceFile::cast(ast::to_node(ast::ptr(it.syntax()), root))
                        .map(|it| it.items())
                }
                ModuleSourceView::Module(it) => {
                    ast::Module::cast(ast::to_node(ast::ptr(it.syntax()), root))
                        .and_then(|it| it.item_list())
                        .map(|it| it.items())
                }
                ModuleSourceView::BlockExpr(_) => continue,
            };
            for item in items.into_iter().flatten() {
                walk.item(InFile::new(file, item), Container::Module, Parent::Module);
            }
        }
        let mut owners = walk.free_owners;
        owners.extend(walk.trait_items);
        owners.extend(walk.impl_items);
        owners.extend(walk.foreign_owners);
        Numbering {
            defs: walk.defs,
            edges: walk.edges,
            modules: walk.modules,
            module_of: walk.module_of,
            owners,
            unported: walk.unported,
            trivial_derives: walk.trivial_derives,
        }
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
}

impl<'db> Walk<'_, '_, 'db> {
    fn item(&mut self, item: InFile<ast::Item<'db>>, container: Container, parent: Parent) {
        if !self.cx.cfg_enabled(item.value.syntax()) {
            return;
        }
        if let ast::Item::MacroCall(call) = item.value {
            if let Some(expansion) = self.expand(item.with_value(call)) {
                self.expansion(expansion, container, parent);
            }
            return;
        }
        if self.cx.vsema.is_attr_macro_call(item) {
            if let Some(expansion) = self.cx.vsema.expand_attr_macro(item.value) {
                self.expansion(expansion.value, container, parent);
            }
            return;
        }
        if let ast::Item::MacroDef(it) = item.value
            && ast::HasVisibility::visibility(&it).is_some()
        {
            self.unported = true;
        }
        let file = item.file_id;
        let def = self.define(item, container, parent);
        let (list_container, list_kind) = match (&item.value, def) {
            (ast::Item::Module(_) | ast::Item::MacroRules(_) | ast::Item::MacroDef(_), _)
            | (ast::Item::Trait(_) | ast::Item::Impl(_), None) => return,
            (ast::Item::Trait(_), Some(def)) => {
                (Some(Container::Trait(def)), SyntaxKind::ASSOC_ITEM_LIST)
            }
            (ast::Item::Impl(_), Some(def)) => {
                (Some(Container::Impl(def)), SyntaxKind::ASSOC_ITEM_LIST)
            }
            (ast::Item::ExternBlock(_), _) => {
                (Some(Container::Extern), SyntaxKind::EXTERN_ITEM_LIST)
            }
            _ => (None, SyntaxKind::ASSOC_ITEM_LIST),
        };
        let inner = match def {
            Some(def) => Parent::Def(def),
            None => parent,
        };
        for child in item.value.syntax().children() {
            match list_container {
                Some(container) if child.kind() == list_kind => {
                    let item_parent = match container {
                        Container::Extern => parent,
                        _ => inner,
                    };
                    for it in child.children() {
                        if let Some(item) = ast::Item::cast(it) {
                            self.item(InFile::new(file, item), container, item_parent);
                        }
                    }
                }
                _ => self.node(file, child, inner),
            }
        }
    }

    /// What a macro call or an attribute macro of an item expands to, in place of it.
    fn expansion(&mut self, root: InFile<ast::Node<'db>>, container: Container, parent: Parent) {
        for child in root.value.children() {
            if is_item(child)
                && let Some(item) = ast::Item::cast(child)
            {
                self.item(root.with_value(item), container, parent);
            }
        }
    }

    /// `node`, inside the item whose nested items have `parent`: dispatched on its kind, so that
    /// a node that is no item, macro call or anonymous definition costs its children's walk.
    fn node(&mut self, file: HirFileId, node: ast::Node<'db>, parent: Parent) {
        let kind = node.kind();
        if ast::AnyHasAttrs::can_cast(kind) && !self.cx.cfg_enabled(node) {
            return;
        }
        if is_item(node) {
            if kind == SyntaxKind::ASM_EXPR {
                self.global_asm(node);
            } else if let Some(item) = ast::Item::cast(node) {
                self.item(InFile::new(file, item), Container::Module, parent);
            }
            return;
        }
        if kind == SyntaxKind::MACRO_CALL {
            let Some(call) = ast::MacroCall::cast(node) else { return };
            let Some(expansion) = self.expand(InFile::new(file, call)) else { return };
            match expansion.value.kind() {
                SyntaxKind::MACRO_ITEMS | SyntaxKind::MACRO_STMTS => {
                    for child in expansion.value.children() {
                        self.node(expansion.file_id, child, parent);
                    }
                }
                _ => self.node(expansion.file_id, expansion.value, parent),
            }
            return;
        }
        let parent = match is_anon(node) {
            true => Parent::Anon(anon_key(file, node)),
            false => parent,
        };
        for child in node.children() {
            stdx::ensure_sufficient_stack(|| self.node(file, child, parent));
        }
    }

    fn global_asm(&mut self, node: ast::Node<'_>) {
        let names = node
            .descendants()
            .any(|it| matches!(it.kind(), SyntaxKind::ASM_SYM | SyntaxKind::ASM_CONST));
        self.unported |= names;
    }

    fn expand(&self, call: InFile<ast::MacroCall<'db>>) -> Option<InFile<ast::Node<'db>>> {
        let id = self.cx.vsema.to_def(call.value)?;
        let file: HirFileId = id.into();
        Some(InFile::new(file, self.cx.vsema.parse_or_expand(file)))
    }

    fn push(&mut self, def: Def) -> DefIdx {
        let index =
            DefIdx::try_from(self.defs.len()).expect("fewer than 2^32 definitions in a crate");
        self.ids.insert(def.id, index);
        self.defs.push(def);
        self.module_of.push((self.modules.len() - 1) as u32);
        index
    }

    /// Records `item` as a definition with its variants, fields and derives.
    fn define(
        &mut self,
        item: InFile<ast::Item<'db>>,
        container: Container,
        parent: Parent,
    ) -> Option<DefIdx> {
        let cx = self.cx;
        let db = cx.db;
        let node = Some(item.map(|it| ast::ptr(it.syntax())));
        let assoc = matches!(container, Container::Trait(_) | Container::Impl(_));
        let foreign = matches!(container, Container::Extern);
        let parent = match container {
            Container::Trait(it) | Container::Impl(it) => Parent::Def(it),
            Container::Module | Container::Extern => parent,
        };
        let def = |id, kind, name| Def { id, kind, node, name, parent, foreign };
        let (def, adt) = match item.value {
            ast::Item::Fn(it) => {
                let function = cx.vsema.to_def(it)?;
                let id = FunctionId::try_from(function).ok()?;
                let kind = match assoc {
                    true => Kind::AssocFn { method: function.has_self_param(db) },
                    false => Kind::Fn,
                };
                (def(Id::Fn(id), kind, Some(function.name(db))), None)
            }
            ast::Item::Const(it) => {
                let konst = cx.vsema.to_def(it)?;
                let kind = if assoc { Kind::AssocConst } else { Kind::Const };
                let name = konst.name(db).unwrap_or_else(|| Name::new_symbol_root(sym::underscore));
                (def(Id::Const(ConstId::from(konst)), kind, Some(name)), None)
            }
            ast::Item::Static(it) => {
                let it = cx.vsema.to_def(it)?;
                (def(Id::Static(StaticId::from(it)), Kind::Static, Some(it.name(db))), None)
            }
            ast::Item::Struct(it) => {
                let it = cx.vsema.to_def(it)?;
                let id = AdtId::StructId(it.into());
                (def(Id::Adt(id), Kind::Struct, Some(it.name(db))), Some(id))
            }
            ast::Item::Union(it) => {
                let it = cx.vsema.to_def(it)?;
                let id = AdtId::UnionId(it.into());
                (def(Id::Adt(id), Kind::Union, Some(it.name(db))), Some(id))
            }
            ast::Item::Enum(it) => {
                let it = cx.vsema.to_def(it)?;
                let id = AdtId::EnumId(it.into());
                (def(Id::Adt(id), Kind::Enum, Some(it.name(db))), Some(id))
            }
            ast::Item::Trait(it) => {
                let kind = if it.eq_token().is_some() { Kind::TraitAlias } else { Kind::Trait };
                let it = cx.vsema.to_def(it)?;
                (def(Id::Trait(TraitId::from(it)), kind, Some(it.name(db))), None)
            }
            ast::Item::TypeAlias(it) => {
                let it = cx.vsema.to_def(it)?;
                let kind = match (assoc, foreign) {
                    (true, _) => Kind::AssocTy,
                    (_, true) => Kind::ForeignTy,
                    _ => Kind::TyAlias,
                };
                (def(Id::Alias(TypeAliasId::from(it)), kind, Some(it.name(db))), None)
            }
            ast::Item::Impl(it) => {
                let id = ImplId::try_from(cx.vsema.to_def(it)?).ok()?;
                (def(Id::Impl(id), Kind::Impl { of_trait: it.trait_().is_some() }, None), None)
            }
            ast::Item::Use(_) => {
                let ptr = item.map(|it| ast::ptr(it.syntax()));
                (def(Id::Use(ptr), Kind::Use, None), None)
            }
            _ => return None,
        };
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
            self.fields_and_variants(item, index);
            self.derived(adt, parent);
            self.expanded_derives(item, parent);
        }
        Some(index)
    }

    /// The items the derives of `item` expand to, after it, where they are expanded (a build;
    /// the front end records the builtin ones of a check instead, [`Walk::derived`]): free items
    /// of its module. The impls of `Clone`'s and `Debug`'s are [`Defs::trivial_derives`].
    fn expanded_derives(&mut self, item: InFile<ast::Item<'db>>, parent: Parent) {
        let cx = self.cx;
        let db = cx.db;
        let Some(owner) = ast::AnyHasAttrs::cast(item.value.syntax()) else { return };
        let cfg = cx.krate.cfg(db);
        for (meta, _) in hir_expand::attrs::cfg_attrs_view(ast::attrs_including_inner(&owner), cfg)
        {
            let is_derive = matches!(meta, ast::Meta::TokenTreeMeta(it)
                if it.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == "derive"));
            if !is_derive {
                continue;
            }
            let Some(derived) = cx.vsema.expand_derive_macro(meta) else { continue };
            for root in derived.into_iter().flatten() {
                let file = cx.vsema.hir_file_for(root.value);
                let Some(call) = file.macro_file() else { continue };
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
                self.expansion(InFile::new(file, root.value), Container::Module, parent);
                let Some(name) = trivial else { continue };
                for at in first..self.defs.len() {
                    if self.defs[at].kind == (Kind::Impl { of_trait: true }) {
                        self.trivial_derives.insert(at as DefIdx, name);
                    }
                }
            }
        }
    }

    fn current(&mut self) -> &mut ModuleItems {
        self.modules.last_mut().expect("items are walked within a module")
    }

    fn fields_and_variants(&mut self, item: InFile<ast::Item<'db>>, adt: DefIdx) {
        let fields = match item.value {
            ast::Item::Struct(it) => it.field_list(),
            ast::Item::Union(it) => it.record_field_list().map(ast::FieldList::RecordFieldList),
            ast::Item::Enum(it) => {
                let db = self.cx.db;
                for variant in it.variant_list().into_iter().flat_map(|it| it.variants()) {
                    if !self.cx.cfg_enabled(variant.syntax()) {
                        continue;
                    }
                    let Some(id) = self.cx.vsema.to_def(variant) else { continue };
                    let index = self.push(Def {
                        id: Id::Variant(id.into()),
                        kind: Kind::Variant,
                        node: Some(item.with_value(ast::ptr(variant.syntax()))),
                        name: Some(id.name(db)),
                        parent: Parent::Def(adt),
                        foreign: false,
                    });
                    self.edges.push((adt, index));
                    self.fields(item.file_id, variant.field_list(), index);
                }
                return;
            }
            _ => return,
        };
        self.fields(item.file_id, fields, adt);
    }

    fn fields(&mut self, file: HirFileId, list: Option<ast::FieldList<'db>>, owner: DefIdx) {
        match list {
            Some(ast::FieldList::RecordFieldList(it)) => {
                for field in it.fields() {
                    if self.cx.cfg_enabled(field.syntax()) {
                        let def = self.cx.vsema.to_def(field);
                        self.field(file, field.syntax(), def, owner);
                    }
                }
            }
            Some(ast::FieldList::TupleFieldList(it)) => {
                for field in it.fields() {
                    if self.cx.cfg_enabled(field.syntax()) {
                        let def = self.cx.vsema.to_def(field);
                        self.field(file, field.syntax(), def, owner);
                    }
                }
            }
            None => {}
        }
    }

    fn field(
        &mut self,
        file: HirFileId,
        node: ast::Node<'_>,
        field: Option<hir::Field>,
        owner: DefIdx,
    ) {
        let Some(field) = field else { return };
        let index = self.push(Def {
            id: Id::Field(FieldId::from(field)),
            kind: Kind::Field,
            node: Some(InFile::new(file, ast::ptr(node))),
            name: Some(field.name(self.cx.db)),
            parent: Parent::Def(owner),
            foreign: false,
        });
        self.edges.push((owner, index));
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
                node: None,
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
                    node: None,
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

/// Whether `node` is an item of HIR's: an item, an associated item or an item of an `extern`
/// block, not a macro call (which is what it expands to); a `global_asm!` is one.
fn is_item(node: ast::Node<'_>) -> bool {
    match node.kind() {
        SyntaxKind::MACRO_CALL => false,
        SyntaxKind::ASM_EXPR => matches!(
            node.parent().map(|it| it.kind()),
            Some(SyntaxKind::SOURCE_FILE | SyntaxKind::ITEM_LIST | SyntaxKind::MACRO_ITEMS)
        ),
        kind => ast::Item::can_cast(kind),
    }
}

/// What a closure or an anonymous constant is known by: its node inside any parentheses, which
/// is the node the front end maps the expression back to.
fn anon_key<'t>(file: HirFileId, node: ast::Node<'t>) -> InFile<SyntaxNodePtr> {
    let inner = |node: ast::Node<'t>| {
        (node.kind() == SyntaxKind::PAREN_EXPR)
            .then(|| node.children().find(|it| ast::Expr::can_cast(it.kind())))
            .flatten()
    };
    let mut ptr = ast::ptr(node);
    let mut next = inner(node);
    while let Some(node) = next {
        ptr = ast::ptr(node);
        next = inner(node);
    }
    InFile::new(file, ptr)
}

/// Whether `node` is a definition of its own in rustc's HIR that items in it have as their
/// parent: a closure, an `async` or `gen` block (a coroutine), a `const` block, or an
/// anonymous constant (an array's length, a const argument, a discriminant).
pub(in super::super) fn is_anon(node: ast::Node<'_>) -> bool {
    match node.kind() {
        SyntaxKind::CLOSURE_EXPR => true,
        SyntaxKind::BLOCK_EXPR
            if node
                .children_with_tokens()
                .any(|it| matches!(it.kind(), T![const] | T![async] | T![gen])) =>
        {
            true
        }
        // A plain block is an expression like any other: an anonymous constant where it is an
        // array's length or a const argument (`[(); { fn f() {} 1 }]`), below.
        kind if !ast::Expr::can_cast(kind) => false,
        _ => match node.parent() {
            Some(parent) => match parent.kind() {
                SyntaxKind::CONST_ARG | SyntaxKind::VARIANT => true,
                SyntaxKind::ARRAY_EXPR => {
                    let has_semi =
                        parent.children_with_tokens().any(|it| it.kind() == SyntaxKind::SEMICOLON);
                    has_semi
                        && parent.children().filter(|it| ast::Expr::can_cast(it.kind())).nth(1)
                            == Some(node)
                }
                _ => false,
            },
            None => false,
        },
    }
}
