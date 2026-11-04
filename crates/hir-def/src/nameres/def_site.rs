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
//! The first segment of every path goes through [`DefMap::hygienic_origin`]: an expression's or
//! type's, a macro call's, a `use` path's (with the context of its first token, `ImportOp::ctxt`)
//! and a `pub(in path)` or `pub(super)` visibility's (`RawVisibility::Module`'s context). `self`
//! and `super` are rustc's `resolve_self` ([`DefMap::resolve_self`]): the nearest module around
//! the path whose `mod` is written with the keyword's context ([`DefMap::module_key`]), going on
//! past the crate root at the def site of the context's outer mark; each `super` then steps to the
//! parent and does the same. `crate` and a 2015 `::` are rustc's `resolve_crate_root`: the root of
//! the crate defining the macro of the context's first opaque mark. A 2015 import's plain path
//! starts at that crate root too. So a `use super::*` a `macro` writes in a module it writes
//! under a child of its call site reaches the macro's own module, as in rustc's `globs.rs`.
//!
//! An enum's variants are keyed the same way where they are lowered ([`variant_name`], in
//! `EnumVariants` and the variant's id), so an enum a `macro` writes may have `V` and a `$v` it
//! is handed as `V` (rustc's `build_reduced_graph_for_variant`, under the enum's module). A
//! path's segment after the enum (`E::V`, `<E>::V`, `Self::V`, `use E::V`) is looked up by
//! [`name_in`] the scope the enum's expansion made ([`enum_expansion`]), as rustc's
//! `resolve_ident_in_module` and the `hygienic_eq` of its type-relative lookups do, and a glob of
//! an enum imports each variant under [`reverse_glob_adjust`], as a glob of a module does. A
//! record literal's or a pattern's path takes its hygiene as a path expression's does, so the
//! enum a `macro` writes and names there (`Rec::V { .. }`, `Tup::V(..)`) is found.
//!
//! A trait's items and a trait impl's are compared the same way. A name a method call or a path
//! segment ([`crate::expr_store::path::Path::segment_hygiene`]) writes is looked up among a
//! trait's items by [`trait_item_name`] (rustc's `find_by_ident_and_namespace`), among the traits
//! of rustc's hygienic walk where a `macro` writes it ([`for_each_hygienic_trait_in_scope`],
//! `traits_in_scope`). A name asked of bounds and supertraits before its trait is known
//! (`T::Assoc`, `Trait<Assoc = ..>`) is a [`written_name`] that each trait adjusts
//! ([`trait_item_written`], [`trait_item_is_written`]). An impl's item implements the trait's
//! item it [`implements`] (rustc's `check_trait_item`), which E0407, E0046, E0201,
//! `lookup_impl_method` and the solver's `fetch_eligible_assoc_item` read. Item lists keep plain
//! names, so an impl a `macro` or a derive writes for a real file's trait still matches by text.
//!
//! The privacy of a field or an inherent impl's item that a name written in a `macro` reaches is
//! checked from the def site [`privacy_scope`] gives (rustc's `adjust_ident_and_get_scope`): the
//! module defining the macro of the last mark [`adjust`] removes, so a `pub macro` a `macro`
//! writes reads the private fields of a struct written beside it. Field accesses (a tuple
//! index too), record literals and patterns, `offset_of!` and method calls check it so; an
//! `offset_of!`'s field is looked up by [`name_in`] as a field access is. A signature's
//! lifetime is compared with its binders' by key (E0261, blaze-diagnostics' `lifetimes`).
//!
//! Not hygienic: positional fields, as in rustc ("FIXME(jseyfried): positional field hygiene").

use base_db::{Crate, Db};
use hir_expand::{
    AstId, HirFileId, MacroCallId, MacroDefKind,
    mod_path::{ModPath, PathKind},
    name::Name,
};
use intern::sym;
use la_arena::Idx;
use span::{AstIdNode, Edition, ErasedFileAstId, SyntaxContext, TextRange};
use syntax::{
    ast,
    view::{self, AstNode as _},
};

use crate::{
    AstIdLoc, Lookup, MacroId, ModuleDefId, ModuleId, UseId,
    expr_store::HygieneId,
    item_scope::{BuiltinShadowMode, ImportId},
    nameres::{
        BlockInfo, DefMap, LocalDefMap, MacroSubNs, ModuleOrigin, crate_def_map,
        crate_local_def_map,
        path_resolution::{PathRead, adjust_to_nearest_non_block_module},
    },
    per_ns::{Namespace, PerNs},
    src::use_trees_view,
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
    /// followed. For a `self`, `super`, `crate` or 2015 `::` path, the module the keyword names
    /// ([`DefMap::resolve_self`], [`DefMap::crate_origin`]), where its segments go on as
    /// `self::` ones ([`keyed_path`]).
    At(ModuleId, SyntaxContext),
    /// The path names nothing from here: the module defining the `macro` is not known yet (its
    /// own crate's def map is being collected, and the macro is in no module around the path),
    /// or its `super`s go above the crate root. The path is unresolved.
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
        self.hygienic_origin_reading(db, module, path, ns, ctxt, &mut |_| {})
    }

    /// [`DefMap::hygienic_origin`], calling `reads` with each scope slot the walk read for a
    /// name the expansion wrote ([`PathRead::Slot`], [`PathRead::LegacyMacro`] of the keyed
    /// name), as [`DefMap::resolve_path_fp_recording`] reports a lookup's reads: an import of a
    /// def map being collected is retried when one of them is written.
    pub(crate) fn hygienic_origin_reading(
        &self,
        db: &Db,
        module: ModuleId,
        path: &ModPath,
        ns: Option<Namespace>,
        ctxt: SyntaxContext,
        reads: &mut impl FnMut(PathRead),
    ) -> Hygienic {
        match path.kind {
            PathKind::Super(levels) => return self.super_origin(db, module, levels, ctxt),
            PathKind::Crate => return self.crate_origin(db, ctxt),
            PathKind::Abs if self.data.edition == Edition::Edition2015 => {
                return self.crate_origin(db, ctxt);
            }
            PathKind::Abs | PathKind::DollarCrate(_) => return Hygienic::Here,
            PathKind::Plain => {}
        }
        let mut module = module;
        let mut ctxt = ctxt;
        let mut moved = false;
        // Each turn either ends or follows one opaque mark out to its def site with the context
        // outside it (`parent`), so it ends after as many turns as `ctxt` has opaque marks.
        loop {
            let map = self.map_for(db, module);
            match map.hygienic_step(db, module, path, ns, ctxt, reads) {
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

    /// One step of the lexical walk of a plain path's first segment (rustc's
    /// `early_resolve_ident_in_lexical_scope` over `hygienic_lexical_parent`).
    fn hygienic_step(
        &self,
        db: &Db,
        module: ModuleId,
        path: &ModPath,
        ns: Option<Namespace>,
        ctxt: SyntaxContext,
        reads: &mut impl FnMut(PathRead),
    ) -> Step {
        let ctxt = ctxt.normalize_to_macros_2_0(db);
        let Some(expn) = ctxt.outer_expn(db) else { return Step::Here };
        let expn = MacroCallId::from(expn);
        if matches!(expn.loc(db).def.kind, MacroDefKind::UnimplementedBuiltIn(_)) {
            return Step::Here;
        }
        let Some(name) = path.segments().first() else { return Step::Here };
        // The blocks around the path, innermost first, then the module holding them: rustc's
        // ribs of anonymous modules, then the first proper module.
        let mut map = self;
        let mut at = module;
        loop {
            if map.wrote(db, at, name, ns, ctxt, reads) {
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
        if map.module_written_inside(db, at, expn) {
            // A module the expansion wrote: what is not in it are the preludes', as for a
            // module written anywhere.
            return Step::At(at);
        }
        match self.macro_def_module(db, module, expn) {
            Some(def_site) => Step::DefSite { module: def_site, ctxt: ctxt.parent(db) },
            None => Step::Unknown,
        }
    }

    /// Where a `self` (`levels` 0) or `super` path whose keyword is written with `ctxt` starts,
    /// written in `module`: rustc's `resolve_path` of `kw::SelfLower` (`resolve_self`) and of
    /// each `kw::Super` (`resolve_super_in_module`: the first shares the context `resolve_self`
    /// moved, each later one starts again from the keyword's). [`Hygienic::Here`] where that is
    /// the module the path names as written, as in every module no `macro` wrote.
    fn super_origin(&self, db: &Db, module: ModuleId, levels: u8, ctxt: SyntaxContext) -> Hygienic {
        let written = ctxt.normalize_to_macros_2_0(db);
        let mut ctxt = written;
        let Some(mut at) = self.resolve_self(db, &mut ctxt, module, module) else {
            return Hygienic::Unknown;
        };
        // The module the path names as written: `None` above the crate root.
        let mut plain = Some(adjust_to_nearest_non_block_module(db, self, module));
        for level in 0..levels {
            let Some(parent) = self.map_for(db, at).module(db, at).parent else {
                return Hygienic::Unknown;
            };
            if level > 0 {
                ctxt = written;
            }
            let Some(next) = self.resolve_self(db, &mut ctxt, parent, module) else {
                return Hygienic::Unknown;
            };
            at = next;
            plain = plain.and_then(|(map, it)| {
                let parent = map.module(db, it).parent?;
                Some(adjust_to_nearest_non_block_module(db, self.map_for(db, parent), parent))
            });
        }
        match plain.is_some_and(|(_, it)| it == at) {
            true => Hygienic::Here,
            false => Hygienic::At(at, written),
        }
    }

    /// rustc's `resolve_self`: the module a `self` written with `ctxt` (normalized to macros
    /// 2.0) names in `module`: the nearest proper module around it whose `mod` is written with
    /// that context ([`DefMap::module_key`]); past the crate root, the module defining the
    /// macro of `ctxt`'s outer mark (rustc's `expn_def_scope`), with the mark removed from
    /// `ctxt`, and on out from there. That module is looked for first in the modules around
    /// `path_module`, the module the path is written in (a def map being collected knows only
    /// those). `None` where it is not known yet ([`Hygienic::Unknown`]).
    pub(crate) fn resolve_self(
        &self,
        db: &Db,
        ctxt: &mut SyntaxContext,
        module: ModuleId,
        path_module: ModuleId,
    ) -> Option<ModuleId> {
        let (mut map, mut at) =
            adjust_to_nearest_non_block_module(db, self.map_for(db, module), module);
        // Where the module defining a macro is looked for first: the modules around the path,
        // then around each def site passed.
        let mut search = path_module;
        // Each turn moves to a parent module, or past a crate root to a def site with one mark
        // of `ctxt` removed: it ends after as many crate roots as `ctxt` has marks, at the
        // latest at a crate root, whose `mod` has the root context.
        loop {
            if map.module_key(db, at) == (!ctxt.is_root()).then_some(*ctxt) {
                return Some(at);
            }
            let next = match map.module(db, at).parent {
                Some(parent) => parent,
                None => {
                    let (Some(expn), _) = ctxt.remove_mark(db) else { return None };
                    search = self.map_for(db, search).macro_def_module(
                        db,
                        search,
                        MacroCallId::from(expn),
                    )?;
                    search
                }
            };
            (map, at) = adjust_to_nearest_non_block_module(db, self.map_for(db, next), next);
        }
    }

    /// rustc's `resolve_crate_root` of a `crate` (or 2015 `::`) written with `ctxt`: the root
    /// of the crate defining the macro of the first mark `ctxt`, normalized to macros 2.0, has
    /// (the last [`adjust`] to the root removes), or this crate's when it has none
    /// ([`Hygienic::Here`]).
    pub(crate) fn crate_origin(&self, db: &Db, ctxt: SyntaxContext) -> Hygienic {
        let mut ctxt = ctxt.normalize_to_macros_2_0(db);
        let Some(expn) = adjust(db, &mut ctxt, None) else { return Hygienic::Here };
        let krate = expn.loc(db).def.krate;
        match krate == self.krate {
            true => Hygienic::Here,
            false => Hygienic::At(crate_def_map(db, krate).root_module_id(), ROOT),
        }
    }

    /// Where the first segment of the import `path`, written in `module` with `ctxt` (the
    /// context of the path's first token, [`use_path_context`]), is looked up: as
    /// [`DefMap::hygienic_origin_reading`], but a 2015 plain path starts at the crate root of
    /// its first token's context, as rustc gives it a `{{root}}` segment of that span and
    /// resolves that by `resolve_crate_root`.
    pub(crate) fn import_origin(
        &self,
        db: &Db,
        module: ModuleId,
        path: &ModPath,
        ctxt: SyntaxContext,
        reads: &mut impl FnMut(PathRead),
    ) -> Hygienic {
        if path.kind == PathKind::Plain && self.data.edition == Edition::Edition2015 {
            return self.crate_origin(db, ctxt);
        }
        let ns = if path.segments().len() > 1 { Some(Namespace::Types) } else { None };
        self.hygienic_origin_reading(db, module, path, ns, ctxt, reads)
    }

    /// The key of `module`'s `mod` item: the context its `mod` keyword is written with,
    /// normalized to macros 2.0 (rustc's `module.span.ctxt()` that `resolve_self` compares).
    /// `None` for a crate root and every `mod` a real file or a `macro_rules!` writes.
    pub(crate) fn module_key(&self, db: &Db, module: ModuleId) -> Option<SyntaxContext> {
        origin_key(db, &self.module(db, module).origin)
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
        reads: &mut impl FnMut(PathRead),
    ) -> bool {
        let wants = |it: Namespace| ns.is_none_or(|ns| ns == it);
        let keyed = name.keyed(ctxt);
        if wants(Namespace::Macros) {
            reads(PathRead::LegacyMacro(module, keyed));
            if self.module(db, module).legacy_macro_in_scope(db, &keyed).is_some() {
                return true;
            }
        }
        reads(PathRead::Slot(module, keyed));
        let res = self.name_in_module(db, module, &keyed);
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
            MacroDefKind::ProcMacro(..) => {
                return Some(crate_def_map(db, def.krate).root_module_id());
            }
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
            && !matches!(
                self.modules,
                super::ModulesSlot::Published(_) | super::ModulesSlot::Collected(_)
            );
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

/// rustc's `TyCtxt::adjust_ident_and_get_scope`, its scope: the module from which the privacy of
/// a field or associated item named with `ctxt` is checked, among the fields of an ADT or the
/// items of an impl that `expn` made ([`variant_expansion`], [`impl_expansion`]). That is the
/// module defining the macro of the last expansion [`adjust`] removes from `ctxt` normalized to
/// macros 2.0 (rustc's `ExpnData::parent_module`, its nearest module that is no block), looked
/// for from `here`, the module the name is written in. `None` where adjusting removes no mark,
/// or where that expansion's macro has no module (an unimplemented builtin: rustc's
/// `parent_module` of `None`): then the name is checked from `here`, as rustc's
/// `unwrap_or_else(parent_module)` does. So a `pub macro` a `macro` writes in a module reads the
/// private fields of a struct written there, wherever it is called.
///
/// A macro declared by an item always has a module once the def maps are collected, so one not
/// found is a broken invariant: it stops, naming the macro, the expansion and `here`, and never
/// checks from `here` in its place (no fallback).
pub fn privacy_scope(
    db: &Db,
    ctxt: SyntaxContext,
    expn: Option<MacroCallId>,
    here: ModuleId,
) -> Option<ModuleId> {
    if ctxt.is_root() {
        return None;
    }
    let mut ctxt = ctxt.normalize_to_macros_2_0(db);
    let scope = adjust(db, &mut ctxt, expn)?;
    let def = scope.loc(db).def;
    if matches!(def.kind, MacroDefKind::UnimplementedBuiltIn(_)) {
        return None;
    }
    let map = here.def_map(db);
    let Some(module) =
        map.macro_def_module(db, here, scope).or_else(|| in_blocks_around(db, map, scope))
    else {
        panic!(
            "def-site privacy (rustc's `adjust_ident_and_get_scope`): no module declares the macro \
             {:?} of crate {:?} whose expansion {scope:?} a name written in module {here:?} \
             adjusts out of",
            def.kind, def.krate,
        )
    };
    // rustc's `nearest_parent_mod`: each turn leaves one block, ending at the crate root.
    let mut module = module;
    while module.is_block_module(db) {
        let Some(parent) = module.containing_module(db) else {
            panic!(
                "def-site privacy: the block module {module:?} defining {:?} has no parent",
                def.kind
            )
        };
        module = parent.to_static();
    }
    Some(module)
}

/// The module of a block def map around `map` (or `map` itself) that declares the macro of
/// `expn`: a `macro` written in a module inside a block, which no walk out of a module passes.
fn in_blocks_around(db: &Db, map: &DefMap, expn: MacroCallId) -> Option<ModuleId> {
    let MacroDefKind::Declarative(ast_id, _) = expn.loc(db).def.kind else { return None };
    let mut map = map;
    // Each turn moves to the def map holding the block, ending at a crate's.
    while let Some(BlockInfo { parent, .. }) = map.block {
        if let Some(found) = map.modules(db).find_map(|(id, _)| declared_in(db, map, id, ast_id)) {
            return Some(found);
        }
        map = parent.def_map(db);
    }
    None
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

/// rustc's `expn_that_defined` of the enum `id`: the expansion its `enum` is written in, which
/// made the scope of its variants (the `expansion` of rustc's module of an enum, made by
/// `build_reduced_graph_for_item`), what a variant's name in a path is adjusted to.
pub fn enum_expansion(db: &Db, id: crate::EnumId) -> Option<MacroCallId> {
    expansion_of(id.lookup(db).id.file_id)
}

/// The name a variant whose name is `name` (written in `file`) binds among its enum's
/// variants: keyed by the context it is written with ([`binding_key_at`]), as rustc's
/// `build_reduced_graph_for_variant` defines it under its `BindingKey` in the enum's module.
/// An enum a `macro` writes may so have `V` and a `$v` it is handed as `V`, two variants. A
/// variant a real file or a `macro_rules!` names is its plain name.
pub(crate) fn variant_name(db: &Db, file: HirFileId, name: Option<view::Name<'_>>) -> Name {
    let Some(name) = name else { return Name::missing() };
    let plain = hir_expand::name::AsName::as_name(&name);
    match binding_key_at(db, file, view::AstNode::syntax(&name).text_range()) {
        Some(key) => plain.keyed(key),
        None => plain,
    }
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
/// is declared with), is what `name` looks up ([`impl_item_name`], [`trait_item_name`]): the
/// same text, and the item's own key ([`assoc_item_key`]) is the name's, which is
/// [`hygienic_eq`] (rustc's `AssocItems::find_by_ident_and_namespace`). With neither keyed it
/// is plain name equality.
pub fn assoc_item_is(db: &Db, item_name: Name, item: crate::AssocItemId, name: Name) -> bool {
    item_name.unkeyed() == name.unkeyed() && assoc_item_key(db, item) == name.key()
}

/// rustc's `expn_that_defined` of the trait `id`: the expansion it is written in, which made
/// the scope of its items.
pub fn trait_expansion(db: &Db, id: crate::TraitId) -> Option<MacroCallId> {
    expansion_of(id.lookup(db).id.file_id)
}

/// [`name_in`] for an associated item name written with `hygiene` (a method call's
/// `member_hygiene`, a path's segment) among the items of the trait `id`, as
/// [`impl_item_name`] among an impl's. What [`assoc_item_is`] compares a trait's items with.
pub fn trait_item_name(db: &Db, id: crate::TraitId, name: Name, hygiene: HygieneId) -> Name {
    if hygiene.is_root() {
        return name;
    }
    name_in(db, name, hygiene.syntax_context(), trait_expansion(db, id))
}

/// `name`, the name of the associated item `item`, [keyed](Name::keyed) by the item's own key
/// ([`assoc_item_key`]): what it binds among its trait's items, rustc's `BindingKey`. The names
/// [`trait_item_name`] gives are compared with it by equality.
pub fn assoc_item_binding_name(db: &Db, item: crate::AssocItemId, name: Name) -> Name {
    match assoc_item_key(db, item) {
        Some(key) => name.unkeyed().keyed(key),
        None => name.unkeyed(),
    }
}

/// The name an associated item written `name` with `hygiene` (a path segment's
/// [`crate::expr_store::path::Path::segment_hygiene`]) is asked for by where the trait it is
/// looked up in is not known yet (the key of a query that searches bounds and supertraits,
/// `T::Assoc`): `name` [keyed](Name::keyed) by [`key_of_context`], rustc's `assoc_ident` before
/// any trait adjusts it. [`trait_item_written`] adjusts it to each trait it is looked up in.
pub fn written_name(db: &Db, name: Name, hygiene: HygieneId) -> Name {
    if hygiene.is_root() {
        return name;
    }
    match key_of_context(db, hygiene.syntax_context()) {
        Some(key) => name.unkeyed().keyed(key),
        None => name,
    }
}

/// [`trait_item_name`] of a [`written_name`]: the name it looks an item of the trait `id` up
/// by. A name with no key is itself.
pub fn trait_item_written(db: &Db, id: crate::TraitId, written: Name) -> Name {
    match written.key() {
        Some(key) => name_in(db, written.unkeyed(), key, trait_expansion(db, id)),
        None => written,
    }
}

/// Whether `item`, a trait's item listed as `item_name`, is what the [`written_name`] `written`
/// looks up among that trait's items: [`assoc_item_is`] of [`trait_item_written`], rustc's
/// `hygienic_eq(assoc_ident, item.ident, trait_def_id)` in `trait_may_define_assoc_item` and
/// `trait_defines_associated_item_named`. An item of no trait is looked up by no shorthand.
pub fn trait_item_is_written(
    db: &Db,
    item_name: Name,
    item: crate::AssocItemId,
    written: Name,
) -> bool {
    if item_name.unkeyed() != written.unkeyed() {
        return false;
    }
    let container = match item {
        crate::AssocItemId::FunctionId(it) => it.lookup(db).container,
        crate::AssocItemId::ConstId(it) => it.lookup(db).container,
        crate::AssocItemId::TypeAliasId(it) => it.lookup(db).container,
    };
    let crate::ItemContainerId::TraitId(trait_) = container else { return false };
    assoc_item_is(db, item_name, item, trait_item_written(db, trait_, written))
}

/// The key of a name written with `hygiene`, as a [`written_name`] has it: its context
/// normalized to macros 2.0, `None` where that is a root context (every name a real file or a
/// `macro_rules!` writes), for which the traits in scope are the plain walk's.
pub fn written_key(db: &Db, hygiene: HygieneId) -> Option<SyntaxContext> {
    key_of_context(db, hygiene.syntax_context())
}

/// Whether the item `impl_item` of an impl of the trait `trait_`, declared `impl_name`,
/// implements the trait's item `trait_item`, declared `trait_name` (the caller matches their
/// kinds): rustc's `check_trait_item`, which adjusts the impl item's ident with
/// `normalize_to_macros_2_0_and_adjust(module.expansion)` of the trait's module and looks its
/// `BindingKey` up there. The same text, and the impl item's key adjusted to the trait's
/// expansion is the trait item's own key. So an impl a `macro` writes for a trait of a real
/// file implements the trait's items by their text, while a trait a `macro` writes is
/// implemented only by the items written with the context its own items are.
pub fn implements(
    db: &Db,
    trait_: crate::TraitId,
    impl_item: crate::AssocItemId,
    impl_name: Name,
    trait_item: crate::AssocItemId,
    trait_name: Name,
) -> bool {
    impl_name.unkeyed() == trait_name.unkeyed()
        && implemented_name(db, || Some(trait_), impl_item, impl_name)
            == Some(assoc_item_binding_name(db, trait_item, trait_name))
}

/// The name the item `impl_item` of a trait impl, declared `impl_name`, implements among its
/// trait's items, as [`assoc_item_binding_name`] gives theirs ([`implements`]): its text keyed by
/// its own key adjusted to the trait's expansion. `trait_` gives the trait, asked of only for an
/// item that has a key; `None` where that trait is not known, and the item implements nothing.
pub fn implemented_name(
    db: &Db,
    trait_: impl FnOnce() -> Option<crate::TraitId>,
    impl_item: crate::AssocItemId,
    impl_name: Name,
) -> Option<Name> {
    Some(match assoc_item_key(db, impl_item) {
        Some(key) => name_in(db, impl_name.unkeyed(), key, trait_expansion(db, trait_()?)),
        None => impl_name.unkeyed(),
    })
}

/// The traits in scope for an associated item name written with the key `ctxt` ([`written_key`])
/// in `module` (a block's or a module's root): rustc's `traits_in_scope`, which walks
/// `visit_scopes` from the module and leaves each by `hygienic_lexical_parent`. A module whose
/// expansion ([`DefMap::module_expansion`]) does not descend from the outer mark of `ctxt` is
/// followed by the module that defines that mark's macro, with the mark removed
/// (`expn_def_scope`); a block by the module around it; any other module ends the walk, which
/// the prelude's traits close. Every trait of a module visited is in scope, whatever its key, as
/// rustc's `traits_in_module` takes them. Calls `f` with each, innermost first, with repeats.
pub fn for_each_hygienic_trait_in_scope(
    db: &Db,
    module: ModuleId,
    ctxt: SyntaxContext,
    mut f: impl FnMut(crate::TraitId),
) {
    let mut ctxt = ctxt.normalize_to_macros_2_0(db);
    let mut at = module;
    // Each turn either removes one mark of `ctxt` or leaves a block for the module around it,
    // which is outside it: it ends after at most as many turns as `ctxt` has marks times the
    // depth of the blocks a def site is in.
    loop {
        let map = at.def_map(db);
        map.module(db, at).traits_in_scope(db).for_each(&mut f);
        let outer = ctxt.outer_expn(db).map(MacroCallId::from);
        if !is_descendant_of(db, map.module_expansion(db, at), outer) {
            let (Some(mark), _) = ctxt.remove_mark(db) else { break };
            match map.macro_def_module(db, at, MacroCallId::from(mark)) {
                Some(def_site) => at = def_site,
                None => break,
            }
            continue;
        }
        match map.block {
            Some(BlockInfo { parent, .. }) if at == map.root => at = parent,
            _ => break,
        }
    }
    if let Some((prelude, _)) = module.def_map(db).prelude() {
        prelude.def_map(db).module(db, prelude).traits_in_scope(db).for_each(&mut f);
    }
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
/// [`Hygienic::At`] names. Only a plain path's first segment is looked up by key; a `self::`,
/// `super::`, `crate::` or 2015 `::` path, whose keyword named that module, goes on there as
/// `self::` with the same segments.
pub(crate) fn keyed_path(path: &ModPath, key: SyntaxContext) -> ModPath {
    match (path.kind, path.segments().split_first()) {
        (PathKind::Plain, Some((first, rest))) if !key.is_root() => ModPath::from_segments_in(
            PathKind::Plain,
            std::iter::once(first.keyed(key))
                .chain(rest.iter().copied())
                .enumerate()
                .map(|(i, name)| (name, path.context_of(i, ROOT))),
        ),
        (PathKind::Plain, _) => *path,
        _ => ModPath::from_segments_in(
            PathKind::SELF,
            path.segments().iter().enumerate().map(|(i, &name)| (name, path.context_of(i, ROOT))),
        ),
    }
}

/// The key `name`, the name `def` is declared with, binds in its module's scope: keyed by the
/// context it is written with ([`Name::keyed`]). A builtin type is not keyed.
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
        // A variant's id holds its keyed name ([`variant_name`]).
        ModuleDefId::EnumVariantId(it) => it.lookup(db).name.key(),
        ModuleDefId::BuiltinType(_) => None,
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
    normalized_key(
        db,
        name_range(node).map_or(ROOT, |range| file.span_map(db).span_for_range(range).ctx),
    )
}

/// The key of the name the `idx`th use tree of `use_`, an item of `call`'s expansion, binds:
/// as [`expanded_item_key`], a query of its own.
#[blaze_query::tracked(returns(copy))]
fn use_tree_key(db: &Db, call: MacroCallId, use_: UseId, idx: u32) -> Option<SyntaxContext> {
    let file = HirFileId::from(call);
    let trees = use_trees_view(db, use_.lookup(db).id);
    let tree = trees[Idx::from_raw(la_arena::RawIdx::from_u32(idx))];
    normalized_key(
        db,
        name_range(view::AstNode::syntax(&tree))
            .map_or(ROOT, |range| file.span_map(db).span_for_range(range).ctx),
    )
}

/// The context, normalized to macros 2.0, the path of the import `import` is written with: its
/// first token's, the one rustc's `resolve_path` reads for its first segment (`self`, `super`,
/// `crate`, `::` or a name, or the `{{root}}` a 2015 path is given from it). A root context in a
/// real file.
pub(crate) fn use_path_context(db: &Db, import: ImportId) -> SyntaxContext {
    let loc = import.use_.lookup(db);
    match loc.id.file_id.macro_file() {
        None => ROOT,
        Some(call) => {
            expanded_use_path_context(db, call, import.use_, import.idx.into_raw().into_u32())
        }
    }
}

/// [`use_path_context`] of a use tree of `call`'s expansion: a query of its own, as
/// [`expanded_item_key`]. The path of a nested tree starts in the outermost tree around it that
/// has a path or a leading `::`.
#[blaze_query::tracked(returns(copy))]
fn expanded_use_path_context(db: &Db, call: MacroCallId, use_: UseId, idx: u32) -> SyntaxContext {
    let file = HirFileId::from(call);
    let trees = use_trees_view(db, use_.lookup(db).id);
    let mut tree = trees[Idx::from_raw(la_arena::RawIdx::from_u32(idx))];
    let mut start = view::AstNode::syntax(&tree).text_range();
    // Each turn moves to the tree around this one, ending at the `use` item's own tree.
    while let Some(list) = tree.parent_use_tree_list() {
        tree = list.parent_use_tree();
        if tree.path().is_some() || tree.coloncolon_token().is_some() {
            start = view::AstNode::syntax(&tree).text_range();
        }
    }
    file.span_map(db).span_for_range(start).ctx.normalize_to_macros_2_0(db)
}

/// [`DefMap::module_key`] of a module declared at `origin`.
pub(crate) fn origin_key(db: &Db, origin: &ModuleOrigin) -> Option<SyntaxContext> {
    let declaration = origin.declaration()?;
    let call = declaration.file_id.macro_file()?;
    module_item_key(db, call, declaration.value.erase())
}

/// [`DefMap::module_key`] of the `mod` item `ast` of `call`'s expansion: the context of its
/// `mod` keyword. A query of its own, as [`expanded_item_key`].
#[blaze_query::tracked(returns(copy))]
fn module_item_key(db: &Db, call: MacroCallId, ast: ErasedFileAstId) -> Option<SyntaxContext> {
    let file = HirFileId::from(call);
    let node = view::to_node(file.ast_id_map(db).get_erased(ast), file.root(db));
    let range = match view::Module::cast(node).and_then(|it| it.mod_token()) {
        Some(keyword) => keyword.text_range(),
        None => node.text_range(),
    };
    normalized_key(db, file.span_map(db).span_for_range(range).ctx)
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
