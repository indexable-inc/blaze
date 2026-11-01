//! What rustc names a definition by: its definitions and their parents (`def_key`), the
//! children a module, enum or trait shows (`module_children`), the names it trims paths to
//! (`trimmed_def_paths`), the parents it reaches a dependency's items through
//! (`visible_parent_map`), and every declaration by its name (what `lookup_import_candidates`
//! walks the crates for). Ported from `compiler/rustc_middle/src/ty/print/pretty.rs` and
//! `compiler/rustc_metadata/src/rmeta/decoder/cstore_impl.rs` (MIT or Apache-2.0).
//!
//! The tables are the check's: [`Paths`] lives in [`Cx`] and keeps them in its frame, each built
//! the first time it is read, and every later answer is a hash probe and a slice.

use std::cell::OnceCell;

use base_db::{CrateOrigin, LangCrateOrigin};
use blaze_hash::FixedState;
use blaze_mem::Frame;
use hashbrown::hash_map::Entry;
use hir_def::{
    AdtId, AssocItemId, AttrDefId, ConstId, DefWithBodyId, EnumVariantId, ExternCrateId,
    FunctionId, HasModule, ImplId, ItemContainerId, Lookup, MacroId, ModuleDefId, ModuleId,
    StaticId, TraitId, TypeAliasId,
    attrs::AttrFlags,
    expr_store::Body,
    item_scope::{ImportOrExternCrate, ImportOrGlob, ItemScope},
    nameres::crate_def_map,
    signatures::FunctionSignature,
    visibility::Visibility,
};
use hir_expand::name::Name;
use intern::{Symbol, sym};
use syntax::{
    SyntaxKind, WalkEvent,
    view::{self as ast, AstNode as _},
};

use crate::cx::Cx;

/// A hash map in the check's frame.
type Map<'f, K, V> = hashbrown::HashMap<K, V, FixedState, &'f Frame>;
type Set<'f, K> = hashbrown::HashSet<K, FixedState, &'f Frame>;
/// A column in the check's frame.
type Column<'f, T> = Vec<T, &'f Frame>;

fn map<K, V>(frame: &Frame) -> Map<'_, K, V> {
    hashbrown::HashMap::with_hasher_in(FixedState, frame)
}

/// A definition a path names: rustc's `DefId`, of the kinds diagnostics print.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Def {
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

impl Def {
    /// None for a primitive type, which has no definition.
    pub(crate) fn of(def: ModuleDefId) -> Option<Def> {
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

    fn of_assoc(item: AssocItemId) -> Def {
        match item {
            AssocItemId::FunctionId(it) => Def::Function(it),
            AssocItemId::ConstId(it) => Def::Const(it),
            AssocItemId::TypeAliasId(it) => Def::TypeAlias(it),
        }
    }

    fn attr_owner(self) -> AttrDefId {
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
    pub(crate) fn module(self, cx: &Cx<'_, '_>) -> ModuleId {
        self.attr_owner().module(cx.db)
    }

    pub(crate) fn krate(self, cx: &Cx<'_, '_>) -> base_db::Crate {
        self.module(cx).krate(cx.db)
    }

    pub(crate) fn is_local(self, cx: &Cx<'_, '_>) -> bool {
        hir::Crate::from(self.krate(cx)) == cx.krate
    }

    pub(crate) fn is_doc_hidden(self, cx: &Cx<'_, '_>) -> bool {
        AttrFlags::query(cx.db, self.attr_owner()).contains(AttrFlags::IS_DOC_HIDDEN)
    }

    /// rustc's `DefKind::ns`: None for an impl.
    fn ns(self) -> Option<Ns> {
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
    fn is_module_like(self) -> bool {
        matches!(self, Def::Module(_) | Def::Adt(AdtId::EnumId(_)) | Def::Trait(_))
    }

    /// Whether it is in value namespace: what `def_path_str` prints with `::<..>`.
    pub(crate) fn is_value(self) -> bool {
        self.ns() == Some(Ns::Value)
    }

    /// Whether its path segment is rustc's `DefPathData::TypeNs`, the segments a re-export
    /// can rename.
    pub(crate) fn is_type_ns(self) -> bool {
        self.ns() == Some(Ns::Type)
    }

    /// The name of the definition's path segment; None for a crate root and an impl.
    pub(crate) fn name(self, cx: &Cx<'_, '_>) -> Option<Name> {
        let db = cx.db;
        match self {
            Def::Module(it) => it.name(db),
            Def::Impl(_) => None,
            Def::ExternCrate(it) => hir::ExternCrateDecl::from(it).alias_or_name(db),
            Def::Variant(it) => Some(it.lookup(db).name),
            Def::Macro(it) => Some(hir::Macro::from(it).name(db)),
            Def::Adt(it) => Some(hir::Adt::from(it).name(db)),
            Def::Function(it) => Some(FunctionSignature::of(db, it).name),
            Def::Const(it) => hir::Const::from(it).name(db),
            Def::Static(it) => Some(hir::Static::from(it).name(db)),
            Def::Trait(it) => Some(hir::Trait::from(it).name(db)),
            Def::TypeAlias(it) => Some(hir::TypeAlias::from(it).name(db)),
        }
    }

    /// The container of an associated item, of a function or constant written in a trait or
    /// an impl.
    pub(crate) fn container(self, cx: &Cx<'_, '_>) -> Option<ItemContainerId> {
        let db = cx.db;
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
    fn is_foreign(self, cx: &Cx<'_, '_>) -> bool {
        matches!(self.container(cx), Some(ItemContainerId::ExternBlockId(_)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Ns {
    Type,
    Value,
    Macro,
}

/// What a definition's path is printed under: rustc's `def_key(def).parent`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Parent {
    Def(Def),
    /// An item written in a body: the body's owner, then the closures it is written in,
    /// outermost first, each with its `{closure#N}` disambiguator.
    Body { owner: Def, closures: Vec<u32> },
}

/// The definition's parent; None for a crate root.
pub(crate) fn parent(cx: &Cx<'_, '_>, def: Def) -> Option<Parent> {
    let db = cx.db;
    let module_parent = |module: ModuleId| {
        if module.is_block_module(db) { block_owner(cx, module) } else { Some(Parent::Def(Def::Module(module))) }
    };
    match def {
        Def::Module(it) => module_parent(it.containing_module(db)?.to_static()),
        Def::Variant(it) => Some(Parent::Def(Def::Adt(it.lookup(db).parent.into()))),
        Def::Function(_) | Def::Const(_) | Def::Static(_) | Def::TypeAlias(_) => {
            match def.container(cx)? {
                ItemContainerId::ModuleId(it) => module_parent(it),
                ItemContainerId::ExternBlockId(it) => module_parent(it.lookup(db).container),
                ItemContainerId::ImplId(it) => Some(Parent::Def(Def::Impl(it))),
                ItemContainerId::TraitId(it) => Some(Parent::Def(Def::Trait(it))),
            }
        }
        Def::Adt(_) | Def::Trait(_) | Def::Macro(_) | Def::Impl(_) | Def::ExternCrate(_) => {
            module_parent(def.module(cx))
        }
    }
}

/// The body a block module is written in, found from the syntax of its block: rustc's
/// definitions have no blocks, an item in a body is a child of the body's owner, or of the
/// closure it is written in.
fn block_owner(cx: &Cx<'_, '_>, module: ModuleId) -> Option<Parent> {
    let db = cx.db;
    let block = module.block(db)?;
    let ast_id = block.ast_id(db);
    let ptr = ast_id.file_id.ast_id_map(db).get(ast_id.value);
    body_parent(cx, cx.node_view(ast_id.with_value(ptr)).value.syntax())
}

/// The path of a closure, an `async` block or a `gen` block `node` (of a tree `cx.vsema`
/// gave out): its body's owner and the closures down to it, itself last.
pub(crate) fn closure_path<'db>(cx: &Cx<'_, 'db>, node: ast::Node<'db>) -> Option<Parent> {
    body_parent(cx, node)
}

/// The body's owner `node` is written in and the closures around it, `node` included when it
/// is one.
fn body_parent<'db>(cx: &Cx<'_, 'db>, node: ast::Node<'db>) -> Option<Parent> {
    let db = cx.db;
    // alloc: printing a closure's path, the edge
    let mut closures = Vec::new();
    for ancestor in cx.vsema.ancestors_with_macros(node) {
        if is_closure_like(ancestor) {
            closures.push(ancestor);
            continue;
        }
        let owner = match ancestor.kind() {
            SyntaxKind::FN => Def::Function(FunctionId::try_from(cx.vsema.to_def(ast::Fn::cast(ancestor)?)?).ok()?),
            SyntaxKind::CONST => Def::Const(cx.vsema.to_def(ast::Const::cast(ancestor)?)?.into()),
            SyntaxKind::STATIC => Def::Static(cx.vsema.to_def(ast::Static::cast(ancestor)?)?.into()),
            _ => continue,
        };
        // alloc: the closures of the path printed, returned in it
        let mut indices = Vec::with_capacity(closures.len() + 1);
        if let Def::Function(f) = owner {
            let sig = FunctionSignature::of(db, f);
            if sig.is_async() || sig.is_gen() {
                indices.push(0);
            }
        }
        let mut parent = ancestor;
        for closure in closures.into_iter().rev() {
            indices.push(closure_index(parent, closure));
            parent = closure;
        }
        return Some(Parent::Body { owner, closures: indices });
    }
    None
}

/// Whether rustc makes a closure definition of `node`: a closure, an `async` or `gen` block.
pub(crate) fn is_closure_like(node: ast::Node<'_>) -> bool {
    match node.kind() {
        SyntaxKind::CLOSURE_EXPR => true,
        SyntaxKind::BLOCK_EXPR => ast::BlockExpr::cast(node).is_some_and(|it| {
            it.async_token().is_some() || it.gen_token().is_some()
        }),
        _ => false,
    }
}

/// Whether `node` is a definition closures are numbered under: an item, or a closure.
fn is_closure_parent(node: ast::Node<'_>) -> bool {
    ast::Item::can_cast(node.kind()) || is_closure_like(node)
}

/// The disambiguator rustc gives `closure` among the closures of `parent`: closures are
/// numbered as they are reached, outer before inner, each under the nearest item or closure
/// around it. One walk of `parent` up to `closure`, not entering another item or closure:
/// what is inside one is numbered under it.
pub(crate) fn closure_index(parent: ast::Node<'_>, closure: ast::Node<'_>) -> u32 {
    let mut index = 0;
    let mut walk = parent.preorder();
    walk.next();
    while let Some(event) = walk.next() {
        let WalkEvent::Enter(node) = event else { continue };
        if node == closure {
            break;
        }
        if is_closure_parent(node) {
            index += u32::from(is_closure_like(node));
            walk.skip_subtree();
        }
    }
    index
}

/// A public child `module_children` lists, with what the walks over children test of it.
/// 16 bytes or so: a name, a definition and two flags.
#[derive(Clone, Copy)]
struct Child {
    name: Name,
    def: Def,
    /// Whether `trimmed_def_paths` counts it: neither `#[doc(hidden)]` nor a type alias
    /// outside an `extern` block.
    trim: bool,
    /// Whether the `use` or `extern crate` it is re-exported by is `#[doc(hidden)]`.
    hidden_reexport: bool,
}

/// The public children of `def`, appended to `out` in `module_children`'s order: a module's
/// types, values then macros, an enum's variants, a trait's items.
fn list_children(cx: &Cx<'_, '_>, def: Def, out: &mut Column<'_, Child>) {
    let db = cx.db;
    let child = |name: &Name, def: Def, hidden_reexport: bool| Child {
        name: *name,
        def,
        trim: !def.is_doc_hidden(cx)
            && !matches!(def, Def::TypeAlias(it) if !matches!(it.lookup(db).container, ItemContainerId::ExternBlockId(_))),
        hidden_reexport,
    };
    match def {
        Def::Module(module) => {
            let scope = module.def_map(db).module(db, module).scope(db);
            scope_children(cx, scope, out, child);
        }
        Def::Adt(AdtId::EnumId(it)) => {
            out.extend(it.enum_variants(db).variants.iter().map(|(name, &(id, _))| child(name, Def::Variant(id), false)));
        }
        Def::Trait(it) => {
            out.extend(it.trait_items(db).items.iter().map(|(name, item)| child(name, Def::of_assoc(*item), false)));
        }
        _ => {}
    }
}

fn scope_children(
    cx: &Cx<'_, '_>,
    scope: &ItemScope,
    out: &mut Column<'_, Child>,
    child: impl Fn(&Name, Def, bool) -> Child,
) {
    let hidden = |owner: AttrDefId| AttrFlags::query(cx.db, owner).contains(AttrFlags::IS_DOC_HIDDEN);
    let reexport = |import: Option<ImportOrExternCrate>| match import {
        Some(ImportOrExternCrate::Import(it)) => hidden(AttrDefId::UseId(it.use_)),
        Some(ImportOrExternCrate::Glob(it)) => hidden(AttrDefId::UseId(it.use_)),
        Some(ImportOrExternCrate::ExternCrate(it)) => hidden(AttrDefId::ExternCrateId(it)),
        None => false,
    };
    for (name, item) in scope.types() {
        if item.vis == Visibility::Public
            && let Some(def) = Def::of(item.def)
        {
            out.push(child(name, def, reexport(item.import)));
        }
    }
    for (name, item) in scope.values() {
        if item.vis == Visibility::Public
            && let Some(def) = Def::of(item.def)
        {
            let import = item.import.map(|it| match it {
                ImportOrGlob::Import(it) => ImportOrExternCrate::Import(it),
                ImportOrGlob::Glob(it) => ImportOrExternCrate::Glob(it),
            });
            out.push(child(name, def, reexport(import)));
        }
    }
    for (name, item) in scope.macros() {
        if item.vis == Visibility::Public {
            out.push(child(name, Def::Macro(item.def), reexport(item.import)));
        }
    }
}

/// The public children of every module, enum and trait of the direct dependencies reached
/// through public children from their roots, listed once: both walks of printing's tables and
/// [`Paths::reexport_name`] read them here. Node `i` is `defs[i]`, its children
/// `rows[starts[i]..starts[i + 1]]`; the roots are the first `roots` nodes.
struct Tree<'f> {
    node: Map<'f, Def, u32>,
    defs: Column<'f, Def>,
    starts: Column<'f, u32>,
    rows: Column<'f, Child>,
    roots: usize,
}

impl<'f> Tree<'f> {
    fn new(cx: &Cx<'_, '_>, frame: &'f Frame, direct: &[hir::Crate]) -> Tree<'f> {
        let mut tree = Tree {
            node: map(frame),
            defs: Vec::new_in(frame),
            starts: Vec::new_in(frame),
            rows: Vec::new_in(frame),
            roots: 0,
        };
        for krate in direct {
            tree.add(Def::Module(krate.root_module(cx.db).into()));
        }
        tree.roots = tree.defs.len();
        let mut next = 0;
        while next < tree.defs.len() {
            let parent = tree.defs[next];
            let from = tree.rows.len();
            tree.starts.push(from as u32);
            list_children(cx, parent, &mut tree.rows);
            for at in from..tree.rows.len() {
                let def = tree.rows[at].def;
                if def.is_module_like() {
                    tree.add(def);
                }
            }
            next += 1;
        }
        tree.starts.push(tree.rows.len() as u32);
        tree
    }

    fn add(&mut self, def: Def) {
        if let Entry::Vacant(it) = self.node.entry(def) {
            it.insert(self.defs.len() as u32);
            self.defs.push(def);
        }
    }

    fn index(&self, def: Def) -> usize {
        *self.node.get(&def).expect("every module-like public child of a node is a node") as usize
    }

    fn children_at(&self, node: usize) -> &[Child] {
        &self.rows[self.starts[node] as usize..self.starts[node + 1] as usize]
    }
}

/// The names of the crate and its direct dependencies that diagnostics print and suggest.
/// Each table is built in the check's frame the first time it is read:
/// - printing's (`trimmed_def_paths`, `visible_parent_map`, the crate's `extern crate` items),
///   the first time a path is printed;
/// - [`Decls`], every declaration by its name, the first time an import is looked for.
pub(crate) struct Paths<'f> {
    frame: &'f Frame,
    printing: OnceCell<Printing<'f>>,
    decls: OnceCell<Decls<'f>>,
}

/// Printing's tables: one hash probe per answer.
struct Printing<'f> {
    trimmed: Map<'f, Def, Name>,
    visible_parent: Map<'f, Def, Def>,
    /// Each dependency the crate names by an `extern crate` item written in it.
    extern_crates: Map<'f, base_db::Crate, ExternCrateId>,
    tree: Tree<'f>,
}

impl<'f> Paths<'f> {
    pub(crate) fn new(cx: &Cx<'f, '_>) -> Paths<'f> {
        Paths { frame: cx.frame(), printing: OnceCell::new(), decls: OnceCell::new() }
    }

    fn printing(&self, cx: &Cx<'_, '_>) -> &Printing<'f> {
        self.printing.get_or_init(|| Printing::new(cx, self.frame))
    }

    /// The one name `def` is known by across the crate and its direct dependencies.
    pub(crate) fn trimmed(&self, cx: &Cx<'_, '_>, def: Def) -> Option<Name> {
        self.printing(cx).trimmed.get(&def).copied()
    }

    pub(crate) fn visible_parent(&self, cx: &Cx<'_, '_>, def: Def) -> Option<Def> {
        self.printing(cx).visible_parent.get(&def).copied()
    }

    /// The `extern crate` item the crate names `krate` by, when it writes one.
    pub(crate) fn extern_crate(&self, cx: &Cx<'_, '_>, krate: base_db::Crate) -> Option<ExternCrateId> {
        self.printing(cx).extern_crates.get(&krate).copied()
    }

    /// The name of the public child of `parent` that is `def`, other than `_`: what a
    /// re-export calls it. `parent` is a visible parent, so its children are listed.
    pub(crate) fn reexport_name(&self, cx: &Cx<'_, '_>, parent: Def, def: Def) -> Option<Name> {
        let tree = &self.printing(cx).tree;
        tree.children_at(tree.index(parent))
            .iter()
            .find(|it| it.def == def && *it.name.symbol() != sym::underscore)
            .map(|it| it.name)
    }

    /// Every declaration of the crate and its direct dependencies, by name.
    pub(crate) fn decls(&self, cx: &Cx<'_, '_>) -> &Decls<'f> {
        self.decls.get_or_init(|| Decls::new(cx, self.frame))
    }
}

impl<'f> Printing<'f> {
    fn new(cx: &Cx<'_, '_>, frame: &'f Frame) -> Printing<'f> {
        let mut unique: Map<'f, (Ns, Name), Option<Def>> = map(frame);
        let mut extern_crates = map(frame);
        local_names(cx, frame, &mut unique, &mut extern_crates);
        let direct = direct_crates(cx, frame, &extern_crates);
        let tree = Tree::new(cx, frame, &direct);
        trimmed_names(frame, &tree, &mut unique);
        let mut trimmed: Map<'f, Def, Name> =
            hashbrown::HashMap::with_capacity_and_hasher_in(unique.len(), FixedState, frame);
        for ((_, name), def) in unique {
            let Some(def) = def else { continue };
            match trimmed.entry(def) {
                Entry::Occupied(mut it) => {
                    if it.get().symbol().cmp_order(name.symbol()).is_gt() {
                        *it.get_mut() = name;
                    }
                }
                Entry::Vacant(it) => {
                    it.insert(name);
                }
            }
        }
        let visible_parent = visible_parent_map(cx, frame, &tree);
        Printing { trimmed, visible_parent, extern_crates, tree }
    }
}

/// `trimmed_def_paths`' count of a name in a namespace: the one definition it names, or none
/// once it names two.
fn collect(unique: &mut Map<'_, (Ns, Name), Option<Def>>, name: Name, ns: Ns, def: Def) {
    match unique.entry((ns, name)) {
        Entry::Occupied(mut it) => {
            if it.get().is_some_and(|existing| existing != def) {
                *it.get_mut() = None;
            }
        }
        Entry::Vacant(it) => {
            it.insert(Some(def));
        }
    }
}

/// The crate's own named definitions, wherever they are written (rustc's `hir_free_items`),
/// counted into `unique`; the names its glob imports bring in, which name nothing (rustc's
/// `glob_map` has the names used through a glob import, a name any local glob brings in counts
/// here until the front end records which are used); and its `extern crate` items.
fn local_names<'f>(
    cx: &Cx<'_, '_>,
    frame: &'f Frame,
    unique: &mut Map<'f, (Ns, Name), Option<Def>>,
    extern_crates: &mut Map<'f, base_db::Crate, ExternCrateId>,
) {
    let db = cx.db;
    let mut bodies: Column<'f, DefWithBodyId> = Vec::new_in(frame);
    let mut seen_bodies: Set<'f, DefWithBodyId> = hashbrown::HashSet::with_hasher_in(FixedState, frame);
    let def_map = crate_def_map(db, cx.krate.into());
    let mut scopes: Column<'f, &ItemScope> = Vec::new_in(frame);
    scopes.extend(def_map.modules(db).map(|(_, data)| data.scope(db)));
    fn glob(unique: &mut Map<'_, (Ns, Name), Option<Def>>, name: &Name) {
        for ns in [Ns::Type, Ns::Value, Ns::Macro] {
            unique.insert((ns, *name), None);
        }
    }
    loop {
        for scope in scopes.drain(..) {
            for (name, item) in scope.types() {
                if matches!(item.import, Some(ImportOrExternCrate::Glob(_))) {
                    glob(unique, name);
                }
            }
            for (name, item) in scope.values() {
                if matches!(item.import, Some(ImportOrGlob::Glob(_))) {
                    glob(unique, name);
                }
            }
            for (name, item) in scope.macros() {
                if matches!(item.import, Some(ImportOrExternCrate::Glob(_))) {
                    glob(unique, name);
                }
            }
            for id in scope.extern_crate_decls() {
                let decl = hir::ExternCrateDecl::from(id);
                if let Some(name) = decl.alias_or_name(db) {
                    collect(unique, name, Ns::Type, Def::ExternCrate(id));
                }
                if let Some(krate) = decl.resolved_crate(db) {
                    extern_crates.entry(krate.into()).or_insert(id);
                }
            }
            for id in scope.declarations() {
                let Some(def) = Def::of(id) else { continue };
                if !def.is_foreign(cx)
                    && let Some(name) = def.name(cx)
                {
                    collect(unique, name, def.ns().unwrap_or(Ns::Type), def);
                }
                match id {
                    ModuleDefId::FunctionId(it) => bodies.push(it.into()),
                    ModuleDefId::ConstId(it) => bodies.push(it.into()),
                    ModuleDefId::StaticId(it) => bodies.push(it.into()),
                    ModuleDefId::TraitId(it) => {
                        bodies.extend(it.trait_items(db).items.iter().filter_map(|(_, it)| body_of(*it)));
                    }
                    _ => {}
                }
            }
            for id in scope.impls() {
                bodies.extend(id.impl_items(db).items.iter().filter_map(|(_, it)| body_of(*it)));
            }
        }
        let Some(body) = bodies.pop() else { break };
        if !seen_bodies.insert(body) {
            continue;
        }
        for (_, block) in Body::of(db, body).blocks(db) {
            scopes.extend(block.modules(db).map(|(_, data)| data.scope(db)));
        }
    }
}

fn body_of(item: AssocItemId) -> Option<DefWithBodyId> {
    match item {
        AssocItemId::FunctionId(it) => Some(it.into()),
        AssocItemId::ConstId(it) => Some(it.into()),
        AssocItemId::TypeAliasId(_) => None,
    }
}

/// The crates rustc counts as the crate's direct dependencies, the ones it has loaded for
/// the crate itself, `std` first: `std` (or `core` when there is no `std`), the crates of
/// the crate's `extern crate` items, and the dependencies that are not the language's.
fn direct_crates<'f>(
    cx: &Cx<'_, '_>,
    frame: &'f Frame,
    extern_crates: &Map<'_, base_db::Crate, ExternCrateId>,
) -> Column<'f, hir::Crate> {
    let db = cx.db;
    // alloc: hir's list of the crate's dependencies, once per check
    let deps = cx.krate.dependencies(db);
    let lang = |krate: hir::Crate| match krate.origin(db) {
        CrateOrigin::Lang(it) => Some(it),
        _ => None,
    };
    let injected = deps
        .iter()
        .map(|it| it.krate)
        .find(|&it| lang(it) == Some(LangCrateOrigin::Std))
        .or_else(|| deps.iter().map(|it| it.krate).find(|&it| lang(it) == Some(LangCrateOrigin::Core)));
    let mut out: Column<'f, hir::Crate> = Vec::with_capacity_in(deps.len(), frame);
    out.extend(injected);
    for krate in deps.iter().map(|it| it.krate) {
        let named = extern_crates.contains_key(&base_db::Crate::from(krate));
        if !out.contains(&krate) && (lang(krate).is_none() || named) {
            out.push(krate);
        }
    }
    out
}

/// rustc's `trimmed_def_paths` over the direct dependencies: the public children reached from
/// their roots, except `#[doc(hidden)]` ones and type aliases outside `extern` blocks (nor what
/// is below them), counted into `unique` with the crate's own.
fn trimmed_names(frame: &Frame, tree: &Tree<'_>, unique: &mut Map<'_, (Ns, Name), Option<Def>>) {
    let nodes = tree.defs.len();
    let mut seen: Column<'_, u64> = Vec::with_capacity_in(nodes.div_ceil(64), frame);
    seen.resize(nodes.div_ceil(64), 0);
    let mut stack: Column<'_, usize> = Vec::with_capacity_in(nodes, frame);
    for root in 0..tree.roots {
        seen[root / 64] |= 1 << (root % 64);
        stack.push(root);
    }
    while let Some(node) = stack.pop() {
        for child in tree.children_at(node) {
            if !child.trim {
                continue;
            }
            if let Some(ns) = child.def.ns() {
                collect(unique, child.name, ns, child.def);
            }
            if child.def.is_module_like() {
                let at = tree.index(child.def);
                let bit = 1 << (at % 64);
                if seen[at / 64] & bit == 0 {
                    seen[at / 64] |= bit;
                    stack.push(at);
                }
            }
        }
    }
}

/// rustc's `visible_parent_map`: for each definition of a direct dependency reached through
/// public children, breadth first over all of them so that the shortest path wins, the
/// parent it is reached through; parents through `_`, a `#[doc(hidden)]` parent or a
/// `#[doc(hidden)]` re-export only where there is no other.
fn visible_parent_map<'f>(cx: &Cx<'_, '_>, frame: &'f Frame, tree: &Tree<'_>) -> Map<'f, Def, Def> {
    let mut map: Map<'f, Def, Def> =
        hashbrown::HashMap::with_capacity_and_hasher_in(tree.rows.len(), FixedState, frame);
    let mut fallback: Column<'_, (Def, Def)> = Vec::new_in(frame);
    let mut queue: Column<'_, usize> = Vec::with_capacity_in(tree.defs.len(), frame);
    queue.extend(0..tree.roots);
    let mut head = 0;
    while let Some(&node) = queue.get(head) {
        head += 1;
        let parent = tree.defs[node];
        let parent_hidden = parent.is_doc_hidden(cx);
        for child in tree.children_at(node) {
            if *child.name.symbol() == sym::underscore || parent_hidden || child.hidden_reexport {
                fallback.push((child.def, parent));
                continue;
            }
            if let Entry::Vacant(entry) = map.entry(child.def) {
                entry.insert(parent);
                if child.def.is_module_like() {
                    queue.push(tree.index(child.def));
                }
            }
        }
    }
    for (child, parent) in fallback {
        map.entry(child).or_insert(parent);
    }
    map
}

/// A declaration of the crate or of a direct dependency: what an import is suggested from.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Decl {
    pub(crate) def: hir::ModuleDef,
    /// Whether it is the crate's own.
    pub(crate) local: bool,
}

/// Every named declaration of the crate and its direct dependencies, the crate's first, each
/// module's in order and an enum's variants after it, grouped by name: name `n`'s are
/// `decls[starts[n]..starts[n + 1]]` in that order. rustc's `lookup_import_candidates` walks
/// every module of every crate for each name it looks up; here the walk is done once per check
/// and a lookup is one probe of `at` and a slice.
pub(crate) struct Decls<'f> {
    at: Map<'f, Symbol, u32>,
    starts: Column<'f, u32>,
    decls: Column<'f, Decl>,
}

impl<'f> Decls<'f> {
    fn new(cx: &Cx<'_, '_>, frame: &'f Frame) -> Decls<'f> {
        let db = cx.db;
        let mut at: Map<'f, Symbol, u32> = map(frame);
        let mut pairs: Column<'f, (u32, Decl)> = Vec::new_in(frame);
        let mut push = |name: &Name, def: hir::ModuleDef, local: bool| {
            let next = at.len() as u32;
            let row = *at.entry(*name.symbol()).or_insert(next);
            pairs.push((row, Decl { def, local }));
        };
        for krate in std::iter::once(cx.krate).chain(cx.krate.dependencies_iter(db).map(|it| it.krate)) {
            let local = krate == cx.krate;
            for (_, data) in crate_def_map(db, krate.into()).modules(db) {
                for id in data.scope(db).declarations() {
                    let def = hir::ModuleDef::from(id);
                    if let Some(name) = def.name(db) {
                        push(&name, def, local);
                    }
                    if let ModuleDefId::AdtId(AdtId::EnumId(it)) = id {
                        for (name, &(variant, _)) in it.enum_variants(db).variants.iter() {
                            push(name, hir::ModuleDef::EnumVariant(variant.into()), local);
                        }
                    }
                }
            }
        }
        let rows = at.len();
        let mut starts: Column<'f, u32> = Vec::with_capacity_in(rows + 1, frame);
        starts.resize(rows + 1, 0);
        for &(row, _) in &pairs {
            starts[row as usize + 1] += 1;
        }
        for row in 1..=rows {
            starts[row] += starts[row - 1];
        }
        let mut cursors: Column<'_, u32> = Vec::with_capacity_in(rows, frame);
        cursors.extend_from_slice(&starts[..rows]);
        let mut decls: Column<'f, Decl> = Vec::with_capacity_in(pairs.len(), frame);
        if let Some(&(_, first)) = pairs.first() {
            decls.resize(pairs.len(), first);
        }
        for &(row, decl) in &pairs {
            let cursor = &mut cursors[row as usize];
            decls[*cursor as usize] = decl;
            *cursor += 1;
        }
        Decls { at, starts, decls }
    }

    /// The declarations named `name`, in the order above. A name no declaration has may not
    /// be interned at all: then it is not looked for.
    pub(crate) fn named(&self, name: &str) -> &[Decl] {
        let Some(row) = Symbol::find(name).and_then(|it| self.at.get(&it)) else { return &[] };
        let row = *row as usize;
        &self.decls[self.starts[row] as usize..self.starts[row + 1] as usize]
    }
}
