//! Def-site hygiene of macro expansions (macros 2.0, `decl_macro`, and the def-site spans of
//! builtin and proc macros): where the first segment of a path an expansion writes is looked up.
//!
//! A port of rustc's rule (`rustc_resolve`'s `hygienic_lexical_parent` and `expn_def_scope`, with
//! bindings keyed by `normalize_to_macros_2_0`). A path whose first segment carries an opaque
//! mark of an expansion `E` (its context normalized to macros 2.0 has `E` as its outer
//! expansion) sees, in the blocks and the module around it, only the bindings whose names carry
//! that same context: the names `E` wrote itself. When none of them has the name and that module
//! is not one `E` wrote, the lookup goes on at the module that defines `E`'s macro, with the
//! context outside `E`, and so on for each opaque mark. `self`, `super` and `crate` mean the def
//! site's module the same way. A `macro_rules!` marks its expansion semi-opaque, which
//! normalizing drops, so its paths resolve where it is called, as before. A builtin macro's
//! def site is where core declares it; a proc macro's is its crate's root, as rustc's
//! `macro_def_scope`.
//!
//! A binding is keyed by its name and the context that name was written with, normalized to
//! macros 2.0 (rustc's `BindingKey`): [`Name::keyed`], which leaves every name a real file or a
//! `macro_rules!` writes as it is. The context is read from the span map of the file the item's
//! name, or a `use`'s last segment or rename, is written in, so a name `E` was handed by its
//! caller (`$name`) keeps the caller's context and is not `E`'s. Two expansions of one `macro`
//! that each define `static DLSYM` in one block (std's `weak!` on macOS) bind two keys, and each
//! expansion's `DLSYM` finds its own. A glob import carries the keys of the names it imports.
//! The keys are given here ([`def_binding_name`], [`import_binding_key`]) and looked up here
//! ([`DefMap::resolve_path_from`]): the preludes and builtin types take the name unkeyed.
//!
//! The same key names a `mod` ([`module_binding_name`]), an `extern crate` in the module and the
//! extern prelude ([`extern_crate_binding_name`]), and a `macro_rules!` or `macro` in its textual
//! scope ([`macro_def_name`]), as rustc's `IdentKey` does. Every later segment of a path is looked
//! up with the context it was written with, adjusted to the expansion of the module it is looked
//! up in ([`key_in`], rustc's `normalize_to_macros_2_0_and_adjust`); the contexts are recorded on
//! the [`ModPath`] only for paths written in a macro file. A glob imports a name under
//! [`reverse_glob_adjust`] of its key, and skips the names it cannot name
//! ([`glob_imported_name`]).
//!
//! A field's name is keyed the same way where the ADT is lowered (`VariantFields`), so a struct a
//! `macro` writes may have fields `a` and `$a`; a field access, a record literal's or pattern's
//! field and a method call keep the context their name is written with (the expression store's
//! `member_hygiene` and `record_field_hygiene`), and are looked up by [`name_in`] the scope the
//! ADT's or the impl's expansion made, as rustc's `adjust_ident_and_get_scope` and
//! `find_by_ident_and_namespace` do. An inherent impl's items are compared by
//! [`assoc_item_is`].
//!
//! Generic parameters are keyed too, in their item's generics ([`param_name`]): they always
//! have macros 2.0 hygiene, and a path or lifetime finds the parameter written with its own
//! context normalized so.
//!
//! Not hygienic yet: the first segment of a `use` path (it does not go through
//! [`DefMap::hygienic_origin`], so a `use super::*` a `macro` writes in a child module of its
//! call site reaches that module's parent, not rustc's `resolve_self` module), enum variants and
//! associated items looked up through a path (`T::f`), the path of a `pub(in path)` visibility,
//! positional fields (rustc's own FIXME), a trait's items, and the items of a trait impl as
//! matched to its trait's.

use base_db::{Crate, Db};
use hir_expand::{
    AstId, HirFileId, MacroCallId, MacroDefKind,
    mod_path::{ModPath, PathKind},
    name::Name,
};
use intern::sym;
use la_arena::Idx;
use span::{AstIdNode, ErasedFileAstId, SyntaxContext, TextRange};
use syntax::{
    ast,
    view::{self, AstNode as _},
};

use crate::{
    AstIdLoc, Lookup, MacroId, ModuleDefId, ModuleId, UseId,
    expr_store::HygieneId,
    item_scope::{BuiltinShadowMode, ImportId},
    src::use_trees_view,
    nameres::{
        BlockInfo, DefMap, LocalDefMap, MacroSubNs, ModuleOrigin, crate_def_map,
        crate_local_def_map, path_resolution::adjust_to_nearest_non_block_module,
    },
    per_ns::{Namespace, PerNs},
};

/// Where a path's first segment is looked up, after def-site hygiene.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hygienic {
    /// Where the path is written: its context carries no opaque mark of a `macro`, or the
    /// scope it is written in is one the `macro` wrote.
    Here,
    /// From this module: a block or module around the path that has a name the expansion
    /// wrote, or the module defining the `macro` (followed through each opaque mark). The
    /// context, normalized to macros 2.0, is the one the first segment is keyed by there
    /// ([`Name::keyed`]): the expansion's for a name it wrote, the context outside each mark
    /// followed.
    At(ModuleId, SyntaxContext),
    /// The module defining the `macro` is not known yet: its own crate's def map is being
    /// collected, and the macro is in no module around the path. The path is unresolved.
    Unknown,
}

/// One step of [`DefMap::hygienic_step`].
enum Step {
    Here,
    At(ModuleId),
    DefSite { module: ModuleId, ctxt: SyntaxContext },
    Unknown,
}

impl DefMap {
    /// Where the first segment of `path`, written in `module` of this def map with syntax
    /// context `ctxt`, is looked up: see the module docs. `ns` is the namespace the first
    /// segment is looked up in (`None`: any).
    pub(crate) fn hygienic_origin(
        &self,
        db: &Db,
        module: ModuleId,
        path: &ModPath,
        ns: Option<Namespace>,
        ctxt: SyntaxContext,
    ) -> Hygienic {
        let mut module = module;
        let mut ctxt = ctxt;
        let mut moved = false;
        // Each turn either ends or follows one opaque mark out to its def site with the context
        // outside it (`parent`), so it ends after as many turns as `ctxt` has opaque marks.
        loop {
            let map = self.map_for(db, module);
            match map.hygienic_step(db, module, path, ns, ctxt) {
                Step::Here if moved => {
                    return Hygienic::At(module, ctxt.normalize_to_macros_2_0(db));
                }
                Step::Here => return Hygienic::Here,
                Step::At(it) => return Hygienic::At(it, ctxt.normalize_to_macros_2_0(db)),
                Step::Unknown => return Hygienic::Unknown,
                Step::DefSite { module: def_site, ctxt: outside } => {
                    module = def_site;
                    ctxt = outside;
                    moved = true;
                }
            }
        }
    }

    /// [`DefMap::resolve_path`] from `origin`, what [`DefMap::hygienic_origin`] gave for the
    /// path written in `module`.
    pub(crate) fn resolve_path_from(
        &self,
        local_def_map: &LocalDefMap,
        db: &Db,
        module: ModuleId,
        path: &ModPath,
        shadow: BuiltinShadowMode,
        expected_macro_subns: Option<MacroSubNs>,
        origin: Hygienic,
    ) -> (PerNs, Option<usize>) {
        match origin {
            Hygienic::Here => {
                self.resolve_path(local_def_map, db, module, path, shadow, expected_macro_subns)
            }
            Hygienic::At(at, key) => self.map_for(db, at).resolve_path(
                self.local_for(db, at, local_def_map),
                db,
                at,
                &keyed_path(path, key),
                shadow,
                expected_macro_subns,
            ),
            Hygienic::Unknown => (PerNs::none(), None),
        }
    }

    /// The def map of `module`: this one when it is this one's, so that a def map being
    /// collected reads itself and not its query.
    pub(crate) fn map_for<'a>(&'a self, db: &'a Db, module: ModuleId) -> &'a DefMap {
        let block = module.block(db).map(|it| it.to_static());
        if module.krate(db) == self.krate && block == self.block.map(|it| it.block) {
            self
        } else {
            module.def_map(db)
        }
    }

    /// The local def map of `module`'s crate: `own` (this crate's) when it is this crate.
    pub(crate) fn local_for<'a>(
        &self,
        db: &'a Db,
        module: ModuleId,
        own: &'a LocalDefMap,
    ) -> &'a LocalDefMap {
        let krate = module.krate(db);
        if krate == self.krate { own } else { crate_local_def_map(db, krate).local(db) }
    }

    fn hygienic_step(
        &self,
        db: &Db,
        module: ModuleId,
        path: &ModPath,
        ns: Option<Namespace>,
        ctxt: SyntaxContext,
    ) -> Step {
        let ctxt = ctxt.normalize_to_macros_2_0(db);
        let Some(expn) = ctxt.outer_expn(db) else { return Step::Here };
        let expn = MacroCallId::from(expn);
        if matches!(expn.loc(db).def.kind, MacroDefKind::UnimplementedBuiltIn(_)) {
            return Step::Here;
        }
        let (map, nearest) = match path.kind {
            PathKind::Plain => {
                let Some(name) = path.segments().first() else { return Step::Here };
                // The blocks around the path, innermost first, then the module holding them:
                // rustc's ribs of anonymous modules, then the first proper module.
                let mut map = self;
                let mut at = module;
                loop {
                    if map.wrote(db, at, name, ns, ctxt) {
                        return Step::At(at);
                    }
                    match map.block {
                        Some(BlockInfo { parent, .. }) if at == map.root => {
                            at = parent;
                            map = self.map_for(db, parent);
                        }
                        _ => break,
                    }
                }
                (map, at)
            }
            PathKind::Super(_) | PathKind::Crate => {
                adjust_to_nearest_non_block_module(db, self, module)
            }
            PathKind::DollarCrate(_) | PathKind::Abs => return Step::Here,
        };
        if map.module_written_inside(db, nearest, expn) {
            // A module the expansion wrote: what is not in it are the preludes', as for a
            // module written anywhere.
            return match path.kind {
                PathKind::Plain => Step::At(nearest),
                _ => Step::Here,
            };
        }
        match self.macro_def_module(db, module, expn) {
            Some(def_site) => Step::DefSite { module: def_site, ctxt: ctxt.parent(db) },
            None => Step::Unknown,
        }
    }

    /// Whether `module`'s own scope has `name` (in `ns`, or any namespace) keyed by the context
    /// `ctxt` (normalized to macros 2.0): bound by a name the expansion `ctxt` marks wrote.
    fn wrote(
        &self,
        db: &Db,
        module: ModuleId,
        name: &Name,
        ns: Option<Namespace>,
        ctxt: SyntaxContext,
    ) -> bool {
        let wants = |it: Namespace| ns.is_none_or(|ns| ns == it);
        if wants(Namespace::Macros)
            && self.module(db, module).legacy_macro_in_scope(db, &name.keyed(ctxt)).is_some()
        {
            return true;
        }
        let res = self.name_in_module(db, module, &name.keyed(ctxt));
        (res.types.is_some() && wants(Namespace::Types))
            || (res.values.is_some() && wants(Namespace::Values))
            || (res.macros.is_some() && wants(Namespace::Macros))
    }

    /// The expansion `module`'s `mod` item (or block) is written in: rustc's
    /// `ModuleData::expansion`, what the name of a segment looked up in the module is adjusted
    /// to ([`key_in`]). `None` for a crate root and every module declared in a real file.
    pub(crate) fn module_expansion(&self, db: &Db, module: ModuleId) -> Option<MacroCallId> {
        match self.module(db, module).origin {
            ModuleOrigin::Inline { definition_tree_id, .. } => {
                expansion_of(definition_tree_id.file_id())
            }
            ModuleOrigin::File { declaration_tree_id, .. } => {
                expansion_of(declaration_tree_id.file_id())
            }
            ModuleOrigin::BlockExpr { block, .. } => expansion_of(block.file_id),
            ModuleOrigin::CrateRoot { .. } => None,
        }
    }

    /// Whether `module` is one `expn` wrote: an inline module or block in its expansion. A
    /// module of its own file never is, whoever declared it.
    fn module_written_inside(&self, db: &Db, module: ModuleId, expn: MacroCallId) -> bool {
        match self.module(db, module).origin {
            ModuleOrigin::Inline { definition_tree_id, .. } => {
                written_inside(db, definition_tree_id.file_id(), expn)
            }
            ModuleOrigin::BlockExpr { block, .. } => written_inside(db, block.file_id, expn),
            ModuleOrigin::CrateRoot { .. } | ModuleOrigin::File { .. } => false,
        }
    }

    /// The module defining the macro whose expansion is `expn`: rustc's `expn_def_scope`.
    /// A proc macro's is its crate's root. A macro declared by an item (`macro`, `macro_rules!`,
    /// a builtin's declaration in core) is looked for first in the modules around `module` (the
    /// def maps around a block, then the module's ancestors), which is all there is to read
    /// while this crate's own def map is collected; then in its crate's def map.
    fn macro_def_module(&self, db: &Db, module: ModuleId, expn: MacroCallId) -> Option<ModuleId> {
        let def = expn.loc(db).def;
        let ast_id = match def.kind {
            MacroDefKind::Declarative(ast_id, _)
            | MacroDefKind::BuiltIn(ast_id, _)
            | MacroDefKind::BuiltInAttr(ast_id, _)
            | MacroDefKind::BuiltInDerive(ast_id, _)
            | MacroDefKind::BuiltInEager(ast_id, _) => ast_id,
            MacroDefKind::ProcMacro(..) => return Some(crate_def_map(db, def.krate).root_module_id()),
            MacroDefKind::UnimplementedBuiltIn(_) => return None,
        };
        let mut map = self;
        let mut at = Some(module);
        while let Some(here) = at {
            if let Some(found) = declared_in(db, map, here, ast_id) {
                return Some(found);
            }
            at = match map.module(db, here).parent {
                Some(parent) => Some(parent),
                None => map.block.map(|BlockInfo { parent, .. }| {
                    map = self.map_for(db, parent);
                    parent
                }),
            };
        }
        // The crate's def map is collected unless it is this one, still being collected.
        let collecting = self.block.is_none()
            && def.krate == self.krate
            && !matches!(self.modules, super::ModulesSlot::Published(_) | super::ModulesSlot::Collected(_));
        match collecting {
            true => None,
            false => macro_def_module(db, def.krate, ast_id),
        }
    }
}

/// The module whose scope `at` (of `map`) declares the macro of `ast_id` in, if it does.
fn declared_in(db: &Db, map: &DefMap, at: ModuleId, ast_id: AstId<ast::Macro>) -> Option<ModuleId> {
    map.module(db, at).scope(db).macros().find_map(|(_, it)| match it.def {
        MacroId::Macro2Id(id) => {
            let loc = id.lookup(db);
            (loc.id.upcast() == ast_id).then_some(loc.container)
        }
        MacroId::MacroRulesId(id) => {
            let loc = id.lookup(db);
            (loc.id.upcast() == ast_id).then_some(loc.container)
        }
        MacroId::ProcMacroId(_) => None,
    })
}

/// [`DefMap::macro_def_module`] in the def map of `krate`, the macro's crate, once collected: a
/// scan of its modules, kept per macro.
#[blaze_query::tracked(returns(copy))]
fn macro_def_module(db: &Db, krate: Crate, ast_id: AstId<ast::Macro>) -> Option<ModuleId> {
    let map = crate_def_map(db, krate);
    map.modules(db).find_map(|(id, _)| declared_in(db, map, id, ast_id))
}

/// Whether `file` is `expn`'s expansion or the expansion of a macro called in it, at any depth.
fn written_inside(db: &Db, file: HirFileId, expn: MacroCallId) -> bool {
    let mut file = file;
    // Each turn moves to the file a macro was called in, ending at a real file.
    while let Some(call) = file.macro_file() {
        if call == expn {
            return true;
        }
        file = call.loc(db).kind.file_id();
    }
    false
}

/// The root context, which every name a real file writes has: no expansion marks it.
const ROOT: SyntaxContext = SyntaxContext::root(span::Edition::Edition2015);

// rustc's hygiene algebra (`rustc_span::hygiene`), over expansions as macro calls: `None` is
// rustc's `ExpnId::root()`, the expansion every real file is in. These are the one home of the
// rule that a key is compared by; the scopes, the generic parameters, the fields and the
// associated items all ask here.

/// The expansion the call `expn` is written in (rustc's `ExpnData::parent`): `None` for a call
/// written in a real file.
pub fn expn_parent(db: &Db, expn: MacroCallId) -> Option<MacroCallId> {
    expn.loc(db).kind.file_id().macro_file()
}

/// Whether `expn` is `ancestor` or was made inside it, at any depth (rustc's
/// `ExpnId::is_descendant_of`). Everything descends from the root, `None`.
pub fn is_descendant_of(db: &Db, expn: Option<MacroCallId>, ancestor: Option<MacroCallId>) -> bool {
    let Some(ancestor) = ancestor else { return true };
    let mut at = expn;
    // Each turn moves to the expansion a call is written in, ending at a real file (`None`).
    while let Some(here) = at {
        if here == ancestor {
            return true;
        }
        at = expn_parent(db, here);
    }
    false
}

/// The expansion whose output `file` is: `None` for a real file. What rustc calls the
/// `expn_that_defined` of an item written in `file`, and the `expansion` of a module whose
/// `mod` (or block) is written there.
pub fn expansion_of(file: HirFileId) -> Option<MacroCallId> {
    file.macro_file()
}

/// rustc's `SyntaxContext::adjust`: removes the outer marks of `ctxt` until `expn` descends
/// from its outer expansion, so that `ctxt` is what a name has as seen from a scope `expn`
/// made. Gives the last mark removed, the expansion whose def site privacy is checked from.
pub fn adjust(db: &Db, ctxt: &mut SyntaxContext, expn: Option<MacroCallId>) -> Option<MacroCallId> {
    let mut scope = None;
    // Each turn removes one mark; a root context has none and its outer expansion is the root,
    // which every expansion descends from.
    while !is_descendant_of(db, expn, ctxt.outer_expn(db).map(MacroCallId::from)) {
        scope = ctxt.remove_mark(db).0.map(MacroCallId::from);
    }
    scope
}

/// The key a name written with `ctxt` is looked up by in a scope that `expn` made (a module
/// a `macro` wrote, an ADT's fields, an impl's items): its context normalized to macros 2.0
/// and adjusted to `expn` (rustc's `normalize_to_macros_2_0_and_adjust`). `None` where that is
/// a root context, as for every scope of a real file.
pub fn key_in(db: &Db, ctxt: SyntaxContext, expn: Option<MacroCallId>) -> Option<SyntaxContext> {
    let mut ctxt = ctxt.normalize_to_macros_2_0(db);
    adjust(db, &mut ctxt, expn);
    (!ctxt.is_root()).then_some(ctxt)
}

/// rustc's `hygienic_eq`: whether a name written with `use_ctxt` names a definition of the same
/// text whose key is `def_key` ([`key_of_context`] of its name), the definition's scope being
/// made by `expn` ([`expansion_of`] the file it is written in).
pub fn hygienic_eq(
    db: &Db,
    use_ctxt: SyntaxContext,
    def_key: Option<SyntaxContext>,
    expn: Option<MacroCallId>,
) -> bool {
    key_in(db, use_ctxt, expn) == def_key
}

/// The name a field or an associated item written `name` with `ctxt` is looked up by among the
/// fields of an ADT or the items of an impl that `expn` made ([`expansion_of`] the file it is
/// written in): `name` [keyed](Name::keyed) by [`key_in`]. It equals the definition's name keyed
/// by its [`key_of_context`] exactly when [`hygienic_eq`] holds: rustc's
/// `adjust_ident_and_get_scope` and its comparison of `normalize_to_macros_2_0` idents
/// (`check_field`, `check_expr_struct_fields`, `find_by_ident_and_namespace`). A name a real
/// file or a `macro_rules!` writes, and a name looked up in a scope no `macro` made, keeps no
/// key.
pub fn name_in(db: &Db, name: Name, ctxt: SyntaxContext, expn: Option<MacroCallId>) -> Name {
    if ctxt.is_root() {
        return name;
    }
    match key_in(db, ctxt, expn) {
        Some(key) => name.keyed(key),
        None => name,
    }
}

/// rustc's `expn_that_defined` of the ADT whose variant `id` is: the expansion its `struct`,
/// `union` or `enum` is written in, which made the scope of its fields.
pub fn variant_expansion(db: &Db, id: crate::VariantId) -> Option<MacroCallId> {
    let file = match id {
        crate::VariantId::StructId(it) => it.lookup(db).id.file_id,
        crate::VariantId::UnionId(it) => it.lookup(db).id.file_id,
        crate::VariantId::EnumVariantId(it) => it.lookup(db).parent.lookup(db).id.file_id,
    };
    expansion_of(file)
}

/// rustc's `expn_that_defined` of the impl `id`: the expansion it is written in, which made
/// the scope of its items.
pub fn impl_expansion(db: &Db, id: crate::ImplId) -> Option<MacroCallId> {
    expansion_of(id.lookup(db).id.file_id)
}

/// The key the name of the associated item `item` binds among its impl's or trait's items, as
/// [`def_binding_key`]: `None` for every item a real file or a `macro_rules!` names.
pub fn assoc_item_key(db: &Db, item: crate::AssocItemId) -> Option<SyntaxContext> {
    match item {
        crate::AssocItemId::FunctionId(it) => def_binding_key(db, it.into()),
        crate::AssocItemId::ConstId(it) => def_binding_key(db, it.into()),
        crate::AssocItemId::TypeAliasId(it) => def_binding_key(db, it.into()),
    }
}

/// [`name_in`] for a method name written with `hygiene` (as an expression store keeps it,
/// [`crate::expr_store::ExpressionStore::member_hygiene`]) among the items of the impl `id`.
pub fn impl_item_name(db: &Db, id: crate::ImplId, name: Name, hygiene: HygieneId) -> Name {
    if hygiene.is_root() {
        return name;
    }
    name_in(db, name, hygiene.syntax_context(), impl_expansion(db, id))
}

/// Whether the associated item `item`, listed by its impl or trait as `item_name` (the text it
/// is declared with), is what `name` looks up ([`impl_item_name`]): the same text, and the
/// item's own key ([`assoc_item_key`]) is the name's, which is [`hygienic_eq`] (rustc's
/// `AssocItems::find_by_ident_and_namespace`). With neither keyed it is plain name equality.
pub fn assoc_item_is(db: &Db, item_name: Name, item: crate::AssocItemId, name: Name) -> bool {
    item_name.unkeyed() == name.unkeyed() && assoc_item_key(db, item) == name.key()
}

/// The root context under `ctxt`'s marks: the edition of the file its tokens were written in.
fn root_of(db: &Db, ctxt: SyntaxContext) -> SyntaxContext {
    let mut at = ctxt;
    // Each turn removes one mark.
    while !at.is_root() {
        at = at.parent(db);
    }
    at
}

/// The key a definition whose name is written with `ctxt` binds: its context normalized to
/// macros 2.0, `None` for a root context (every name a real file or a `macro_rules!` writes).
pub fn key_of_context(db: &Db, ctxt: SyntaxContext) -> Option<SyntaxContext> {
    normalized_key(db, ctxt)
}

/// The name a generic parameter (type, const or lifetime, of an item or of a `for<..>` binder)
/// written as `name` with syntax context `ctxt` is bound under in its generics, and the name a
/// path's first segment or a lifetime written with `ctxt` looks a parameter up by: `name`
/// [keyed](Name::keyed) by [`key_of_context`]. Generic parameters always have macros 2.0
/// hygiene, whatever the transparency of the macro that wrote them: rustc's
/// `with_generic_param_rib` binds `param.ident.normalize_to_macros_2_0()`, and
/// `resolve_ident_in_lexical_scope` (a rib that `contains_params`) and `resolve_lifetime` look up
/// `ident.normalize_to_macros_2_0()`. So in an opaque `macro m($T:ident) { fn f<$T, T>() }` the
/// two `T`s are two parameters, while a semi-opaque or transparent macro's `T` is the caller's.
/// `Self` is never keyed: rustc resolves it with the root context ("FIXME(jseyfried) improve
/// `Self` hygiene"). A name with no key (every one a real file or a `macro_rules!` writes)
/// stays as it is, so ordinary code compares plain names.
pub fn param_name(db: &Db, name: Name, ctxt: SyntaxContext) -> Name {
    if name == sym::Self_ {
        return name;
    }
    match key_of_context(db, ctxt) {
        Some(key) => name.keyed(key),
        None => name,
    }
}

/// The name a path's first segment `name`, written with `hygiene` (the path's
/// [`HygieneId`], its context normalized to macro_rules), looks a generic parameter up by:
/// [`param_name`] of that context, whose macros 2.0 normalization is the one `name` was written
/// with. Taken once, before the scope walk, as rustc's `resolve_ident_in_lexical_scope` takes
/// its `normalized_ident`.
pub fn path_param_name(db: &Db, name: Name, hygiene: HygieneId) -> Name {
    param_name(db, name, hygiene.syntax_context())
}

/// rustc's `SyntaxContext::reverse_glob_adjust`: the key a name keyed `key` in a scope that
/// `expn` made takes when a glob written with `glob_ctxt` imports it, so that the names the
/// glob's own expansion writes find it. `None` when the name cannot be named through the glob
/// (adjusting it to `expn` would remove a mark: it is hidden from the scope's own view).
pub fn reverse_glob_adjust(
    db: &Db,
    key: Option<SyntaxContext>,
    expn: Option<MacroCallId>,
    glob_ctxt: SyntaxContext,
) -> Option<Option<SyntaxContext>> {
    let mut glob = glob_ctxt.normalize_to_macros_2_0(db);
    // A name with no key gets the glob's marks on the root its tokens have, so that the
    // context made is the one the expansion's own tokens of that name have.
    let mut ctxt = key.unwrap_or_else(|| root_of(db, glob));
    if adjust(db, &mut ctxt, expn).is_some() {
        return None;
    }
    // alloc: none for up to 4 marks, the depth of nested `macro`s a glob is written in
    let mut marks: smallvec::SmallVec<[(MacroCallId, hir_expand::hygiene::Transparency); 4]> =
        smallvec::SmallVec::new();
    // Each turn removes one of the glob context's marks, ending at the root at the latest.
    while !is_descendant_of(db, expn, glob.outer_expn(db).map(MacroCallId::from)) {
        let (Some(mark), transparency) = glob.remove_mark(db) else { break };
        marks.push((MacroCallId::from(mark), transparency));
    }
    while let Some((mark, transparency)) = marks.pop() {
        let edition = mark.loc(db).def.edition;
        ctxt = hir_expand::hygiene::apply_mark(db, ctxt, mark.into(), transparency, edition);
    }
    Some(normalized_key(db, ctxt))
}

/// `path` with its first segment keyed by `key` ([`Name::keyed`]), for the lookup in the scope
/// [`Hygienic::At`] names. Only a plain path's first segment is looked up by key; `self::`,
/// `super::` and `crate::` name modules.
pub(crate) fn keyed_path(path: &ModPath, key: SyntaxContext) -> ModPath {
    match (path.kind, path.segments().split_first()) {
        (PathKind::Plain, Some((first, rest))) if !key.is_root() => ModPath::from_segments_in(
            PathKind::Plain,
            std::iter::once(first.keyed(key))
                .chain(rest.iter().copied())
                .enumerate()
                .map(|(i, name)| (name, path.context_of(i, ROOT))),
        ),
        _ => *path,
    }
}

/// The key `name`, the name `def` is declared with, binds in its module's scope: keyed by the
/// context it is written with ([`Name::keyed`]). A builtin type is not keyed, nor an enum's
/// variant.
pub(crate) fn def_binding_name(db: &Db, def: ModuleDefId, name: Name) -> Name {
    match def_binding_key(db, def) {
        Some(key) => name.keyed(key),
        None => name,
    }
}

/// The name the `mod` declared at `declaration` of `file` binds in its parent's scope and its
/// module id is made with: keyed by the context it is written with, as every item's name
/// (rustc plants a module under its `IdentKey` too). Two `mod foo` that a `macro` writes and
/// is handed are two modules; the path of a module's file is found by its name's text.
pub(crate) fn module_binding_name(
    db: &Db,
    file: HirFileId,
    declaration: span::FileAstId<ast::Module>,
    name: Name,
) -> Name {
    match item_key(db, AstId::new(file, declaration)) {
        Some(key) => name.keyed(key),
        None => name,
    }
}

/// The name the `extern crate` `id` binds in its module's scope: keyed by the context its name
/// (or rename) is written with, as an item's. The extern prelude takes the name alone.
pub(crate) fn extern_crate_binding_name(db: &Db, id: crate::ExternCrateId, name: Name) -> Name {
    match item_key(db, id.lookup(db).id) {
        Some(key) => name.keyed(key),
        None => name,
    }
}

/// The context, normalized to macros 2.0, that the binding of `def`'s name is keyed by:
/// `None` where it is a root context, as for every item a real file or a `macro_rules!` writes.
pub fn def_binding_key(db: &Db, def: ModuleDefId) -> Option<SyntaxContext> {
    match def {
        ModuleDefId::FunctionId(it) => item_key(db, it.lookup(db).ast_id()),
        ModuleDefId::AdtId(crate::AdtId::StructId(it)) => item_key(db, it.lookup(db).ast_id()),
        ModuleDefId::AdtId(crate::AdtId::UnionId(it)) => item_key(db, it.lookup(db).ast_id()),
        ModuleDefId::AdtId(crate::AdtId::EnumId(it)) => item_key(db, it.lookup(db).ast_id()),
        ModuleDefId::ConstId(it) => item_key(db, it.lookup(db).ast_id()),
        ModuleDefId::StaticId(it) => item_key(db, it.lookup(db).ast_id()),
        ModuleDefId::TraitId(it) => item_key(db, it.lookup(db).ast_id()),
        ModuleDefId::TypeAliasId(it) => item_key(db, it.lookup(db).ast_id()),
        // A module's id holds its keyed name ([`module_binding_name`]).
        ModuleDefId::ModuleId(it) => it.name(db).and_then(|name| name.key()),
        ModuleDefId::MacroId(it) => macro_binding_key(db, it),
        ModuleDefId::EnumVariantId(_) | ModuleDefId::BuiltinType(_) => None,
    }
}

/// The context, normalized to macros 2.0, that a binding whose name is written at `range` of
/// `file` is keyed by, as [`def_binding_key`]: for a reader that has the syntax.
pub fn binding_key_at(db: &Db, file: HirFileId, range: TextRange) -> Option<SyntaxContext> {
    if file.macro_file().is_none() {
        return None;
    }
    normalized_key(db, file.span_map(db).span_for_range(range).ctx)
}

/// The context, normalized to macros 2.0, that the name the import `import` binds is keyed by,
/// as [`def_binding_key`].
pub fn import_binding_key(db: &Db, import: ImportId) -> Option<SyntaxContext> {
    let loc = import.use_.lookup(db);
    let call = loc.id.file_id.macro_file()?;
    use_tree_key(db, call, import.use_, import.idx.into_raw().into_u32())
}

/// `name` keyed as the name of the macro `def` is written ([`def_binding_key`]): what the
/// textual scope of `macro_rules!` binds it under.
pub(crate) fn macro_def_name(db: &Db, def: hir_expand::MacroDefId, name: Name) -> Name {
    let key = match def.kind {
        MacroDefKind::Declarative(ast_id, _) => item_key(db, ast_id),
        MacroDefKind::BuiltIn(..)
        | MacroDefKind::BuiltInAttr(..)
        | MacroDefKind::BuiltInDerive(..)
        | MacroDefKind::BuiltInEager(..)
        | MacroDefKind::ProcMacro(..)
        | MacroDefKind::UnimplementedBuiltIn(_) => None,
    };
    match key {
        Some(key) => name.unkeyed().keyed(key),
        None => name.unkeyed(),
    }
}

/// The context, normalized to macros 2.0, a macro's name is written with, as
/// [`def_binding_key`]: what the textual scope of `macro_rules!` binds its name under.
pub(crate) fn macro_binding_key(db: &Db, mac: MacroId) -> Option<SyntaxContext> {
    match mac {
        MacroId::Macro2Id(it) => item_key(db, it.lookup(db).id),
        MacroId::MacroRulesId(it) => item_key(db, it.lookup(db).id),
        MacroId::ProcMacroId(it) => item_key(db, it.lookup(db).id),
    }
}

fn normalized_key(db: &Db, ctxt: SyntaxContext) -> Option<SyntaxContext> {
    if ctxt.is_root() {
        return None;
    }
    let key = ctxt.normalize_to_macros_2_0(db);
    (!key.is_root()).then_some(key)
}

/// The key of the name of the item `ast`: `None` in a real file, where every name has the root
/// context.
fn item_key<N: AstIdNode>(db: &Db, ast: AstId<N>) -> Option<SyntaxContext> {
    let call = ast.file_id.macro_file()?;
    expanded_item_key(db, call, ast.value.erase())
}

/// The key of the name of the item `ast` of `call`'s expansion. A query of its own so that the
/// def map, which keys each binding by it, reads this answer and not the expansion's syntax: an
/// edit inside a macro's input that keeps the item's name context keeps the def map.
#[blaze_query::tracked(returns(copy))]
fn expanded_item_key(db: &Db, call: MacroCallId, ast: ErasedFileAstId) -> Option<SyntaxContext> {
    let file = HirFileId::from(call);
    let node = view::to_node(file.ast_id_map(db).get_erased(ast), file.root(db));
    normalized_key(db, name_range(node).map_or(ROOT, |range| file.span_map(db).span_for_range(range).ctx))
}

/// The key of the name the `idx`th use tree of `use_`, an item of `call`'s expansion, binds:
/// as [`expanded_item_key`], a query of its own.
#[blaze_query::tracked(returns(copy))]
fn use_tree_key(db: &Db, call: MacroCallId, use_: UseId, idx: u32) -> Option<SyntaxContext> {
    let file = HirFileId::from(call);
    let trees = use_trees_view(db, use_.lookup(db).id);
    let tree = trees[Idx::from_raw(la_arena::RawIdx::from_u32(idx))];
    normalized_key(db, name_range(view::AstNode::syntax(&tree)).map_or(ROOT, |range| file.span_map(db).span_for_range(range).ctx))
}

/// The context a glob is written with, normalized to macros 2.0: its `*`'s (rustc's
/// `import.span`), what [`reverse_glob_adjust`] adds to the names it imports. A root context in
/// a real file.
pub fn glob_context(db: &Db, glob: crate::item_scope::GlobId) -> SyntaxContext {
    let loc = glob.use_.lookup(db);
    match loc.id.file_id.macro_file() {
        None => ROOT,
        Some(call) => expanded_glob_context(db, call, glob.use_, glob.idx.into_raw().into_u32()),
    }
}

/// [`glob_context`] of a glob of `call`'s expansion: a query of its own, as
/// [`expanded_item_key`].
#[blaze_query::tracked(returns(copy))]
fn expanded_glob_context(db: &Db, call: MacroCallId, use_: UseId, idx: u32) -> SyntaxContext {
    let file = HirFileId::from(call);
    let trees = use_trees_view(db, use_.lookup(db).id);
    let tree = trees[Idx::from_raw(la_arena::RawIdx::from_u32(idx))];
    let range = match tree.star_token() {
        Some(star) => star.text_range(),
        None => view::AstNode::syntax(&tree).text_range(),
    };
    file.span_map(db).span_for_range(range).ctx.normalize_to_macros_2_0(db)
}

/// The name `name` of a scope that `expn` made takes when a glob written with `glob_ctxt`
/// ([`glob_context`]) imports it: [`reverse_glob_adjust`] of its key, `None` when the glob
/// cannot name it. A name with no key through a glob a real file writes is itself.
pub fn glob_imported_name(
    db: &Db,
    name: Name,
    expn: Option<MacroCallId>,
    glob_ctxt: SyntaxContext,
) -> Option<Name> {
    let key = name.key();
    if key.is_none() && glob_ctxt.is_root() {
        return Some(name);
    }
    let plain = name.unkeyed();
    Some(match reverse_glob_adjust(db, key, expn, glob_ctxt)? {
        Some(key) => plain.keyed(key),
        None => plain,
    })
}

/// Where the name a binding takes is written in `node`, an item or a `use` tree.
fn name_range(node: view::Node<'_>) -> Option<TextRange> {
    let range = |it: view::Node<'_>| it.text_range();
    if let Some(rename) = node.children().find_map(view::Rename::cast) {
        return view::HasName::name(&rename).map(|it| range(view::AstNode::syntax(&it)));
    }
    if let Some(name) = node.children().find_map(view::Name::cast) {
        return Some(range(view::AstNode::syntax(&name)));
    }
    if let Some(tree) = view::UseTree::cast(node) {
        let name = tree.path()?.segment()?.name_ref()?;
        return Some(range(view::AstNode::syntax(&name)));
    }
    node.children().find_map(view::NameRef::cast).map(|it| range(view::AstNode::syntax(&it)))
}

