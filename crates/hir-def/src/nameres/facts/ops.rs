//! What collecting a module does, as data: one [`DeclOp`] per mutation the old `ModCollector`
//! made of its `DefCollector`, in the order it made them.
//!
//! A module's [`OpLog`] is the value of its declaration query (round 0) or of one of its macro
//! steps (later rounds). A [`super::unit::Unit`] replays the ops whose target is one of its
//! members, which leaves each member's scope as the old collector left it after the same calls;
//! the driver reads the crate-level ones. Nothing here borrows an item tree, so a log is a plain
//! query value: an import keeps its path and raw visibility, an attribute directive its tree.

use std::ops::Range;

use either::Either;
use hir_expand::{
    ExpandTo, MacroCallId, MacroDefId,
    attrs::{Attr, AttrId},
    builtin::BuiltinDeriveExpander,
    mod_path::ModPath,
    name::Name,
};
use la_arena::Idx;
use smallvec::SmallVec;
use span::SyntaxContext;
use syntax::ast;

use super::FrameVec;
use crate::{
    AstId, AstIdWithPath, BuiltinDeriveImplId, ConstId, ExternBlockId, ExternCrateId, FunctionId,
    ImplId, ItemContainerId, MacroId, ModuleDefId, ModuleId, ProcMacroId, UseId,
    item_scope::ImportOrExternCrate,
    item_tree::{ImportAlias, ImportKind, ModItemId, TreeId},
    nameres::{
        ModuleOrigin, diagnostics::DefDiagnostic, mod_resolution::ModDir, uses::DeriveHelperUse,
    },
    per_ns::PerNs,
    visibility::{RawVisibility, Visibility},
};

/// Where an import comes from: its use tree, its `use` item, whether it is the
/// `#[prelude_import]`, and its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImportSource {
    pub(crate) use_tree: Idx<ast::UseTree>,
    pub(crate) id: UseId,
    pub(crate) is_prelude: bool,
    pub(crate) kind: ImportKind,
}

/// One use tree of a `use` item of `module`: the collector's `ImportDirective`, owned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImportOp {
    pub(crate) module: ModuleId,
    pub(crate) path: ModPath,
    pub(crate) alias: Option<ImportAlias>,
    /// The `use` item's visibility, one for all of its use trees.
    pub(crate) visibility: RawVisibility,
    pub(crate) source: ImportSource,
}

impl ImportOp {
    /// The name a plain import binds, if any (`as _` binds none).
    pub(crate) fn bound_name(&self) -> Option<Name> {
        match &self.alias {
            Some(ImportAlias::Alias(name)) => Some(*name),
            Some(ImportAlias::Underscore) => None,
            None => self.path.segments().last().copied(),
        }
    }
}

/// A macro call left for the fixed point: the collector's `MacroDirective`, owned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MacroOp {
    pub(crate) module: ModuleId,
    pub(crate) depth: u32,
    pub(crate) kind: MacroOpKind,
    pub(crate) container: ItemContainerId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MacroOpKind {
    FnLike {
        ast_id: AstIdWithPath<ast::MacroCall>,
        expand_to: ExpandTo,
        ctxt: SyntaxContext,
    },
    Derive {
        ast_id: AstIdWithPath<ast::Adt>,
        derive_attr: AttrId,
        derive_pos: usize,
        ctxt: SyntaxContext,
        /// The `#[derive]` call this derive is one argument of.
        derive_macro_id: MacroCallId,
    },
    Attr {
        ast_id: AstIdWithPath<ast::Item>,
        attr_id: AttrId,
        attr: Attr,
        mod_item: ModItemId,
        /// The tree `mod_item` is in; its item tree is `tree.item_tree(db, krate)`.
        tree: TreeId,
        /// What the item's collection, after the attribute, reads of it
        /// (`item_tree::deferred_item_shape`): none of it is in the log otherwise.
        shape: u128,
    },
}

/// A builtin derive recorded instead of expanded, waiting for the item it is on to be
/// collected again: the collector's `DeferredBuiltinDerive`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeferredDerive {
    pub(crate) call_id: MacroCallId,
    pub(crate) derive: BuiltinDeriveExpander,
    pub(crate) module_id: ModuleId,
    pub(crate) depth: u32,
    pub(crate) container: ItemContainerId,
    pub(crate) derive_attr_id: AttrId,
    pub(crate) derive_index: u32,
    /// Its helper attributes' place in the item's `derive_helpers_in_scope` list.
    pub(crate) helpers_range: Range<usize>,
}

/// A derive helper in scope for an item: its name, the derive macro, and the derive's call.
pub(crate) type DeriveHelper = (Name, MacroId, Either<MacroCallId, BuiltinDeriveImplId>);

/// The `macro_rules!` a module starts with: its parent's textual scope where it is declared,
/// every definition of each name in definition order.
pub(crate) type Inherited = Box<[(Name, Box<[MacroId]>)]>;

/// One mutation. Ops naming a `module` change that module's state; the rest are crate-level
/// (the root's extern prelude, `macro_use` prelude, proc-macro tables) or side tables.
///
/// Not `Clone`: a log is built once and read in place ([`DeclOp::clone_root_op`] copies the
/// few ops another module's log re-states).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeclOp {
    // A module comes to exist.
    /// The module's own row: first op of its round-0 log, or, for one an expansion declares,
    /// right after its parent's `PushChild`.
    Init {
        module: ModuleId,
        origin: ModuleOrigin,
        visibility: Visibility,
        parent: Option<ModuleId>,
        inherited: Inherited,
    },
    /// The round-0 subtree of `child` (declared by the op before) defines, in the crate root,
    /// the ops of [`super::queries::subtree_macro_exports`] of `key`, at this point of the
    /// root's order. Only the root's unit reads it.
    Splice { child: ModuleId, key: super::queries::DeclKey },
    /// `parent` has the child `child` named `name` (`children_mut().insert`). Replacing one wakes
    /// every import, as a path can read whether a module is its parent's test harness.
    PushChild { parent: ModuleId, name: Name, child: ModuleId },
    /// The directory `mod` declarations of `module` resolve in (`mod_dirs.insert`).
    SetModDir { module: ModuleId, dir: ModDir },

    // The scope of `module`, one op per `ItemScope` method.
    Declare { module: ModuleId, def: ModuleDefId },
    /// `DefCollector::update`: push each resolution with `vis`, then propagate it to the
    /// module's glob importers.
    Update {
        module: ModuleId,
        resolutions: Box<[(Option<Name>, PerNs)]>,
        vis: Visibility,
        import: Option<ImportOrExternCrate>,
    },
    DefineImpl { module: ModuleId, imp: ImplId, is_trait_impl: bool, is_reservation: bool },
    DefineBuiltinDeriveImpl { module: ModuleId, imp: BuiltinDeriveImplId },
    DefineExternBlock { module: ModuleId, id: ExternBlockId },
    DefineExternCrateDecl { module: ModuleId, id: ExternCrateId },
    DefineUseDecl { module: ModuleId, id: UseId },
    DefineUnnamedConst { module: ModuleId, id: ConstId },
    /// Defines `mac` in the textual scope of `module` and wakes the imports that read `name` there.
    DefineLegacyMacro { module: ModuleId, name: Name, mac: MacroId },
    AddMacroInvoc { module: ModuleId, call: AstId<ast::MacroCall>, id: MacroCallId },
    AddAttrMacroInvoc { module: ModuleId, item: AstId<ast::Item>, id: MacroCallId },
    SetDeriveMacroInvoc {
        module: ModuleId,
        adt: AstId<ast::Adt>,
        call: Either<MacroCallId, BuiltinDeriveImplId>,
        attr: AttrId,
        idx: usize,
    },
    InitDeriveAttribute {
        module: ModuleId,
        adt: AstId<ast::Adt>,
        attr: AttrId,
        attr_call: MacroCallId,
        derive_calls: SmallVec<[Option<Either<MacroCallId, BuiltinDeriveImplId>>; 4]>,
    },

    // Directives.
    Import(ImportOp),
    Macro(MacroOp),

    // Crate-level, made by the root's collection only.
    /// The std prelude `inject_prelude` found (the `#[prelude_import]` one is the unit's).
    Prelude { module: ModuleId, import: Option<UseId> },
    ExternPrelude { name: Name, root: ModuleId, extern_crate: Option<ExternCrateId> },
    UnresolvedExternCrate { name: Name },
    MacroUsePrelude { name: Name, mac: MacroId, extern_crate: Option<ExternCrateId> },
    ExportedDerive { mac: MacroId, helpers: Box<[Name]> },
    ProcMacroFn { func: FunctionId, mac: ProcMacroId },

    /// An `extern crate` of the root naming a crate the extern prelude has from the
    /// dependencies anyway (the old `not_introducing`).
    NotIntroducing(ExternCrateId),

    // The def map's own tables. The per-item ones (derive helpers, skipped and active
    // attributes, deferred derives) are a module's `SideTables`, carried by its macro steps.
    Diagnostic(DefDiagnostic),
    /// A macro path that resolved, for `NameRecords` (the old `resolved_macro_paths`):
    /// `bang` is `MacroSubNs::Bang`, else `MacroSubNs::Attr`.
    ResolvedMacroPath {
        module: ModuleId,
        path: ModPath,
        bang: bool,
        def: MacroDefId,
        call: Option<MacroCallId>,
    },
    /// A derive helper attribute that resolved (the old `derive_helper_uses`).
    DeriveHelperUse(DeriveHelperUse),
    /// A macro with compilation-local state ran.
    SessionMacros,
}

impl DeclOp {
    /// The module whose state the op changes, `None` for a crate-level or side-table op.
    pub(crate) fn target(&self) -> Option<ModuleId> {
        match self {
            DeclOp::Init { module, .. }
            | DeclOp::SetModDir { module, .. }
            | DeclOp::Declare { module, .. }
            | DeclOp::Update { module, .. }
            | DeclOp::DefineImpl { module, .. }
            | DeclOp::DefineBuiltinDeriveImpl { module, .. }
            | DeclOp::DefineExternBlock { module, .. }
            | DeclOp::DefineExternCrateDecl { module, .. }
            | DeclOp::DefineUseDecl { module, .. }
            | DeclOp::DefineUnnamedConst { module, .. }
            | DeclOp::DefineLegacyMacro { module, .. }
            | DeclOp::AddMacroInvoc { module, .. }
            | DeclOp::AddAttrMacroInvoc { module, .. }
            | DeclOp::SetDeriveMacroInvoc { module, .. }
            | DeclOp::InitDeriveAttribute { module, .. } => Some(*module),
            DeclOp::PushChild { parent, .. } => Some(*parent),
            DeclOp::Import(it) => Some(it.module),
            DeclOp::Macro(it) => Some(it.module),
            DeclOp::Splice { .. }
            | DeclOp::Prelude { .. }
            | DeclOp::ExternPrelude { .. }
            | DeclOp::UnresolvedExternCrate { .. }
            | DeclOp::MacroUsePrelude { .. }
            | DeclOp::ExportedDerive { .. }
            | DeclOp::ProcMacroFn { .. }
            | DeclOp::NotIntroducing(_)
            | DeclOp::Diagnostic(_)
            | DeclOp::ResolvedMacroPath { .. }
            | DeclOp::DeriveHelperUse(_)
            | DeclOp::SessionMacros => None,
        }
    }

    /// A copy of a resolution op ([`Self::is_resolution_op`]), `None` for any other: what
    /// `module_round` hands from the step that made it to the module that gains it.
    pub(crate) fn clone_resolution_op(&self) -> Option<DeclOp> {
        Some(match self {
            DeclOp::Init { module, origin, visibility, parent, inherited } => DeclOp::Init {
                module: *module,
                origin: *origin,
                visibility: *visibility,
                parent: *parent,
                // clone: the op module_round hands to the module that gains it, once per op and round
                inherited: inherited.clone(),
            },
            &DeclOp::PushChild { parent, name, child } => DeclOp::PushChild { parent, name, child },
            &DeclOp::Declare { module, def } => DeclOp::Declare { module, def },
            DeclOp::Update { module, resolutions, vis, import } => DeclOp::Update {
                module: *module,
                // clone: the op module_round hands to the module that gains it, once per op and round
                resolutions: resolutions.clone(),
                vis: *vis,
                import: *import,
            },
            &DeclOp::DefineImpl { module, imp, is_trait_impl, is_reservation } => {
                DeclOp::DefineImpl { module, imp, is_trait_impl, is_reservation }
            }
            &DeclOp::DefineBuiltinDeriveImpl { module, imp } => {
                DeclOp::DefineBuiltinDeriveImpl { module, imp }
            }
            &DeclOp::DefineExternBlock { module, id } => DeclOp::DefineExternBlock { module, id },
            &DeclOp::DefineExternCrateDecl { module, id } => {
                DeclOp::DefineExternCrateDecl { module, id }
            }
            &DeclOp::DefineUseDecl { module, id } => DeclOp::DefineUseDecl { module, id },
            &DeclOp::DefineUnnamedConst { module, id } => DeclOp::DefineUnnamedConst { module, id },
            &DeclOp::DefineLegacyMacro { module, name, mac } => {
                DeclOp::DefineLegacyMacro { module, name, mac }
            }
            &DeclOp::AddMacroInvoc { module, call, id } => DeclOp::AddMacroInvoc { module, call, id },
            &DeclOp::AddAttrMacroInvoc { module, item, id } => {
                DeclOp::AddAttrMacroInvoc { module, item, id }
            }
            &DeclOp::SetDeriveMacroInvoc { module, adt, call, attr, idx } => {
                DeclOp::SetDeriveMacroInvoc { module, adt, call, attr, idx }
            }
            DeclOp::InitDeriveAttribute { module, adt, attr, attr_call, derive_calls } => {
                DeclOp::InitDeriveAttribute {
                    module: *module,
                    adt: *adt,
                    attr: *attr,
                    attr_call: *attr_call,
                    // clone: the op module_round hands to the module that gains it, once per op and round
                    derive_calls: derive_calls.clone(),
                }
            }
            // clone: the op module_round hands to the module that gains it, once per op and round
            DeclOp::Import(it) => DeclOp::Import(it.clone()),
            DeclOp::Macro(_)
            | DeclOp::Splice { .. }
            | DeclOp::SetModDir { .. }
            | DeclOp::Prelude { .. }
            | DeclOp::ExternPrelude { .. }
            | DeclOp::UnresolvedExternCrate { .. }
            | DeclOp::MacroUsePrelude { .. }
            | DeclOp::ExportedDerive { .. }
            | DeclOp::ProcMacroFn { .. }
            | DeclOp::NotIntroducing(_)
            | DeclOp::Diagnostic(_)
            | DeclOp::ResolvedMacroPath { .. }
            | DeclOp::DeriveHelperUse(_)
            | DeclOp::SessionMacros => return None,
        })
    }

    /// A copy of a scope op of `root` (what a module's `#[macro_export]` or proc macro does
    /// to the crate root), `None` for any other op.
    pub(crate) fn clone_root_op(&self, root: ModuleId) -> Option<DeclOp> {
        match self {
            &DeclOp::Declare { module, def } if module == root => {
                Some(DeclOp::Declare { module, def })
            }
            DeclOp::Update { module, resolutions, vis, import } if *module == root => {
                Some(DeclOp::Update {
                    module: *module,
                    // clone: a #[macro_export] op copied to the crate root, once per op
                    resolutions: resolutions.clone(),
                    vis: *vis,
                    import: *import,
                })
            }
            _ => None,
        }
    }

    /// Whether a [`super::unit::Unit`] applies the op to its members' state: the scope, tree and
    /// import ops. Macro directives and the side tables belong to the macro steps and the
    /// driver.
    pub(crate) fn is_resolution_op(&self) -> bool {
        !matches!(
            self,
            DeclOp::Macro(_)
                | DeclOp::SetModDir { .. }
                | DeclOp::Splice { .. }
                | DeclOp::ExternPrelude { .. }
                | DeclOp::UnresolvedExternCrate { .. }
                | DeclOp::MacroUsePrelude { .. }
                | DeclOp::ExportedDerive { .. }
                | DeclOp::ProcMacroFn { .. }
                | DeclOp::NotIntroducing(_)
                | DeclOp::Diagnostic(_)
                | DeclOp::ResolvedMacroPath { .. }
                | DeclOp::DeriveHelperUse(_)
                | DeclOp::SessionMacros
        )
    }
}

/// The ops of one collection, in the order they were made, with what a reader checks before it
/// scans them ([`OpLog::has_children`], [`OpLog::writes_elsewhere`]). A producer pushes into a
/// vector of its frame and makes the log once, at its exact size ([`OpLog::from_frame`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct OpLog {
    pub(crate) ops: Box<[DeclOp]>,
    /// The module of the log's first `Init`: in a round-0 log, the module that made it.
    owner: Option<ModuleId>,
    flags: u8,
}

/// A `Splice`: the module has round-0 children, each collected by its own query.
const SPLICE: u8 = 1;
/// A scope op (`Declare`, `Update`) of a module other than the owner as of that op.
const ELSEWHERE: u8 = 2;

impl OpLog {
    pub(crate) fn from_frame(ops: FrameVec<'_, DeclOp>) -> OpLog {
        let mut owner = None;
        let mut flags = 0;
        for op in &ops {
            match op {
                &DeclOp::Init { module, .. } if owner.is_none() => owner = Some(module),
                DeclOp::Splice { .. } => flags |= SPLICE,
                &DeclOp::Declare { module, .. } | &DeclOp::Update { module, .. }
                    if owner != Some(module) =>
                {
                    flags |= ELSEWHERE;
                }
                _ => {}
            }
        }
        // A frame vector's `IntoIter` is `TrustedLen`: one allocation of exactly its length.
        // alloc: the log is the query's value
        let ops = ops.into_iter().collect();
        OpLog { ops, owner, flags }
    }

    /// Whether the log declares a round-0 child (a `Splice`), whose subtree may make ops in the
    /// crate root.
    pub(crate) fn has_children(&self) -> bool {
        self.flags & SPLICE != 0
    }

    /// Whether a scope op names a module other than the log's first `Init`'s: in a module's
    /// round-0 log, a `#[macro_export]` or proc macro made in the crate root. Without one,
    /// [`OpLog::root_ops`] of any root but the owner is empty.
    pub(crate) fn writes_elsewhere(&self) -> bool {
        self.flags & ELSEWHERE != 0
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, DeclOp> {
        self.ops.iter()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// The `#[macro_export]` ops and proc-macro definitions it makes in `root`'s scope, in order:
    /// what the root's own collection sees after collecting the module that made them.
    pub(crate) fn root_ops(&self, root: ModuleId) -> impl Iterator<Item = &DeclOp> + '_ {
        self.ops.iter().filter(move |op| {
            matches!(op, DeclOp::Declare { module, .. } | DeclOp::Update { module, .. } if *module == root)
        })
    }
}
