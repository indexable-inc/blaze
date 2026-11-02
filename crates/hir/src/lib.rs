//! HIR (previously known as descriptors) provides a high-level object-oriented
//! access to Rust code.
//!
//! The principal difference between HIR and syntax trees is that HIR is bound
//! to a particular crate instance. That is, it has cfg flags and features
//! applied. So, the relation between syntax and HIR is many-to-one.
//!
//! HIR is the public API of the all of the compiler logic above syntax trees.
//! It is written in "OO" style. Each type is self contained (as in, it knows its
//! parents and full context). It should be "clean code".
//!
//! `hir_*` crates are the implementation of the compiler logic.
//! They are written in "ECS" style, with relatively little abstractions.
//! Many types are not self-contained, and explicitly use local indexes, arenas, etc.
//!
//! `hir` is what insulates the "we don't know how to actually write an incremental compiler"
//! from the ide with completions, hovers, etc. It is a (soft, internal) boundary:
//! <https://www.tedinski.com/2018/02/06/system-boundaries.html>.

#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![feature(allocator_api, allocator_ext)]
#![recursion_limit = "512"]

extern crate rustc_type_ir;

mod attrs;
mod body_analysis;
mod from_id;
mod match_lints;
mod named;
mod semantics;
mod source_analyzer;

pub mod check;
pub mod db;
pub mod diagnostics;
pub mod evict;

mod display;
mod scratch;

#[cfg(test)]
mod tests;
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

#[doc(hidden)]
pub use hir_def::ModuleId;

use std::{
    borrow::Borrow,
    fmt, iter,
    ops::{ControlFlow, Not},
};

use arrayvec::ArrayVec;
use base_db::{CrateDisplayName, CrateOrigin, Db, LangCrateOrigin, TargetKind, all_crates};
use blaze_hash::{HashMap, HashSet};
use blaze_hashcons::Interned;
use either::Either;
use hir_def::{
    AdtId, AssocItemId, AssocItemLoc, BuiltinDeriveImplId, CallableDefId, ConstId, ConstParamId,
    DefWithBodyId, EnumId, EnumVariantId, ExpressionStoreOwnerId, ExternBlockId, ExternCrateId,
    FunctionId, GenericDefId, HasModule, ImplId, ItemContainerId, LifetimeParamId, LocalFieldId,
    Lookup, MacroExpander, MacroId, StaticId, StructId, TupleId, TypeAliasId, TypeOrConstParamId,
    TypeParamId, UnionId,
    attrs::AttrFlags,
    builtin_derive::BuiltinDeriveImplMethod,
    expr_store::ExpressionStore,
    hir::{
        BindingAnnotation, BindingId, Expr, ExprId, ExprOrPatId, LabelId, Pat,
        generics::{GenericParams, LifetimeParamData, TypeOrConstParamData, TypeParamProvenance},
    },
    item_tree::ImportAlias,
    lang_item::LangItemTarget,
    layout::{self, ReprOptions, TargetDataLayout},
    per_ns::PerNs,
    resolver::{HasResolver, Resolver},
    signatures::{
        ConstSignature, EnumSignature, FunctionSignature, ImplFlags, ImplSignature, StaticFlags,
        StaticSignature, StructFlags, StructSignature, TraitFlags, TraitSignature,
        TypeAliasSignature, UnionSignature, VariantFields,
    },
    src::HasSource as _,
    unstable_features::UnstableFeatures,
    visibility::visibility_from_ast_view,
};
use hir_expand::{builtin::BuiltinDeriveExpander, proc_macro::ProcMacroKind};
use hir_ty::{
    GenericPredicates, InferBodyId, InferenceResult, ParamEnvAndCrate, TyDefId, ValueTyDefId,
    all_super_traits, autoderef, check_orphan_rules,
    consteval::try_const_usize,
    db::{
        AnonConstId, InternedClosure, InternedClosureId, InternedCoroutineClosureId,
        InternedCoroutineId,
    },
    direct_super_traits,
    layout::{
        InternedLayout, Layout as TyLayout, RustcEnumVariantIdx, RustcFieldIdx, TagEncoding,
        intern_layout,
    },
    method_resolution::{self, InherentImpls, MethodResolutionContext, TraitSet, TraitsInScope},
    next_solver::{
        AliasTy, AnyImplId, ClauseKind, DbInterner, EarlyBinder, ErrorGuaranteed, FnSig,
        GenericArg, GenericArgs, ParamEnv, PolyFnSig, Region, RegionExt as _, SolverDefId, Ty,
        TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
    traits::{self, structurally_normalize_ty},
};
use hir_ty_mir::{
    mir::interpret_mir,
    mir_pretty::{MirBodyPretty as _, MirEvalErrorPretty as _},
};
use itertools::Itertools;
use rustc_type_ir::{
    AliasTyKind, TypeSuperVisitable, TypeVisitable, TypeVisitableExt, TypeVisitor, fast_reject,
    inherent::{AdtDef as _, GenericArgs as _, IntoKind, SliceLike, Term as _, Ty as _},
};
use span::{AstIdNode, Edition, FileId};
use stdx::{format_to, impl_from, never};
use syntax::{
    AstNode, AstPtr, SmolStr, SyntaxNode, TextRange, ToSmolStr,
    ast::{self, HasName as _},
    format_smolstr,
    view::{HasName as _, HasVisibility as _},
};

use crate::db::HirDatabase;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredicateEvaluationStatus {
    Holds,
    NotProven,
    Invalid,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredicateEvaluationResult {
    pub status: PredicateEvaluationStatus,
    pub message: String,
}

impl PredicateEvaluationResult {
    pub fn holds(message: impl Into<String>) -> Self {
        Self { status: PredicateEvaluationStatus::Holds, message: message.into() }
    }

    pub fn not_proven(message: impl Into<String>) -> Self {
        Self { status: PredicateEvaluationStatus::NotProven, message: message.into() }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self { status: PredicateEvaluationStatus::Invalid, message: message.into() }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self { status: PredicateEvaluationStatus::Unsupported, message: message.into() }
    }
}

pub use crate::{
    attrs::{AttrsWithOwner, HasAttrs, resolve_doc_path_on},
    body_analysis::BodyAnalysis,
    diagnostics::*,
    match_lints::{
        BindingOrigin, BindingWithVariantName, ConstPatternError, IrrefutableLet, LetSource,
        ManualPartialEq, MatchLint, NonContiguousRangeEndpoints, OverlappingRangeEndpoints,
        PatternTypo, SpecialTy, UnreachablePattern,
    },
    named::named_dependencies,
    semantics::{
        LintAttr, PathResolution, PathResolutionPerNs, SemanticsImpl, SemanticsScope, TypeInfo,
        VisibleTraits,
        view::{self, FieldSourceView, LocalSourceView, ModuleSourceView},
    },
};

// Be careful with these re-exports.
//
// `hir` is the boundary between the compiler and the IDE. It should try hard to
// isolate the compiler from the ide, to allow the two to be refactored
// independently. Re-exporting something from the compiler is the sure way to
// breach the boundary.
//
// Generally, a refactoring which *removes* a name from this list is a good
// idea!
pub use {
    cfg::{CfgAtom, CfgExpr, CfgOptions},
    hir_def::{
        Complete,
        FindPathConfig,
        attrs::{
            Docs, IsInnerDoc,
            stability::{
                DeprecatedSince, Deprecation, DeprecationEntry, RustcVersion, Stability,
                StabilityLevel, StableSince, UnstableReason,
            },
        },
        expr_store::Body,
        find_path::PrefixKind,
        import_map,
        lang_item::{LangItemEnum as LangItem, crate_lang_items},
        nameres::{DefMap, crate_def_map},
        per_ns::Namespace,
        type_ref::{Mutability, TypeRef},
        visibility::Visibility,
        // FIXME: This is here since some queries take it as input that are used
        // outside of hir.
        {GenericParamId, ModuleDefId, TraitId},
    },
    hir_expand::{
        EditionedFileId, ExpandResult, HirFileId, MacroCallId, MacroKind,
        change::ChangeWithProcMacros,
        files::{
            FilePosition, FilePositionWrapper, FileRange, FileRangeWrapper, HirFilePosition,
            HirFileRange, InFile, InFileWrapper, InMacroFile, InRealFile, MacroFilePosition,
            MacroFileRange,
        },
        inert_attr_macro::AttributeTemplate,
        mod_path::{ModPath, PathKind, tool_path},
        name::{self, Name},
        proc_macro::{ProcMacros, ProcMacrosBuilder},
        tt,
    },
    hir_ty::{
        CastError, EditionLint, PointerCast, ReceiverAdjustment, attach_db, attach_db_allow_change,
        consteval::ConstEvalError,
        display::{ClosureStyle, DisplayTarget, HirDisplay, HirDisplayError},
        drop::DropGlue,
        dyn_compatibility::{DynCompatibilityViolation, MethodViolationCode},
        layout::LayoutError,
        mir::{MirEvalError, MirLowerError},
        next_solver::abi::Safety,
        next_solver::clear_tls_solver_cache,
        setup_tracing,
    },
    // FIXME: These are needed for import assets, properly encapsulate them.
    hir_ty::{method_resolution::TraitImpls, next_solver::SimplifiedType},
    // FIXME: Properly encapsulate mir
    hir_ty_mir::mir,
    hir_ty_mir::{const_macro, mir_pretty::ConstEvalErrorPretty},
    hir_ty_wfcheck::SignatureMismatchAt,
    intern::{Symbol, sym},
};

// These are negative re-exports: pub using these names is forbidden, they
// should remain private to hir internals.
#[allow(unused)]
use {
    hir_def::expr_store::path::Path,
    hir_expand::{
        name::AsName,
        span_map::{ExpansionSpanMap, RealSpanMap, SpanMap},
    },
    hir_ty::next_solver,
};

/// hir::Crate describes a single crate. It's the main interface with which
/// a crate's dependencies interact. Mostly, it should be just a proxy for the
/// root module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Crate {
    pub(crate) id: base_db::Crate,
}

#[derive(Debug)]
pub struct CrateDependency {
    pub krate: Crate,
    pub name: Name,
}

impl Crate {
    pub fn base(self) -> base_db::Crate {
        self.id
    }

    pub fn origin(self, db: &Db) -> CrateOrigin {
        // clone: editor edge; hot callers use `lang_origin`
        self.id.data(db).origin.clone()
    }

    /// Which of the language's crates this is, without cloning [`Self::origin`].
    pub fn lang_origin(self, db: &Db) -> Option<LangCrateOrigin> {
        match self.id.data(db).origin {
            CrateOrigin::Lang(origin) => Some(origin),
            _ => None,
        }
    }

    pub fn is_builtin(self, db: &Db) -> bool {
        self.lang_origin(db).is_some()
    }

    /// Whether a primitive type's name can mean something else in this crate: when `false`, a
    /// one-segment path `u8` (`str`, `bool`, ..) written in a module of the crate is the
    /// primitive type, unless a generic parameter or a block scope around it has that name
    /// ([`hir_def::nameres::crate_shadows_primitive_type`]). Computed once per crate.
    pub fn shadows_primitive_type(self, db: &Db) -> bool {
        hir_def::nameres::crate_shadows_primitive_type(db, self.id)
    }

    pub fn dependencies(self, db: &Db) -> Vec<CrateDependency> {
        // alloc: editor edge; hot callers use `dependencies_iter`
        self.dependencies_iter(db).collect()
    }

    /// [`Crate::dependencies`], without the vector.
    pub fn dependencies_iter(self, db: &Db) -> impl ExactSizeIterator<Item = CrateDependency> + '_ {
        self.id.data(db).dependencies.iter().map(|dep| {
            let krate = Crate { id: dep.crate_id };
            let name = dep.as_name();
            CrateDependency { krate, name }
        })
    }

    pub fn reverse_dependencies(self, db: &Db) -> Vec<Crate> {
        let all_crates = all_crates(db);
        all_crates
            .iter()
            .copied()
            .filter(|&krate| krate.data(db).dependencies.iter().any(|it| it.crate_id == self.id))
            .map(|id| Crate { id })
            .collect() // alloc: editor edge, once per request
    }

    pub fn transitive_reverse_dependencies(
        self,
        db: &Db,
    ) -> impl Iterator<Item = Crate> {
        self.id.transitive_rev_deps(db).iter().map(|&id| Crate { id })
    }

    pub fn notable_traits_in_deps(self, db: &Db) -> impl Iterator<Item = &TraitId> {
        self.id
            .transitive_deps(db)
            .iter()
            .filter_map(|&krate| hir_def::crate_notable_traits(db, krate))
            .flatten()
    }

    pub fn root_module(self, db: &Db) -> Module {
        Module { id: crate_def_map(db, self.id).root_module_id() }
    }

    pub fn modules(self, db: &Db) -> Vec<Module> {
        // alloc: editor edge; hot callers use `modules_iter`
        self.modules_iter(db).collect()
    }

    /// [`Crate::modules`], without the vector.
    pub fn modules_iter(self, db: &Db) -> impl Iterator<Item = Module> + '_ {
        crate_def_map(db, self.id).modules(db).map(|(id, _)| id.into())
    }

    pub fn root_file(self, db: &Db) -> FileId {
        self.id.data(db).root_file_id
    }

    /// Whether the root file's `#![cfg(...)]` is false, which leaves the crate empty.
    pub fn is_configured_out(self, db: &Db) -> bool {
        let file = self.id.root_file_id(db);
        hir_def::item_tree::file_item_tree(db, file.into(), self.id).configured_out()
    }

    pub fn edition(self, db: &Db) -> Edition {
        self.id.data(db).edition
    }

    pub fn version(self, db: &Db) -> Option<String> {
        // clone: editor edge, an owned string for display
        self.id.extra_data(db).version.clone()
    }

    pub fn display_name(self, db: &Db) -> Option<CrateDisplayName> {
        // clone: editor edge, an owned name for display
        self.id.extra_data(db).display_name.clone()
    }

    pub fn target_kind(self, db: &Db) -> TargetKind {
        self.id.data(db).target_kind
    }

    /// rustc's `--crate-type`s the crate is built as; empty when the build named none.
    pub fn crate_types(self, db: &Db) -> base_db::CrateTypes {
        self.id.data(db).crate_types
    }

    /// The ADT `#[rustc_diagnostic_item = name]` names among the items this crate sees (rustc's
    /// `get_diagnostic_item`), as `TokenStream` names `proc_macro::TokenStream`.
    pub fn diagnostic_item_adt(self, db: &Db, name: &Symbol) -> Option<Adt> {
        match hir_def::diagnostic_item::diagnostic_items(db, self.id).get(name)? {
            hir_def::AttrDefId::AdtId(adt) => Some(adt.into()),
            _ => None,
        }
    }

    pub fn query_external_importables(
        self,
        db: &Db,
        query: import_map::Query,
    ) -> impl Iterator<Item = (Either<ModuleDef, Macro>, Complete)> {
        let _p = tracing::info_span!("query_external_importables").entered();
        let mut found = blaze_mem::scratch::lend::<crate::scratch::Importables>();
        let mut seen = crate::scratch::take_importables_seen();
        import_map::search_dependencies_each(db, self.into(), &query, |item, complete| {
            if seen.insert((item, complete)) {
                found.push((item, complete));
            }
        });
        crate::scratch::give_importables_seen(seen);
        crate::scratch::LentCopied::new(found).map(
            |(item, do_not_complete)| {
                let item = match ItemInNs::from(item) {
                    ItemInNs::Types(mod_id) | ItemInNs::Values(mod_id) => Either::Left(mod_id),
                    ItemInNs::Macros(mac_id) => Either::Right(mac_id),
                };
                (item, do_not_complete)
            },
        )
    }

    pub fn all(db: &Db) -> Vec<Crate> {
        // alloc: editor edge, once per request
        all_crates(db).iter().map(|&id| Crate { id }).collect()
    }

    /// Try to get the root URL of the documentation of a crate.
    pub fn get_html_root_url(self, db: &Db) -> Option<String> {
        // Look for #![doc(html_root_url = "...")]
        let doc_url = AttrFlags::doc_html_root_url(db, self.id);
        // alloc: editor edge, an owned URL for docs links
        doc_url.as_ref().map(|s| s.trim_matches('"').trim_end_matches('/').to_owned() + "/")
    }

    pub fn cfg<'db>(&self, db: &'db Db) -> &'db CfgOptions {
        self.id.cfg_options(db)
    }

    pub fn potential_cfg<'db>(&self, db: &'db Db) -> &'db CfgOptions {
        let data = self.id.extra_data(db);
        data.potential_cfg_options.as_ref().unwrap_or_else(|| self.id.cfg_options(db))
    }

    pub fn to_display_target(self, db: &Db) -> DisplayTarget {
        DisplayTarget::from_crate(db, self.id)
    }

    fn core(db: &Db) -> Option<Crate> {
        all_crates(db)
            .iter()
            .copied()
            .find(|&krate| {
                matches!(krate.data(db).origin, CrateOrigin::Lang(LangCrateOrigin::Core))
            })
            .map(Crate::from)
    }

    pub fn is_unstable_feature_enabled(self, db: &Db, feature: &Symbol) -> bool {
        UnstableFeatures::query(db, self.id).is_enabled(feature)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Module {
    pub(crate) id: ModuleId,
}

/// The defs which can be visible in the module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleDef {
    Module(Module),
    Function(Function),
    Adt(Adt),
    // Can't be directly declared, but can be imported.
    EnumVariant(EnumVariant),
    Const(Const),
    Static(Static),
    Trait(Trait),
    TypeAlias(TypeAlias),
    BuiltinType(BuiltinType),
    Macro(Macro),
}
impl_from!(
    Module,
    Function,
    Adt(Struct, Enum, Union),
    EnumVariant,
    Const,
    Static,
    Trait,
    TypeAlias,
    BuiltinType,
    Macro
    for ModuleDef
);

impl_from!(
    Variant { Struct => Adt, Union => Adt, EnumVariant => EnumVariant }
    for ModuleDef
);

impl ModuleDef {
    pub fn module(self, db: &Db) -> Option<Module> {
        match self {
            ModuleDef::Module(it) => it.parent(db),
            ModuleDef::Function(it) => Some(it.module(db)),
            ModuleDef::Adt(it) => Some(it.module(db)),
            ModuleDef::EnumVariant(it) => Some(it.module(db)),
            ModuleDef::Const(it) => Some(it.module(db)),
            ModuleDef::Static(it) => Some(it.module(db)),
            ModuleDef::Trait(it) => Some(it.module(db)),
            ModuleDef::TypeAlias(it) => Some(it.module(db)),
            ModuleDef::Macro(it) => Some(it.module(db)),
            ModuleDef::BuiltinType(_) => None,
        }
    }

    pub fn canonical_path(&self, db: &Db, edition: Edition) -> Option<String> {
        let name = self.name(db)?;
        let segments = self.module(db)?.path_segments(db).chain(Some(name));
        // alloc: editor edge, a path rendered for display
        Some(segments.map(|it| it.display(db, edition).to_string()).join("::"))
    }

    pub fn canonical_module_path(
        &self,
        db: &Db,
    ) -> Option<impl Iterator<Item = Module>> {
        self.module(db).map(|it| it.path_to_root(db).into_iter().rev())
    }

    pub fn name(self, db: &Db) -> Option<Name> {
        let name = match self {
            ModuleDef::Module(it) => it.name(db)?,
            ModuleDef::Const(it) => it.name(db)?,
            ModuleDef::Adt(it) => it.name(db),
            ModuleDef::Trait(it) => it.name(db),
            ModuleDef::Function(it) => it.name(db),
            ModuleDef::EnumVariant(it) => it.name(db),
            ModuleDef::TypeAlias(it) => it.name(db),
            ModuleDef::Static(it) => it.name(db),
            ModuleDef::Macro(it) => it.name(db),
            ModuleDef::BuiltinType(it) => it.name(),
        };
        Some(name)
    }

    pub fn as_def_with_body(self) -> Option<DefWithBody> {
        match self {
            ModuleDef::Function(it) => Some(it.into()),
            ModuleDef::Const(it) => Some(it.into()),
            ModuleDef::Static(it) => Some(it.into()),
            ModuleDef::EnumVariant(it) => Some(it.into()),

            ModuleDef::Module(_)
            | ModuleDef::Adt(_)
            | ModuleDef::Trait(_)
            | ModuleDef::TypeAlias(_)
            | ModuleDef::Macro(_)
            | ModuleDef::BuiltinType(_) => None,
        }
    }

    /// Returns only defs that have generics from themselves, not their parent.
    pub fn as_self_generic_def(self) -> Option<GenericDef> {
        match self {
            ModuleDef::Function(it) => Some(it.into()),
            ModuleDef::Adt(it) => Some(it.into()),
            ModuleDef::Trait(it) => Some(it.into()),
            ModuleDef::TypeAlias(it) => Some(it.into()),
            ModuleDef::Module(_)
            | ModuleDef::EnumVariant(_)
            | ModuleDef::Static(_)
            | ModuleDef::Const(_)
            | ModuleDef::BuiltinType(_)
            | ModuleDef::Macro(_) => None,
        }
    }

    pub fn as_generic_def(self) -> Option<GenericDef> {
        match self {
            ModuleDef::Function(it) => Some(it.into()),
            ModuleDef::Adt(it) => Some(it.into()),
            ModuleDef::Trait(it) => Some(it.into()),
            ModuleDef::TypeAlias(it) => Some(it.into()),
            ModuleDef::Static(it) => Some(it.into()),
            ModuleDef::Const(it) => Some(it.into()),
            ModuleDef::EnumVariant(_)
            | ModuleDef::Module(_)
            | ModuleDef::BuiltinType(_)
            | ModuleDef::Macro(_) => None,
        }
    }

    pub fn attrs(&self, db: &Db) -> Option<AttrsWithOwner> {
        Some(match self {
            ModuleDef::Module(it) => it.attrs(db),
            ModuleDef::Function(it) => HasAttrs::attrs(*it, db),
            ModuleDef::Adt(it) => it.attrs(db),
            ModuleDef::EnumVariant(it) => it.attrs(db),
            ModuleDef::Const(it) => it.attrs(db),
            ModuleDef::Static(it) => it.attrs(db),
            ModuleDef::Trait(it) => it.attrs(db),
            ModuleDef::TypeAlias(it) => it.attrs(db),
            ModuleDef::Macro(it) => it.attrs(db),
            ModuleDef::BuiltinType(_) => return None,
        })
    }
}

impl HasCrate for ModuleDef {
    fn krate(&self, db: &Db) -> Crate {
        match self.module(db) {
            Some(module) => module.krate(db),
            None => Crate::core(db).unwrap_or_else(|| all_crates(db)[0].into()),
        }
    }
}

impl HasAttrs for ModuleDef {
    fn attr_id(self, db: &Db) -> attrs::AttrsOwner {
        match self {
            ModuleDef::Module(it) => it.attr_id(db),
            ModuleDef::Function(it) => it.attr_id(db),
            ModuleDef::Adt(it) => it.attr_id(db),
            ModuleDef::EnumVariant(it) => it.attr_id(db),
            ModuleDef::Const(it) => it.attr_id(db),
            ModuleDef::Static(it) => it.attr_id(db),
            ModuleDef::Trait(it) => it.attr_id(db),
            ModuleDef::TypeAlias(it) => it.attr_id(db),
            ModuleDef::Macro(it) => it.attr_id(db),
            ModuleDef::BuiltinType(_) => attrs::AttrsOwner::Dummy,
        }
    }
}

impl HasVisibility for ModuleDef {
    fn visibility(&self, db: &Db) -> Visibility {
        match *self {
            ModuleDef::Module(it) => it.visibility(db),
            ModuleDef::Function(it) => it.visibility(db),
            ModuleDef::Adt(it) => it.visibility(db),
            ModuleDef::Const(it) => it.visibility(db),
            ModuleDef::Static(it) => it.visibility(db),
            ModuleDef::Trait(it) => it.visibility(db),
            ModuleDef::TypeAlias(it) => it.visibility(db),
            ModuleDef::EnumVariant(it) => it.visibility(db),
            ModuleDef::Macro(it) => it.visibility(db),
            ModuleDef::BuiltinType(_) => Visibility::Public,
        }
    }
}

impl Module {
    /// Name of this module.
    pub fn name(self, db: &Db) -> Option<Name> {
        self.id.name(db)
    }

    /// Returns the crate this module is part of.
    pub fn krate(self, db: &Db) -> Crate {
        Crate { id: self.id.krate(db) }
    }

    /// Topmost parent of this module. Every module has a `crate_root`, but some
    /// might be missing `krate`. This can happen if a module's file is not included
    /// in the module tree of any target in `Cargo.toml`.
    pub fn crate_root(self, db: &Db) -> Module {
        let def_map = crate_def_map(db, self.id.krate(db));
        Module { id: def_map.crate_root(db) }
    }

    pub fn is_crate_root(self, db: &Db) -> bool {
        self.crate_root(db) == self
    }

    /// The file that is this module, for a crate root or a `mod m;`; `None` for an inline
    /// module or a block.
    pub fn as_source_file_id(self, db: &Db) -> Option<EditionedFileId> {
        self.id.def_map(db).module(db, self.id).origin.file_id()
    }

    /// The range of this module's items: its file, or for an inline module or a block, its
    /// item list or block.
    pub fn definition_source_range(self, db: &Db) -> InFile<TextRange> {
        self.id.def_map(db).module(db, self.id).definition_source_range(db)
    }

    /// The file this module's items are written in: its own, or for an inline module or a
    /// block, the file holding it.
    pub fn definition_source_file_id(self, db: &Db) -> HirFileId {
        self.id.def_map(db).module(db, self.id).definition_source_file_id()
    }

    /// Whether this module is written in its parent's file: a `mod m {}` or a block.
    pub fn is_inline(self, db: &Db) -> bool {
        self.id.def_map(db).module(db, self.id).origin.is_inline()
    }

    /// The range which declares this module, either a `mod foo;` or a `mod foo {}`. `None` for
    /// the crate root or a block.
    pub fn declaration_source_range(self, db: &Db) -> Option<InFile<TextRange>> {
        self.id.def_map(db).module(db, self.id).declaration_source_range(db)
    }

    /// Iterates over all child modules.
    pub fn children(self, db: &Db) -> impl Iterator<Item = Module> {
        let def_map = self.id.def_map(db);
        def_map.module(db, self.id).children(db).values().map(|module_id| Module { id: *module_id })
    }

    /// Finds a parent module.
    pub fn parent(self, db: &Db) -> Option<Module> {
        let def_map = self.id.def_map(db);
        let parent_id = def_map.containing_module(db, self.id)?;
        Some(Module { id: parent_id })
    }

    /// Finds nearest non-block ancestor `Module` (`self` included).
    pub fn nearest_non_block_module(self, db: &Db) -> Module {
        let mut id = self.id;
        while id.is_block_module(db) {
            id = id.containing_module(db).expect("block without parent module");
        }
        Module { id: id.to_static() }
    }

    pub fn path_to_root(self, db: &Db) -> Vec<Module> {
        // alloc: editor edge, a path as deep as the module nesting
        let mut res = vec![self];
        let mut curr = self;
        while let Some(next) = curr.parent(db) {
            res.push(next);
            curr = next
        }
        res
    }

    /// Names of the modules enclosing `self`, crate root first, `self` last.
    ///
    /// Nameless modules — the crate root, and block modules — drop out, so this is
    /// generally shorter than [`Module::path_to_root`]. Segments stay `Name`s rather
    /// than rendered text because callers disagree on the edition to display with,
    /// and some need to take the path apart rather than print it.
    ///
    /// [`ModuleDef::canonical_module_path`] is the same walk yielding the `Module`s
    /// themselves, for callers that need more than the name — each module's own
    /// edition, say.
    pub fn path_segments(self, db: &Db) -> impl Iterator<Item = Name> {
        self.path_to_root(db).into_iter().rev().filter_map(|it| it.name(db))
    }

    pub fn modules_in_scope(&self, db: &Db, pub_only: bool) -> Vec<(Name, Module)> {
        let def_map = self.id.def_map(db);
        let scope = def_map.module(db, self.id).scope(db);

        let mut res = Vec::new(); // alloc: editor edge, the owned list callers take

        for (name, item) in scope.types() {
            if let ModuleDefId::ModuleId(m) = item.def
                && (!pub_only || item.vis == Visibility::Public)
            {
                res.push((*name, Module { id: m }));
            }
        }

        res
    }

    /// Returns a `ModuleScope`: a set of items, visible in this module.
    pub fn scope(
        self,
        db: &Db,
        visible_from: Option<Module>,
    ) -> Vec<(Name, ScopeDef<'_>)> {
        self.id
            .def_map(db)
            .module(db, self.id)
            .scope(db)
            .entries()
            .filter_map(|(name, def)| {
                if let Some(m) = visible_from {
                    let filtered = def.filter_visibility(|vis| vis.is_visible_from(db, m.id));
                    if filtered.is_none() && !def.is_none() { None } else { Some((name, filtered)) }
                } else {
                    Some((name, def))
                }
            })
            .flat_map(|(name, def)| {
                ScopeDef::all_items(def).into_iter().map(move |item| (*name, item))
            })
            .collect() // alloc: editor edge, the owned scope listing callers take
    }

    pub fn resolve_mod_path(
        &self,
        db: &Db,
        segments: impl IntoIterator<Item = Name>,
    ) -> Option<impl Iterator<Item = ItemInNs>> {
        let items = self
            .id
            .resolver(db)
            .resolve_module_path_in_items(db, &ModPath::from_segments(PathKind::Plain, segments));
        Some(items.iter_items().map(|(item, _)| item.into()))
    }

    /// Fills `acc` with the module's diagnostics.
    pub fn diagnostics<'db>(
        self,
        db: &'db Db,
        acc: &mut Vec<AnyDiagnostic<'db>>,
        style_lints: bool,
    ) {
        let bodies = &crate::check::every_body;
        crate::diagnostics::DiagnosticsCollector::collect(db, self.id, acc, style_lints, bodies);
    }

    /// Fills `acc` with the module's diagnostics, of the bodies only those `bodies` asks for:
    /// those of the items themselves are all there.
    pub fn diagnostics_of_bodies<'db>(
        self,
        db: &'db Db,
        acc: &mut Vec<AnyDiagnostic<'db>>,
        style_lints: bool,
        bodies: &(impl Fn(DefWithBody) -> bool + ?Sized),
    ) {
        crate::diagnostics::DiagnosticsCollector::collect(db, self.id, acc, style_lints, bodies);
    }

    pub fn declarations(self, db: &Db) -> Vec<ModuleDef> {
        // alloc: editor edge; hot callers use `declarations_iter`
        self.declarations_iter(db).collect()
    }

    /// [`Module::declarations`], without the vector.
    pub fn declarations_iter(self, db: &Db) -> impl Iterator<Item = ModuleDef> + '_ {
        let scope = self.id.def_map(db).module(db, self.id).scope(db);
        scope
            .declarations()
            .map(ModuleDef::from)
            .chain(scope.unnamed_consts().map(|id| ModuleDef::Const(Const::from(id))))
    }

    pub fn legacy_macros(self, db: &Db) -> Vec<Macro> {
        // alloc: editor edge; hot callers use `legacy_macros_iter`
        self.legacy_macros_iter(db).collect()
    }

    /// [`Module::legacy_macros`], without the vector.
    pub fn legacy_macros_iter(self, db: &Db) -> impl Iterator<Item = Macro> + '_ {
        let scope = self.id.def_map(db).module(db, self.id).scope(db);
        scope.legacy_macros().flat_map(|(_, it)| it).map(|&it| it.into())
    }

    pub fn extern_blocks(self, db: &Db) -> Vec<ExternBlock> {
        let def_map = self.id.def_map(db);
        let scope = def_map.module(db, self.id).scope(db);
        scope.extern_blocks().map(|id| ExternBlock { id }).collect() // alloc: editor edge
    }

    /// The `use` items written in the module.
    pub fn use_decls(self, db: &Db) -> Vec<UseDecl> {
        let def_map = self.id.def_map(db);
        let scope = def_map.module(db, self.id).scope(db);
        scope.use_decls().map(|id| UseDecl { id }).collect() // alloc: editor edge
    }

    /// The `extern crate` items written in the module.
    pub fn extern_crate_decls(self, db: &Db) -> Vec<ExternCrateDecl> {
        let def_map = self.id.def_map(db);
        let scope = def_map.module(db, self.id).scope(db);
        // alloc: editor edge
        scope.extern_crate_decls().map(|id| ExternCrateDecl { id }).collect()
    }

    pub fn impl_defs(self, db: &Db) -> Vec<Impl> {
        // alloc: editor edge; hot callers use `impl_defs_iter`
        self.impl_defs_iter(db).collect()
    }

    /// [`Module::impl_defs`], without the vector.
    pub fn impl_defs_iter(self, db: &Db) -> impl Iterator<Item = Impl> + '_ {
        let scope = self.id.def_map(db).module(db, self.id).scope(db);
        scope.impls().map(Impl::from).chain(scope.builtin_derive_impls().map(Impl::from))
    }

    /// Finds a path that can be used to refer to the given item from within
    /// this module, if possible.
    pub fn find_path(
        self,
        db: &Db,
        item: impl Into<ItemInNs>,
        cfg: FindPathConfig,
    ) -> Option<ModPath> {
        hir_def::find_path::find_path(
            db,
            item.into().try_into().ok()?,
            self.into(),
            PrefixKind::Plain,
            false,
            cfg,
        )
    }

    /// Finds a path that can be used to refer to the given item from within
    /// this module, if possible. This is used for returning import paths for use-statements.
    pub fn find_use_path(
        self,
        db: &Db,
        item: impl Into<ItemInNs>,
        prefix_kind: PrefixKind,
        cfg: FindPathConfig,
    ) -> Option<ModPath> {
        hir_def::find_path::find_path(
            db,
            item.into().try_into().ok()?,
            self.into(),
            prefix_kind,
            true,
            cfg,
        )
    }

    #[inline]
    pub fn doc_keyword(self, db: &Db) -> Option<Symbol> {
        AttrFlags::doc_keyword(db, self.id)
    }

    /// Whether it has `#[path = "..."]` attribute.
    #[inline]
    pub fn has_path(&self, db: &Db) -> bool {
        self.attrs(db).attrs.contains(AttrFlags::HAS_PATH)
    }
}

impl HasVisibility for Module {
    fn visibility(&self, db: &Db) -> Visibility {
        let def_map = self.id.def_map(db);
        let module_data = &def_map.module(db, self.id);
        module_data.visibility
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Field {
    pub(crate) parent: Variant,
    pub(crate) id: LocalFieldId,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone, Hash)]
pub struct TupleField<'db> {
    pub owner: InferBodyId<'db>,
    pub tuple: TupleId,
    pub index: u32,
}

impl<'db> TupleField<'db> {
    pub fn name(&self) -> Name {
        Name::new_tuple_field(self.index as usize)
    }

    pub fn ty(&self, db: &'db Db) -> Type<'db> {
        let interner = DbInterner::new_no_crate(db);
        let ty = InferenceResult::of(db, self.owner)
            .tuple_field_access_type(self.tuple)
            .as_slice()
            .get(self.index as usize)
            .copied()
            .unwrap_or_else(|| Ty::new_error(interner, ErrorGuaranteed));
        Type::new_body(db, self.owner.expression_store_owner(db), ty)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FieldSource {
    Named(ast::RecordField),
    Pos(ast::TupleField),
}

impl AstNode for FieldSource {
    fn can_cast(kind: syntax::SyntaxKind) -> bool
    where
        Self: Sized,
    {
        ast::RecordField::can_cast(kind) || ast::TupleField::can_cast(kind)
    }

    fn cast(syntax: SyntaxNode) -> Option<Self>
    where
        Self: Sized,
    {
        if ast::RecordField::can_cast(syntax.kind()) {
            <ast::RecordField as AstNode>::cast(syntax).map(FieldSource::Named)
        } else if ast::TupleField::can_cast(syntax.kind()) {
            <ast::TupleField as AstNode>::cast(syntax).map(FieldSource::Pos)
        } else {
            None
        }
    }

    fn syntax(&self) -> &SyntaxNode {
        match self {
            FieldSource::Named(it) => it.syntax(),
            FieldSource::Pos(it) => it.syntax(),
        }
    }
}

impl Field {
    pub fn name(&self, db: &Db) -> Name {
        VariantId::from(self.parent).fields(db).fields()[self.id].name
    }

    pub fn index(&self) -> usize {
        u32::from(self.id.into_raw()) as usize
    }

    /// Returns the type as in the signature of the struct. Only use this in the
    /// context of the field definition.
    pub fn ty<'db>(&self, db: &'db Db) -> Type<'db> {
        let var_id = self.parent.into();
        let ty = db.field_types(var_id)[self.id].ty().instantiate_identity().skip_norm_wip();
        Type::new(db, var_id.adt_id(db).into(), ty)
    }

    pub fn layout<'db>(&self, db: &'db Db) -> Result<Layout<'db>, LayoutError> {
        self.ty(db).layout(db)
    }

    pub fn parent_def(&self, _db: &Db) -> Variant {
        self.parent
    }
}

impl HasVisibility for Field {
    fn visibility(&self, db: &Db) -> Visibility {
        let variant_data = VariantId::from(self.parent).fields(db);
        let visibility = &variant_data.fields()[self.id].visibility;
        let parent_id: hir_def::VariantId = self.parent.into();
        // FIXME: RawVisibility::Public doesn't need to construct a resolver
        Visibility::resolve(db, &parent_id.resolver(db), visibility)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Struct {
    pub(crate) id: StructId,
}

impl Struct {
    pub fn module(self, db: &Db) -> Module {
        Module { id: self.id.lookup(db).container }
    }

    pub fn name(self, db: &Db) -> Name {
        StructSignature::of(db, self.id).name
    }

    pub fn fields(self, db: &Db) -> Vec<Field> {
        // alloc: editor edge; hot callers use `fields_iter`
        self.fields_iter(db).collect()
    }

    /// [`Self::fields`], without the vector.
    pub fn fields_iter(self, db: &Db) -> impl ExactSizeIterator<Item = Field> + '_ {
        self.id.fields(db).fields().iter().map(move |(id, _)| Field { parent: self.into(), id })
    }

    pub fn ty(self, db: &Db) -> Type<'_> {
        Type::from_def(db, self.id)
    }

    pub fn constructor_ty(self, db: &Db) -> Type<'_> {
        Type::from_value_def(db, self.id)
    }

    pub fn repr(self, db: &Db) -> Option<ReprOptions> {
        AttrFlags::repr(db, self.id.into())
    }

    pub fn kind(self, db: &Db) -> StructKind {
        match self.variant_fields(db).shape {
            hir_def::item_tree::FieldsShape::Record => StructKind::Record,
            hir_def::item_tree::FieldsShape::Tuple => StructKind::Tuple,
            hir_def::item_tree::FieldsShape::Unit => StructKind::Unit,
        }
    }

    fn variant_fields(self, db: &Db) -> &VariantFields {
        self.id.fields(db)
    }

    pub fn is_unstable(self, db: &Db) -> bool {
        AttrFlags::query(db, self.id.into()).contains(AttrFlags::IS_UNSTABLE)
    }
}

impl HasVisibility for Struct {
    fn visibility(&self, db: &Db) -> Visibility {
        let loc = self.id.lookup(db);
        let source = loc.source_view(db);
        visibility_from_ast_view(db, self.id, source.map(|src| src.visibility()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Union {
    pub(crate) id: UnionId,
}

impl Union {
    pub fn name(self, db: &Db) -> Name {
        UnionSignature::of(db, self.id).name
    }

    pub fn module(self, db: &Db) -> Module {
        Module { id: self.id.lookup(db).container }
    }

    pub fn ty(self, db: &Db) -> Type<'_> {
        Type::from_def(db, self.id)
    }

    pub fn constructor_ty(self, db: &Db) -> Type<'_> {
        Type::from_value_def(db, self.id)
    }

    pub fn kind(self, db: &Db) -> StructKind {
        match self.id.fields(db).shape {
            hir_def::item_tree::FieldsShape::Record => StructKind::Record,
            hir_def::item_tree::FieldsShape::Tuple => StructKind::Tuple,
            hir_def::item_tree::FieldsShape::Unit => StructKind::Unit,
        }
    }

    pub fn fields(self, db: &Db) -> Vec<Field> {
        // alloc: editor edge; hot callers use `fields_iter`
        self.fields_iter(db).collect()
    }

    /// [`Self::fields`], without the vector.
    pub fn fields_iter(self, db: &Db) -> impl ExactSizeIterator<Item = Field> + '_ {
        self.id.fields(db).fields().iter().map(move |(id, _)| Field { parent: self.into(), id })
    }
    pub fn is_unstable(self, db: &Db) -> bool {
        AttrFlags::query(db, self.id.into()).contains(AttrFlags::IS_UNSTABLE)
    }
}

impl HasVisibility for Union {
    fn visibility(&self, db: &Db) -> Visibility {
        let loc = self.id.lookup(db);
        let source = loc.source_view(db);
        visibility_from_ast_view(db, self.id, source.map(|src| src.visibility()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Enum {
    pub(crate) id: EnumId,
}

impl Enum {
    pub fn module(self, db: &Db) -> Module {
        Module { id: self.id.lookup(db).container }
    }

    pub fn name(self, db: &Db) -> Name {
        EnumSignature::of(db, self.id).name
    }

    pub fn variants(self, db: &Db) -> Vec<EnumVariant> {
        // alloc: editor edge; hot callers use `variants_iter`
        self.variants_iter(db).collect()
    }

    /// [`Enum::variants`], without the vector.
    pub fn variants_iter(self, db: &Db) -> impl ExactSizeIterator<Item = EnumVariant> + '_ {
        self.id.enum_variants(db).variants.values().map(|&(id, _)| EnumVariant { id })
    }

    pub fn num_variants(self, db: &Db) -> usize {
        self.id.enum_variants(db).variants.len()
    }

    pub fn repr(self, db: &Db) -> Option<ReprOptions> {
        AttrFlags::repr(db, self.id.into())
    }

    pub fn ty<'db>(self, db: &'db Db) -> Type<'db> {
        Type::from_def(db, self.id)
    }

    /// The type of the enum variant bodies.
    pub fn variant_body_ty<'db>(self, db: &'db Db) -> Type<'db> {
        let interner = DbInterner::new_no_crate(db);
        Type::no_params(
            db,
            Type::builtin_type_crate(db),
            match EnumSignature::variant_body_type(db, self.id) {
                layout::IntegerType::Pointer(sign) => match sign {
                    true => Ty::new_int(interner, rustc_type_ir::IntTy::Isize),
                    false => Ty::new_uint(interner, rustc_type_ir::UintTy::Usize),
                },
                layout::IntegerType::Fixed(i, sign) => match sign {
                    true => Ty::new_int(
                        interner,
                        match i {
                            layout::Integer::I8 => rustc_type_ir::IntTy::I8,
                            layout::Integer::I16 => rustc_type_ir::IntTy::I16,
                            layout::Integer::I32 => rustc_type_ir::IntTy::I32,
                            layout::Integer::I64 => rustc_type_ir::IntTy::I64,
                            layout::Integer::I128 => rustc_type_ir::IntTy::I128,
                        },
                    ),
                    false => Ty::new_uint(
                        interner,
                        match i {
                            layout::Integer::I8 => rustc_type_ir::UintTy::U8,
                            layout::Integer::I16 => rustc_type_ir::UintTy::U16,
                            layout::Integer::I32 => rustc_type_ir::UintTy::U32,
                            layout::Integer::I64 => rustc_type_ir::UintTy::U64,
                            layout::Integer::I128 => rustc_type_ir::UintTy::U128,
                        },
                    ),
                },
            },
        )
    }

    /// Returns true if at least one variant of this enum is a non-unit variant.
    pub fn is_data_carrying(self, db: &Db) -> bool {
        self.variants_iter(db).any(|v| !matches!(v.kind(db), StructKind::Unit))
    }

    pub fn layout<'db>(self, db: &'db Db) -> Result<Layout<'db>, LayoutError> {
        Adt::from(self).layout(db)
    }

    pub fn is_unstable(self, db: &Db) -> bool {
        AttrFlags::query(db, self.id.into()).contains(AttrFlags::IS_UNSTABLE)
    }
}

impl HasVisibility for Enum {
    fn visibility(&self, db: &Db) -> Visibility {
        let loc = self.id.lookup(db);
        let source = loc.source_view(db);
        visibility_from_ast_view(db, self.id, source.map(|src| src.visibility()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EnumVariant {
    pub(crate) id: EnumVariantId,
}

impl EnumVariant {
    pub fn module(self, db: &Db) -> Module {
        Module { id: self.id.module(db) }
    }

    pub fn parent_enum(self, db: &Db) -> Enum {
        self.id.lookup(db).parent.into()
    }

    pub fn constructor_ty(self, db: &Db) -> Type<'_> {
        Type::from_value_def(db, self.id)
    }

    pub fn name(self, db: &Db) -> Name {
        self.id.lookup(db).name
    }

    pub fn fields(self, db: &Db) -> Vec<Field> {
        // alloc: editor edge; hot callers use `fields_iter`
        self.fields_iter(db).collect()
    }

    /// [`Self::fields`], without the vector.
    pub fn fields_iter(self, db: &Db) -> impl ExactSizeIterator<Item = Field> + '_ {
        self.id.fields(db).fields().iter().map(move |(id, _)| Field { parent: self.into(), id })
    }

    pub fn kind(self, db: &Db) -> StructKind {
        match self.id.fields(db).shape {
            hir_def::item_tree::FieldsShape::Record => StructKind::Record,
            hir_def::item_tree::FieldsShape::Tuple => StructKind::Tuple,
            hir_def::item_tree::FieldsShape::Unit => StructKind::Unit,
        }
    }

    pub fn eval(self, db: &Db) -> Result<i128, ConstEvalError<'_>> {
        db.const_eval_discriminant(self.into())
    }

    pub fn layout<'db>(&self, db: &'db Db) -> Result<Layout<'db>, LayoutError> {
        let parent_enum = self.parent_enum(db);
        let parent_layout = parent_enum.layout(db)?;
        Ok(match &parent_layout.0.variants {
            layout::Variants::Multiple { .. } => Layout(
                {
                    let lookup = self.id.lookup(db);
                    let rustc_enum_variant_idx = RustcEnumVariantIdx(lookup.index(db));
                    intern_layout(TyLayout::for_variant(&parent_layout.0, rustc_enum_variant_idx))
                },
                db.target_data_layout(parent_enum.krate(db).into()).unwrap(),
            ),
            _ => parent_layout,
        })
    }

    pub fn is_unstable(self, db: &Db) -> bool {
        AttrFlags::query(db, self.id.into()).contains(AttrFlags::IS_UNSTABLE)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StructKind {
    Record,
    Tuple,
    Unit,
}

/// Variants inherit visibility from the parent enum.
impl HasVisibility for EnumVariant {
    fn visibility(&self, db: &Db) -> Visibility {
        self.parent_enum(db).visibility(db)
    }
}

/// A Data Type
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Adt {
    Struct(Struct),
    Union(Union),
    Enum(Enum),
}
impl_from!(Struct, Union, Enum for Adt);

impl Adt {
    pub fn has_non_default_type_params(self, db: &Db) -> bool {
        has_non_default_type_params(db, self.into())
    }

    pub fn layout<'db>(self, db: &'db Db) -> Result<Layout<'db>, LayoutError> {
        let interner = DbInterner::new_no_crate(db);
        let adt_id = AdtId::from(self);
        let args = GenericArgs::for_item_with_defaults(interner, adt_id.into(), |_, id, _| {
            GenericArg::error_from_id(interner, id)
        });
        db.layout_of_adt(adt_id, args.store(), param_env_from_has_crate(db, adt_id).store())
            .map(|layout| Layout(layout, db.target_data_layout(self.krate(db).id).unwrap()))
    }

    /// Turns this ADT into a type. Any type parameters of the ADT will be
    /// turned into unknown types, which is good for e.g. finding the most
    /// general set of completions, but will not look very nice when printed.
    pub fn ty(self, db: &Db) -> Type<'_> {
        let id = AdtId::from(self);
        Type::from_def(db, id)
    }

    pub fn module(self, db: &Db) -> Module {
        match self {
            Adt::Struct(s) => s.module(db),
            Adt::Union(s) => s.module(db),
            Adt::Enum(e) => e.module(db),
        }
    }

    pub fn name(self, db: &Db) -> Name {
        match self {
            Adt::Struct(s) => s.name(db),
            Adt::Union(u) => u.name(db),
            Adt::Enum(e) => e.name(db),
        }
    }

    /// Returns the lifetime of the DataType
    pub fn lifetime(&self, db: &Db) -> Option<LifetimeParamData> {
        let resolver = match self {
            Adt::Struct(s) => s.id.resolver(db),
            Adt::Union(u) => u.id.resolver(db),
            Adt::Enum(e) => e.id.resolver(db),
        };
        resolver
            .generic_params()
            .and_then(|gp| {
                gp.iter_early_bound_lt()
                    // there should only be a single lifetime
                    // but `Arena` requires to use an iterator
                    .nth(0)
            })
            // clone: editor edge, one name and a tag
            .map(|arena| arena.1.clone())
    }

    pub fn as_struct(&self) -> Option<Struct> {
        if let Self::Struct(v) = self { Some(*v) } else { None }
    }

    pub fn as_enum(&self) -> Option<Enum> {
        if let Self::Enum(v) = self { Some(*v) } else { None }
    }

    /// The generic parameters its variance leaves bivariant and no projection of its where
    /// clauses constrains, in declaration order: each one rustc's E0392. Empty when a field's
    /// type has an error.
    pub fn unused_params(self, db: &Db) -> Vec<GenericParam> {
        // alloc: error path, non-empty only when E0392 is reported
        hir_ty::adt_unused_params(db, self.into()).into_iter().map(GenericParam::from).collect()
    }
}

impl HasVisibility for Adt {
    fn visibility(&self, db: &Db) -> Visibility {
        match self {
            Adt::Struct(it) => it.visibility(db),
            Adt::Union(it) => it.visibility(db),
            Adt::Enum(it) => it.visibility(db),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Variant {
    Struct(Struct),
    Union(Union),
    EnumVariant(EnumVariant),
}
impl_from!(Struct, Union, EnumVariant for Variant);

impl Variant {
    pub fn fields(self, db: &Db) -> Vec<Field> {
        match self {
            Variant::Struct(it) => it.fields(db),
            Variant::Union(it) => it.fields(db),
            Variant::EnumVariant(it) => it.fields(db),
        }
    }

    /// [`Variant::fields`], without the vector.
    pub fn fields_iter(self, db: &Db) -> impl ExactSizeIterator<Item = Field> + '_ {
        VariantId::from(self)
            .fields(db)
            .fields()
            .iter()
            .map(move |(id, _)| Field { parent: self, id })
    }

    pub fn module(self, db: &Db) -> Module {
        match self {
            Variant::Struct(it) => it.module(db),
            Variant::Union(it) => it.module(db),
            Variant::EnumVariant(it) => it.module(db),
        }
    }

    pub fn name(&self, db: &Db) -> Name {
        match self {
            Variant::Struct(s) => (*s).name(db),
            Variant::Union(u) => (*u).name(db),
            Variant::EnumVariant(e) => (*e).name(db),
        }
    }

    pub fn adt(&self, db: &Db) -> Adt {
        match *self {
            Variant::Struct(it) => it.into(),
            Variant::Union(it) => it.into(),
            Variant::EnumVariant(it) => it.parent_enum(db).into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AnonConst<'db> {
    id: AnonConstId<'db>,
}

impl<'db> AnonConst<'db> {
    pub fn owner(self, db: &Db) -> ExpressionStoreOwner {
        self.id.loc(db).owner.into()
    }

    pub fn ty(self, db: &'db Db) -> Type<'db> {
        let loc = self.id.loc(db);
        Type { owner: TypeOwnerId::from_anon_const(self.id, db), ty: loc.ty.get() }
    }

    pub fn eval(
        self,
        db: &'db Db,
    ) -> Result<EvaluatedConst<'db>, ConstEvalError<'db>> {
        let ty = self.id.loc(db).ty.get().instantiate_identity().skip_norm_wip();
        db.anon_const_eval(self.id, GenericArgs::empty(), None).map(|it| EvaluatedConst {
            allocation: it,
            def: self.id.into(),
            ty,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InferBody<'db> {
    Body(DefWithBody),
    AnonConst(AnonConst<'db>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExpressionStoreOwner {
    Body(DefWithBody),
    Signature(GenericDef),
    VariantFields(Variant),
}

impl From<GenericDef> for ExpressionStoreOwner {
    fn from(v: GenericDef) -> Self {
        Self::Signature(v)
    }
}

impl From<DefWithBody> for ExpressionStoreOwner {
    fn from(v: DefWithBody) -> Self {
        Self::Body(v)
    }
}

impl_from!(
    ExpressionStoreOwnerId {
        Signature => Signature,
        Body => Body,
        VariantFields => VariantFields,
    }
    for ExpressionStoreOwner
);

impl ExpressionStoreOwner {
    pub fn module(self, db: &Db) -> Module {
        match self {
            Self::Body(body) => body.module(db),
            Self::Signature(generic_def) => generic_def.module(db),
            Self::VariantFields(variant) => variant.module(db),
        }
    }
}

/// The defs which have a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefWithBody {
    Function(Function),
    Static(Static),
    Const(Const),
    EnumVariant(EnumVariant),
}
impl_from!(Function, Const, Static, EnumVariant for DefWithBody);

impl DefWithBody {
    pub fn module(self, db: &Db) -> Module {
        match self {
            DefWithBody::Const(c) => c.module(db),
            DefWithBody::Function(f) => f.module(db),
            DefWithBody::Static(s) => s.module(db),
            DefWithBody::EnumVariant(v) => v.module(db),
        }
    }

    pub fn name(self, db: &Db) -> Option<Name> {
        match self {
            DefWithBody::Function(f) => Some(f.name(db)),
            DefWithBody::Static(s) => Some(s.name(db)),
            DefWithBody::Const(c) => c.name(db),
            DefWithBody::EnumVariant(v) => Some(v.name(db)),
        }
    }

    /// Returns the type this def's body has to evaluate to.
    pub fn body_type(self, db: &Db) -> Type<'_> {
        match self {
            DefWithBody::Function(it) => it.ret_type(db),
            DefWithBody::Static(it) => it.ty(db),
            DefWithBody::Const(it) => it.ty(db),
            DefWithBody::EnumVariant(it) => it.parent_enum(db).variant_body_ty(db),
        }
    }

    fn id(&self) -> Option<DefWithBodyId> {
        Some(match self {
            DefWithBody::Function(it) => match it.id {
                AnyFunctionId::FunctionId(id) => id.into(),
                AnyFunctionId::BuiltinDeriveImplMethod { .. } => return None,
            },
            DefWithBody::Static(it) => it.id.into(),
            DefWithBody::Const(it) => it.id.into(),
            DefWithBody::EnumVariant(it) => it.id.into(),
        })
    }

    #[deprecated = "you should really not use this, this is exported for analysis-stats only"]
    pub fn run_mir_body(self, db: &Db) -> Result<(), MirLowerError<'_>> {
        let Some(id) = self.id() else { return Ok(()) };
        db.mir_body(id.into()).map(drop)
    }

    /// A textual representation of the HIR of this def's body for debugging purposes.
    pub fn debug_hir(self, db: &Db) -> String {
        let Some(id) = self.id() else {
            return String::new(); // alloc: empty
        };
        let body = Body::of(db, id);
        body.pretty_print(db, id, Edition::CURRENT)
    }

    /// A textual representation of the MIR of this def's body for debugging purposes.
    pub fn debug_mir(self, db: &Db) -> String {
        let Some(id) = self.id() else {
            return String::new(); // alloc: empty
        };
        let body = db.mir_body(id.into());
        match body {
            Ok(body) => body.pretty_print(db, self.module(db).krate(db).to_display_target(db)),
            Err(e) => format!("error:\n{e:?}"), // alloc: debug output, error path
        }
    }

    /// Returns an iterator over the inferred types of all expressions in this body.
    pub fn expression_types<'db>(
        self,
        db: &'db Db,
    ) -> impl Iterator<Item = Type<'db>> {
        self.id().into_iter().flat_map(move |def_id| {
            let infer = InferenceResult::of(db, def_id);
            let def_id = def_id.generic_def(db);
            let interner = DbInterner::new_no_crate(db);

            infer.expression_types().map(move |(_, ty)| Type::new_in(interner, def_id, ty))
        })
    }

    /// Returns an iterator over the inferred types of all patterns in this body.
    pub fn pattern_types<'db>(self, db: &'db Db) -> impl Iterator<Item = Type<'db>> {
        self.id().into_iter().flat_map(move |def_id| {
            let infer = InferenceResult::of(db, def_id);
            let def_id = def_id.generic_def(db);
            let interner = DbInterner::new_no_crate(db);

            infer.pattern_types().map(move |(_, ty)| Type::new_in(interner, def_id, ty))
        })
    }

    /// Returns an iterator over the inferred types of all bindings in this body.
    pub fn binding_types<'db>(self, db: &'db Db) -> impl Iterator<Item = Type<'db>> {
        self.id().into_iter().flat_map(move |def_id| {
            let infer = InferenceResult::of(db, def_id);
            let def_id = def_id.generic_def(db);
            let interner = DbInterner::new_no_crate(db);

            infer.binding_types().map(move |(_, ty)| Type::new_in(interner, def_id, ty))
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum AnyFunctionId {
    FunctionId(FunctionId),
    BuiltinDeriveImplMethod { method: BuiltinDeriveImplMethod, impl_: BuiltinDeriveImplId },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Function {
    pub(crate) id: AnyFunctionId,
}

impl fmt::Debug for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.id, f)
    }
}

impl Function {
    pub fn lang(db: &Db, krate: Crate, lang_item: LangItem) -> Option<Function> {
        let lang_items = hir_def::lang_item::lang_items(db, krate.id);
        match lang_item.from_lang_items(lang_items)? {
            LangItemTarget::FunctionId(it) => Some(it.into()),
            _ => None,
        }
    }

    /// Whether this function of a trait cannot be used through `impl_`: one of its where-clauses
    /// that names only the trait's parameters cannot hold there, as `Self: DoubleEndedIterator`
    /// of `Iterator::rev` for an iterator that is not, as far as `krate` can know.
    pub fn is_impossible_in(self, db: &Db, impl_: Impl, krate: Crate) -> bool {
        let (AnyFunctionId::FunctionId(id), Ok(impl_id)) = (self.id, ImplId::try_from(impl_))
        else {
            return false;
        };
        if !matches!(id.lookup(db).container, ItemContainerId::TraitId(_)) {
            return false;
        }
        traits::is_impossible_associated_item(db, krate.id, impl_id, id.into())
    }

    pub fn module(self, db: &Db) -> Module {
        match self.id {
            AnyFunctionId::FunctionId(id) => id.module(db).into(),
            AnyFunctionId::BuiltinDeriveImplMethod { impl_, .. } => impl_.module(db).into(),
        }
    }

    pub fn name(self, db: &Db) -> Name {
        match self.id {
            AnyFunctionId::FunctionId(id) => FunctionSignature::of(db, id).name,
            AnyFunctionId::BuiltinDeriveImplMethod { method, .. } => {
                Name::new_symbol_root(method.name())
            }
        }
    }

    pub fn ty(self, db: &Db) -> Type<'_> {
        match self.id {
            AnyFunctionId::FunctionId(id) => Type::from_value_def(db, id),
            AnyFunctionId::BuiltinDeriveImplMethod { method, impl_ } => {
                // Get the type for the trait function, as we can't get the type for the impl function
                // because it has not `CallableDefId`.
                // FIXME: This does not account for replacing `Self`. Do we really need that?
                let Some(trait_method) = method.trait_method(db, impl_) else {
                    return Type::unknown(db);
                };
                Function::from(trait_method).ty(db)
            }
        }
    }

    pub fn fn_ptr_type(self, db: &Db) -> Type<'_> {
        match self.id {
            AnyFunctionId::FunctionId(id) => {
                let interner = DbInterner::new_no_crate(db);
                let callable_sig =
                    db.callable_item_signature(id.into()).instantiate_identity().skip_norm_wip();
                let ty = Ty::new_fn_ptr(interner, callable_sig);
                Type::new(db, id.into(), ty)
            }
            AnyFunctionId::BuiltinDeriveImplMethod { method, impl_ } => {
                // Get the type for the trait function, as we can't get the type for the impl function
                // because it has not `CallableDefId`.
                // FIXME: This does not account for replacing `Self`. Do we really need that?
                let Some(trait_method) = method.trait_method(db, impl_) else {
                    return Type::unknown(db);
                };
                Function::from(trait_method).fn_ptr_type(db)
            }
        }
    }

    fn fn_sig<'db>(self, db: &'db Db) -> (TypeOwnerId, PolyFnSig<'db>) {
        let fn_ptr = self.fn_ptr_type(db);
        let TyKind::FnPtr(sig_tys, hdr) = fn_ptr.ty.skip_binder().kind() else {
            unreachable!();
        };
        (fn_ptr.owner, sig_tys.with(hdr))
    }

    fn erased_fn_sig<'db>(self, db: &'db Db) -> (TypeOwnerId, FnSig<'db>) {
        let (owner, sig) = self.fn_sig(db);
        let sig = DbInterner::new_no_crate(db).instantiate_bound_regions_with_erased(sig);
        (owner, sig)
    }

    /// The lowered signature, its late-bound lifetimes free lifetimes of the function
    /// (rustc's `liberate_late_bound_regions`), as rustc's checks of a signature read it.
    /// `None` for a built-in derive's method, which has none of its own.
    pub fn signature(self, db: &Db) -> Option<FnSignature<'_>> {
        let AnyFunctionId::FunctionId(id) = self.id else {
            return None;
        };
        let poly = db.callable_item_signature(id.into()).instantiate_identity().skip_norm_wip();
        let sig = DbInterner::new_no_crate(db).liberate_late_bound_regions(id.into(), poly);
        Some(FnSignature { owner: GenericDefId::FunctionId(id), sig })
    }

    /// For a method of a trait impl, the trait's method's signature with the impl's arguments,
    /// in this method's generics (rustc's `suggest_trait_fn_ty_for_impl_fn_infer`), and that
    /// method.
    pub fn trait_signature_in_impl(self, db: &Db) -> Option<(Function, FnSignature<'_>)> {
        let AnyFunctionId::FunctionId(id) = self.id else {
            return None;
        };
        let (trait_method, sig) = hir_ty_wfcheck::trait_signature_in_impl(db, id)?;
        Some((trait_method.into(), FnSignature { owner: GenericDefId::FunctionId(id), sig }))
    }

    /// For a method of a trait impl, the first place its signature is not a subtype of the
    /// trait's method's with the impl's arguments, normalized in this method's environment:
    /// rustc's E0053 (`compare_method_clause_entailment`).
    pub fn impl_signature_mismatch(self, db: &Db) -> Option<ImplSignatureMismatch<'_>> {
        let AnyFunctionId::FunctionId(id) = self.id else {
            return None;
        };
        let it = hir_ty_wfcheck::compare_impl_method_signature(db, id)?;
        let owner = GenericDefId::FunctionId(id);
        let interner = DbInterner::new_no_crate(db);
        Some(ImplSignatureMismatch {
            trait_method: it.trait_method.into(),
            at: it.at,
            types: it.types.map(|types| {
                (
                    Type::new_in(interner, owner, types.expected),
                    Type::new_in(interner, owner, types.found),
                )
            }),
            expected: FnSignature { owner, sig: it.expected },
            found: FnSignature { owner, sig: it.found },
        })
    }

    /// For a `#[proc_macro]` or `#[proc_macro_derive]` function, or with `attribute` a
    /// `#[proc_macro_attribute]` one, the first place its signature differs from safe Rust
    /// `fn(TokenStream) -> TokenStream` (two `TokenStream`s for an attribute): rustc's
    /// `ProcMacroBadSig` (`check_proc_macro`).
    pub fn proc_macro_signature_mismatch(
        self,
        db: &Db,
        attribute: bool,
    ) -> Option<ProcMacroSignatureMismatch<'_>> {
        let AnyFunctionId::FunctionId(id) = self.id else {
            return None;
        };
        let inputs = if attribute { 2 } else { 1 };
        let it = hir_ty_wfcheck::compare_proc_macro_signature(db, id, inputs)?;
        let owner = GenericDefId::FunctionId(id);
        Some(ProcMacroSignatureMismatch {
            at: it.at,
            expected: FnSignature { owner, sig: it.expected },
            found: FnSignature { owner, sig: it.found },
        })
    }

    /// Get this function's return type
    pub fn ret_type(self, db: &Db) -> Type<'_> {
        let (owner, sig) = self.erased_fn_sig(db);
        Type { owner, ty: EarlyBinder::bind(DbInterner::new_no_crate(db), sig.output()) }
    }

    pub fn async_ret_type<'db>(self, db: &'db Db) -> Option<Type<'db>> {
        let AnyFunctionId::FunctionId(id) = self.id else {
            return None;
        };
        if !self.is_async(db) {
            return None;
        }
        let interner = DbInterner::new_no_crate(db);
        let sig = db.callable_item_signature(id.into()).instantiate_identity().skip_norm_wip();
        let ret_ty = interner.instantiate_bound_regions_with_erased(sig).output();
        for pred in ret_ty.impl_trait_bounds(db).into_iter().flatten() {
            let clause = interner.instantiate_bound_regions_with_erased(pred.kind());
            if let ClauseKind::Projection(projection) = clause
                && let Some(output_ty) = projection.term.as_type()
            {
                return Some(Type::new(db, id.into(), output_ty));
            }
        }
        None
    }

    pub fn has_self_param(self, db: &Db) -> bool {
        match self.id {
            AnyFunctionId::FunctionId(id) => FunctionSignature::of(db, id).has_self_param(),
            AnyFunctionId::BuiltinDeriveImplMethod { method, .. } => match method {
                BuiltinDeriveImplMethod::clone
                | BuiltinDeriveImplMethod::fmt
                | BuiltinDeriveImplMethod::hash
                | BuiltinDeriveImplMethod::cmp
                | BuiltinDeriveImplMethod::partial_cmp
                | BuiltinDeriveImplMethod::eq => true,
                BuiltinDeriveImplMethod::default => false,
            },
        }
    }

    pub fn self_param(self, db: &Db) -> Option<SelfParam> {
        self.has_self_param(db).then_some(SelfParam { func: self })
    }

    pub fn assoc_fn_params(self, db: &Db) -> Vec<Param<'_>> {
        let (owner, sig) = self.erased_fn_sig(db);
        let interner = DbInterner::new_no_crate(db);
        let func = match self.id {
            AnyFunctionId::FunctionId(id) => Callee::Def(CallableDefId::FunctionId(id)),
            AnyFunctionId::BuiltinDeriveImplMethod { method, impl_ } => {
                Callee::BuiltinDeriveImplMethod { method, impl_ }
            }
        };
        sig.inputs()
            .iter()
            .enumerate()
            .map(|(idx, &ty)| Param {
                func,
                ty: Type { owner, ty: EarlyBinder::bind(interner, ty) },
                idx,
            })
            .collect() // alloc: editor edge, the owned parameter list callers take
    }

    pub fn num_params(self, db: &Db) -> usize {
        match self.id {
            AnyFunctionId::FunctionId(id) => FunctionSignature::of(db, id).params.len(),
            AnyFunctionId::BuiltinDeriveImplMethod { .. } => {
                self.fn_sig(db).1.skip_binder().inputs().len()
            }
        }
    }

    pub fn method_params(self, db: &Db) -> Option<Vec<Param<'_>>> {
        self.self_param(db)?;
        Some(self.params_without_self(db))
    }

    pub fn params_without_self(self, db: &Db) -> Vec<Param<'_>> {
        let mut params = self.assoc_fn_params(db);
        if self.has_self_param(db) {
            params.remove(0);
        }
        params
    }

    pub fn is_const(self, db: &Db) -> bool {
        match self.id {
            AnyFunctionId::FunctionId(id) => FunctionSignature::of(db, id).is_const(),
            AnyFunctionId::BuiltinDeriveImplMethod { .. } => false,
        }
    }

    pub fn is_async(self, db: &Db) -> bool {
        match self.id {
            AnyFunctionId::FunctionId(id) => FunctionSignature::of(db, id).is_async(),
            AnyFunctionId::BuiltinDeriveImplMethod { .. } => false,
        }
    }

    pub fn is_unsafe(self, db: &Db) -> bool {
        match self.id {
            AnyFunctionId::FunctionId(id) => FunctionSignature::of(db, id).is_unsafe(),
            AnyFunctionId::BuiltinDeriveImplMethod { .. } => false,
        }
    }

    pub fn is_varargs(self, db: &Db) -> bool {
        match self.id {
            AnyFunctionId::FunctionId(id) => FunctionSignature::of(db, id).is_varargs(),
            AnyFunctionId::BuiltinDeriveImplMethod { .. } => false,
        }
    }

    pub fn extern_block(self, db: &Db) -> Option<ExternBlock> {
        match self.id {
            AnyFunctionId::FunctionId(id) => match id.lookup(db).container {
                ItemContainerId::ExternBlockId(id) => Some(ExternBlock { id }),
                _ => None,
            },
            AnyFunctionId::BuiltinDeriveImplMethod { .. } => None,
        }
    }

    pub fn returns_impl_future(self, db: &Db) -> bool {
        if self.is_async(db) {
            return true;
        }

        let ret_type = self.ret_type(db);
        let Some(impl_traits) = ret_type.as_impl_traits(db) else { return false };
        let lang_items = hir_def::lang_item::lang_items(db, self.krate(db).id);
        let Some(future_trait_id) = lang_items.Future else {
            return false;
        };
        let Some(sized_trait_id) = lang_items.Sized else {
            return false;
        };

        let mut has_impl_future = false;
        impl_traits
            .filter(|t| {
                let fut = t.id == future_trait_id;
                has_impl_future |= fut;
                !fut && t.id != sized_trait_id
            })
            // all traits but the future trait must be auto traits
            .all(|t| t.is_auto(db))
            && has_impl_future
    }

    /// Does this function have `#[test]` attribute?
    pub fn is_test(self, db: &Db) -> bool {
        self.attrs(db).contains(AttrFlags::IS_TEST)
    }

    /// is this a `fn main` or a function with an `export_name` of `main`? A crate of
    /// procedural macros has no entry point: its `fn main` is a function as any other (tokio's
    /// `#[proc_macro_attribute] pub fn main`).
    pub fn is_main(self, db: &Db) -> bool {
        match self.id {
            AnyFunctionId::FunctionId(id) => {
                self.exported_main(db)
                    || self.module(db).is_crate_root(db)
                        && FunctionSignature::of(db, id).name == sym::main
                        && !crate_def_map(db, HasModule::krate(&id, db)).is_proc_macro()
            }
            AnyFunctionId::BuiltinDeriveImplMethod { .. } => false,
        }
    }

    fn attrs(self, db: &Db) -> AttrFlags {
        match self.id {
            AnyFunctionId::FunctionId(id) => AttrFlags::query(db, id.into()),
            AnyFunctionId::BuiltinDeriveImplMethod { .. } => AttrFlags::empty(),
        }
    }

    /// Is this a function with an `export_name` of `main`?
    pub fn exported_main(self, db: &Db) -> bool {
        self.attrs(db).contains(AttrFlags::IS_EXPORT_NAME_MAIN)
    }

    /// Does this function have the ignore attribute?
    pub fn is_ignore(self, db: &Db) -> bool {
        self.attrs(db).contains(AttrFlags::IS_IGNORE)
    }

    /// Does this function have `#[bench]` attribute?
    pub fn is_bench(self, db: &Db) -> bool {
        self.attrs(db).contains(AttrFlags::IS_BENCH)
    }

    /// Is this function marked as unstable with `#[feature]` attribute?
    pub fn is_unstable(self, db: &Db) -> bool {
        self.attrs(db).contains(AttrFlags::IS_UNSTABLE)
    }

    pub fn is_unsafe_to_call(
        self,
        db: &Db,
        caller: Option<Function>,
        call_edition: Edition,
    ) -> bool {
        let AnyFunctionId::FunctionId(id) = self.id else {
            return false;
        };
        let (target_features, target_feature_is_safe_in_target) = caller
            .map(|caller| {
                let target_features = match caller.id {
                    AnyFunctionId::FunctionId(id) => hir_ty::TargetFeatures::from_fn(db, id),
                    AnyFunctionId::BuiltinDeriveImplMethod { .. } => {
                        hir_ty::TargetFeatures::default()
                    }
                };
                let target_feature_is_safe_in_target =
                    match &caller.krate(db).id.workspace_data(db).target {
                        Ok(target) => hir_ty::target_feature_is_safe_in_target(target),
                        Err(_) => hir_ty::TargetFeatureIsSafeInTarget::No,
                    };
                (target_features, target_feature_is_safe_in_target)
            })
            .unwrap_or_else(|| {
                (hir_ty::TargetFeatures::default(), hir_ty::TargetFeatureIsSafeInTarget::No)
            });
        matches!(
            hir_ty::is_fn_unsafe_to_call(
                db,
                id,
                &target_features,
                call_edition,
                target_feature_is_safe_in_target
            ),
            hir_ty::Unsafety::Unsafe
        )
    }

    /// Whether this function declaration has a definition.
    ///
    /// This is false in the case of required (not provided) trait methods.
    pub fn has_body(self, db: &Db) -> bool {
        match self.id {
            AnyFunctionId::FunctionId(id) => FunctionSignature::of(db, id).has_body(),
            AnyFunctionId::BuiltinDeriveImplMethod { .. } => true,
        }
    }

    pub fn as_proc_macro(self, db: &Db) -> Option<Macro> {
        let AnyFunctionId::FunctionId(id) = self.id else {
            return None;
        };
        let def_map = crate_def_map(db, HasModule::krate(&id, db));
        def_map.fn_as_proc_macro(id).map(|id| Macro { id: id.into() })
    }

    pub fn eval(
        self,
        db: &Db,
        span_formatter: impl Fn(FileId, TextRange) -> String,
    ) -> Result<String, ConstEvalError<'_>> {
        let AnyFunctionId::FunctionId(id) = self.id else {
            return Err(ConstEvalError::MirEvalError(MirEvalError::NotSupported(
                // alloc: error path
                "evaluation of builtin derive impl methods is not supported".to_owned(),
            )));
        };
        let body = db.monomorphized_runtime_mir_body(
            id.into(),
            GenericArgs::empty().store(),
            ParamEnvAndCrate {
                param_env: db.trait_environment(id.into()),
                krate: id.module(db).krate(db),
            }
            .store(),
        )?;
        let (result, output) = interpret_mir(db, body, None)?;
        let mut text = match result {
            Ok(_) => "pass".to_owned(), // alloc: editor edge, the owned report `eval` returns
            Err(e) => {
                let mut r = String::new(); // alloc: error path, the rendered evaluation error
                _ = e.pretty_print(
                    &mut r,
                    db,
                    &span_formatter,
                    self.krate(db).to_display_target(db),
                );
                r
            }
        };
        let stdout = output.stdout().into_owned();
        if !stdout.is_empty() {
            text += "\n--------- stdout ---------\n";
            text += &stdout;
        }
        let stderr = output.stdout().into_owned();
        if !stderr.is_empty() {
            text += "\n--------- stderr ---------\n";
            text += &stderr;
        }
        Ok(text)
    }
}

// Note: logically, this belongs to `hir_ty`, but we are not using it there yet.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Shared,
    Exclusive,
    Owned,
}

impl From<hir_ty::next_solver::Mutability> for Access {
    fn from(mutability: hir_ty::next_solver::Mutability) -> Access {
        match mutability {
            hir_ty::next_solver::Mutability::Not => Access::Shared,
            hir_ty::next_solver::Mutability::Mut => Access::Exclusive,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Param<'db> {
    func: Callee<'db>,
    /// The index in parameter list, including self parameter.
    idx: usize,
    ty: Type<'db>,
}

impl<'db> Param<'db> {
    pub fn parent_fn(&self) -> Option<Function> {
        match self.func {
            Callee::Def(CallableDefId::FunctionId(f)) => Some(f.into()),
            Callee::BuiltinDeriveImplMethod { method, impl_ } => {
                Some(Function { id: AnyFunctionId::BuiltinDeriveImplMethod { method, impl_ } })
            }
            _ => None,
        }
    }

    // pub fn parent_closure(&self) -> Option<Closure> {
    //     self.func.as_ref().right().cloned()
    // }

    pub fn index(&self) -> usize {
        self.idx
    }

    pub fn ty(&self) -> &Type<'db> {
        &self.ty
    }

    pub fn name(&self, db: &Db) -> Option<Name> {
        Some(self.as_local(db)?.name(db))
    }

    pub fn as_local(&self, db: &'db Db) -> Option<Local<'db>> {
        match self.func {
            Callee::Def(CallableDefId::FunctionId(it)) => {
                let parent = DefWithBodyId::FunctionId(it);
                let body = Body::of(db, parent);
                if let Some(self_param) = body.self_param.filter(|_| self.idx == 0) {
                    Some(Local {
                        parent: parent.into(),
                        parent_infer: parent.into(),
                        binding_id: self_param.user_written,
                    })
                } else if let Pat::Bind { id, .. } =
                    body.pat(body.params[self.idx - body.self_param.is_some() as usize].user_written)
                {
                    Some(Local {
                        parent: parent.into(),
                        parent_infer: parent.into(),
                        binding_id: id,
                    })
                } else {
                    None
                }
            }
            Callee::Closure(closure, _) => {
                let c = closure.loc(db);
                let body_infer_owner = c.owner;
                let body_owner = c.owner.expression_store_owner(db);
                let store = ExpressionStore::of(db, body_owner);

                if let Expr::Closure { args, .. } = store.expr(c.expr)
                    && let Pat::Bind { id, .. } = store.pat(args[self.idx])
                {
                    return Some(Local {
                        parent: body_owner,
                        parent_infer: body_infer_owner,
                        binding_id: id,
                    });
                }
                None
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SelfParam {
    func: Function,
}

impl SelfParam {
    pub fn access(self, db: &Db) -> Access {
        match self.func.id {
            AnyFunctionId::FunctionId(id) => {
                let func_data = FunctionSignature::of(db, id);
                func_data
                    .params
                    .first()
                    .map(|&param| match func_data.store.type_ref(param) {
                        TypeRef::Reference(ref_) => match ref_.mutability {
                            hir_def::type_ref::Mutability::Shared => Access::Shared,
                            hir_def::type_ref::Mutability::Mut => Access::Exclusive,
                        },
                        _ => Access::Owned,
                    })
                    .unwrap_or(Access::Owned)
            }
            AnyFunctionId::BuiltinDeriveImplMethod { method, .. } => match method {
                BuiltinDeriveImplMethod::clone
                | BuiltinDeriveImplMethod::fmt
                | BuiltinDeriveImplMethod::hash
                | BuiltinDeriveImplMethod::cmp
                | BuiltinDeriveImplMethod::partial_cmp
                | BuiltinDeriveImplMethod::eq => Access::Shared,
                BuiltinDeriveImplMethod::default => {
                    unreachable!("this function does not have a self param")
                }
            },
        }
    }

    pub fn parent_fn(&self) -> Function {
        self.func
    }

    pub fn ty<'db>(&self, db: &'db Db) -> Type<'db> {
        let (owner, sig) = self.func.erased_fn_sig(db);
        Type { owner, ty: EarlyBinder::bind(DbInterner::new_no_crate(db), sig.inputs()[0]) }
    }
}

impl HasVisibility for Function {
    fn visibility(&self, db: &Db) -> Visibility {
        match self.id {
            AnyFunctionId::FunctionId(id) => AssocItemId::from(id).assoc_visibility(db),
            AnyFunctionId::BuiltinDeriveImplMethod { .. } => Visibility::Public,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternCrateDecl {
    pub(crate) id: ExternCrateId,
}

impl ExternCrateDecl {
    pub fn module(self, db: &Db) -> Module {
        self.id.module(db).into()
    }

    pub fn resolved_crate(self, db: &Db) -> Option<Crate> {
        let loc = self.id.lookup(db);
        let krate = loc.container.krate(db);
        let name = self.name(db);
        if name == sym::self_ {
            Some(krate.into())
        } else {
            krate.data(db).dependencies.iter().find_map(|dep| {
                if dep.name.symbol() == name.symbol() { Some(dep.crate_id.into()) } else { None }
            })
        }
    }

    pub fn name(self, db: &Db) -> Name {
        let loc = self.id.lookup(db);
        let source = loc.source_view(db);
        as_name_opt(source.value.name_ref())
    }

    pub fn alias(self, db: &Db) -> Option<ImportAlias> {
        let loc = self.id.lookup(db);
        let source = loc.source_view(db);
        let rename = source.value.rename()?;
        if let Some(name) = rename.name() {
            Some(ImportAlias::Alias(name.as_name()))
        } else if rename.underscore_token().is_some() {
            Some(ImportAlias::Underscore)
        } else {
            None
        }
    }

    /// Returns the name under which this crate is made accessible, taking `_` into account.
    pub fn alias_or_name(self, db: &Db) -> Option<Name> {
        match self.alias(db) {
            Some(ImportAlias::Underscore) => None,
            Some(ImportAlias::Alias(alias)) => Some(alias),
            None => Some(self.name(db)),
        }
    }
}

impl HasVisibility for ExternCrateDecl {
    fn visibility(&self, db: &Db) -> Visibility {
        let loc = self.id.lookup(db);
        let source = loc.source_view(db);
        visibility_from_ast_view(db, self.id, source.map(|src| src.visibility()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Const {
    pub(crate) id: ConstId,
}

impl Const {
    pub fn module(self, db: &Db) -> Module {
        Module { id: self.id.module(db) }
    }

    pub fn name(self, db: &Db) -> Option<Name> {
        ConstSignature::of(db, self.id).name
    }

    pub fn ty(self, db: &Db) -> Type<'_> {
        Type::from_value_def(db, self.id)
    }

    pub fn has_body(self, db: &Db) -> bool {
        ConstSignature::of(db, self.id).has_body()
    }

    /// Evaluate the constant.
    pub fn eval(self, db: &Db) -> Result<EvaluatedConst<'_>, ConstEvalError<'_>> {
        let ty = db.value_ty(self.id.into()).unwrap().instantiate_identity().skip_norm_wip();
        db.const_eval(self.id, GenericArgs::empty(), None).map(|it| EvaluatedConst {
            allocation: it,
            def: self.id.into(),
            ty,
        })
    }
}

impl HasVisibility for Const {
    fn visibility(&self, db: &Db) -> Visibility {
        AssocItemId::from(self.id).assoc_visibility(db)
    }
}

pub struct EvaluatedConst<'db> {
    def: InferBodyId<'db>,
    allocation: hir_ty::next_solver::Allocation<'db>,
    ty: Ty<'db>,
}

impl<'db> EvaluatedConst<'db> {
    pub fn render(&self, db: &Db, display_target: DisplayTarget) -> String {
        // alloc: editor edge, rendered for display
        format!("{}", self.allocation.display(db, display_target))
    }

    pub fn render_debug(&self, db: &'db Db) -> Result<String, MirEvalError<'db>> {
        let ty = self.allocation.ty.kind();
        if let TyKind::Int(_) | TyKind::Uint(_) = ty {
            let b = &self.allocation.memory;
            let value = u128::from_le_bytes(mir::pad16(b, mir::IsSigned::No));
            let is_signed = matches!(ty, TyKind::Int(_)).into();
            let value_signed = i128::from_le_bytes(mir::pad16(b, is_signed));
            let mut result = // alloc: editor edge, rendered for display
                if let TyKind::Int(_) = ty { value_signed.to_string() } else { value.to_string() };
            if value >= 10 {
                format_to!(result, " ({value:#X})");
                return Ok(result);
            } else {
                return Ok(result);
            }
        }
        mir::render_const_using_debug_impl(db, self.def, self.allocation, self.ty)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Static {
    pub(crate) id: StaticId,
}

impl Static {
    pub fn module(self, db: &Db) -> Module {
        Module { id: self.id.module(db) }
    }

    pub fn name(self, db: &Db) -> Name {
        StaticSignature::of(db, self.id).name
    }

    pub fn is_mut(self, db: &Db) -> bool {
        StaticSignature::of(db, self.id).flags.contains(StaticFlags::MUTABLE)
    }

    pub fn ty(self, db: &Db) -> Type<'_> {
        Type::from_value_def(db, self.id)
    }

    pub fn extern_block(self, db: &Db) -> Option<ExternBlock> {
        match self.id.lookup(db).container {
            ItemContainerId::ExternBlockId(id) => Some(ExternBlock { id }),
            _ => None,
        }
    }

    /// Evaluate the static initializer.
    pub fn eval(self, db: &Db) -> Result<EvaluatedConst<'_>, ConstEvalError<'_>> {
        let ty = db.value_ty(self.id.into()).unwrap().instantiate_identity().skip_norm_wip();
        db.const_eval_static(self.id).map(|it| EvaluatedConst {
            allocation: it,
            def: self.id.into(),
            ty,
        })
    }
}

impl HasVisibility for Static {
    fn visibility(&self, db: &Db) -> Visibility {
        let loc = self.id.lookup(db);
        let source = loc.source_view(db);
        visibility_from_ast_view(db, self.id, source.map(|src| src.visibility()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Trait {
    pub(crate) id: TraitId,
}

impl Trait {
    pub fn lang(db: &Db, krate: Crate, lang_item: LangItem) -> Option<Trait> {
        let lang_items = hir_def::lang_item::lang_items(db, krate.id);
        match lang_item.from_lang_items(lang_items)? {
            LangItemTarget::TraitId(it) => Some(it.into()),
            _ => None,
        }
    }

    pub fn module(self, db: &Db) -> Module {
        Module { id: self.id.lookup(db).container }
    }

    pub fn name(self, db: &Db) -> Name {
        TraitSignature::of(db, self.id).name
    }

    pub fn direct_supertraits(self, db: &Db) -> Vec<Trait> {
        let traits = direct_super_traits(db, self.into());
        traits.iter().map(|tr| Trait::from(*tr)).collect() // alloc: editor edge
    }

    pub fn all_supertraits(self, db: &Db) -> Vec<Trait> {
        let traits = all_super_traits(db, self.into());
        traits.iter().map(|tr| Trait::from(*tr)).collect() // alloc: editor edge
    }

    pub fn function(self, db: &Db, name: impl PartialEq<Name>) -> Option<Function> {
        self.id.trait_items(db).items.iter().find(|(n, _)| name == *n).and_then(|&(_, it)| match it
        {
            AssocItemId::FunctionId(id) => Some(id.into()),
            _ => None,
        })
    }

    pub fn items(self, db: &Db) -> Vec<AssocItem> {
        // alloc: editor edge; hot callers use `items_iter`
        self.items_iter(db).collect()
    }

    /// [`Trait::items`], without the vector.
    pub fn items_iter(self, db: &Db) -> impl ExactSizeIterator<Item = AssocItem> + '_ {
        self.id.trait_items(db).items.iter().map(|(_name, it)| (*it).into())
    }

    pub fn items_with_supertraits(self, db: &Db) -> Vec<AssocItem> {
        all_super_traits(db, self.into())
            .iter()
            .flat_map(|&tr| Trait::from(tr).items_iter(db))
            .collect() // alloc: editor edge, the owned item list callers take
    }

    pub fn is_auto(self, db: &Db) -> bool {
        TraitSignature::of(db, self.id).flags.contains(TraitFlags::AUTO)
    }

    pub fn is_unsafe(&self, db: &Db) -> bool {
        TraitSignature::of(db, self.id).flags.contains(TraitFlags::UNSAFE)
    }

    /// Whether the trait is `#[rustc_deny_explicit_impl]`: no impl of it may be written.
    pub fn is_deny_explicit_impl(&self, db: &Db) -> bool {
        TraitSignature::of(db, self.id).flags.contains(TraitFlags::DENY_EXPLICIT_IMPL)
    }

    pub fn type_or_const_param_count(
        &self,
        db: &Db,
        count_required_only: bool,
    ) -> usize {
        GenericParams::of(db,self.id.into())
            .iter_type_or_consts()
            .filter(|(_, ty)| !matches!(ty, TypeOrConstParamData::TypeParamData(ty) if ty.provenance != TypeParamProvenance::TypeParamList))
            .filter(|(_, ty)| !count_required_only || !ty.has_default())
            .count()
    }

    pub fn dyn_compatibility(&self, db: &Db) -> Option<DynCompatibilityViolation> {
        hir_ty::dyn_compatibility::dyn_compatibility(db, self.id)
    }

    pub fn dyn_compatibility_all_violations(
        &self,
        db: &Db,
    ) -> Option<Vec<DynCompatibilityViolation>> {
        let mut violations = vec![]; // alloc: error path, filled only with violations
        _ = hir_ty::dyn_compatibility::dyn_compatibility_with_callback(
            db,
            self.id,
            &mut |violation| {
                violations.push(violation);
                ControlFlow::Continue(())
            },
        );
        violations.is_empty().not().then_some(violations)
    }

    /// `#[rust_analyzer::completions(...)]` mode.
    pub fn complete(self, db: &Db) -> Complete {
        Complete::extract(true, self.attrs(db).attrs)
    }

    // Feature: Prefer Underscore Import Attribute
    // Crate authors can declare that their trait prefers to be imported `as _`. This can be used
    // for example for extension traits. To do that, a trait has to include the attribute
    // `#[rust_analyzer::prefer_underscore_import]`
    //
    // When a trait includes this attribute, flyimport will import it `as _`, and the quickfix
    // to import it will prefer to import it `as _` (but allow to import it normally as well).
    //
    // Malformed attributes will be ignored without warnings.
    pub fn prefer_underscore_import(self, db: &Db) -> bool {
        AttrFlags::query(db, self.id.into()).contains(AttrFlags::PREFER_UNDERSCORE_IMPORT)
    }

    pub fn must_implement_one_of(self, db: &Db) -> Option<&[Name]> {
        AttrFlags::must_implement_one_of(db, self.id)
    }
}

impl HasVisibility for Trait {
    fn visibility(&self, db: &Db) -> Visibility {
        let loc = self.id.lookup(db);
        let source = loc.source_view(db);
        visibility_from_ast_view(db, self.id, source.map(|src| src.visibility()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeAlias {
    pub(crate) id: TypeAliasId,
}

impl TypeAlias {
    pub fn has_non_default_type_params(self, db: &Db) -> bool {
        has_non_default_type_params(db, self.id.into())
    }

    pub fn module(self, db: &Db) -> Module {
        Module { id: self.id.module(db) }
    }

    pub fn ty(self, db: &Db) -> Type<'_> {
        Type::from_def(db, self.id)
    }

    pub fn name(self, db: &Db) -> Name {
        TypeAliasSignature::of(db, self.id).name
    }

    pub fn has_type(self, db: &Db) -> bool {
        TypeAliasSignature::of(db, self.id).ty.is_some()
    }
}

impl HasVisibility for TypeAlias {
    fn visibility(&self, db: &Db) -> Visibility {
        AssocItemId::from(self.id).assoc_visibility(db)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternBlock {
    pub(crate) id: ExternBlockId,
}

impl ExternBlock {
    pub fn module(self, db: &Db) -> Module {
        Module { id: self.id.module(db) }
    }
}

/// A `use` item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UseDecl {
    pub(crate) id: hir_def::UseId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StaticLifetime;

impl StaticLifetime {
    pub fn name(self) -> Name {
        Name::new_symbol_root(sym::tick_static)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BuiltinType {
    pub(crate) inner: hir_def::builtin_type::BuiltinType,
}

impl BuiltinType {
    // Constructors are added on demand, feel free to add more.
    pub fn str() -> BuiltinType {
        BuiltinType { inner: hir_def::builtin_type::BuiltinType::Str }
    }

    pub fn i32() -> BuiltinType {
        BuiltinType {
            inner: hir_def::builtin_type::BuiltinType::Int(hir_ty::primitive::BuiltinInt::I32),
        }
    }

    pub fn bool() -> BuiltinType {
        BuiltinType { inner: hir_def::builtin_type::BuiltinType::Bool }
    }

    pub fn ty<'db>(self, db: &'db Db) -> Type<'db> {
        let interner = DbInterner::new_no_crate(db);
        let ty = Ty::from_builtin_type(interner, self.inner);
        Type::no_params(db, Type::builtin_type_crate(db), ty)
    }

    pub fn name(self) -> Name {
        self.inner.as_name()
    }

    pub fn is_int(&self) -> bool {
        matches!(self.inner, hir_def::builtin_type::BuiltinType::Int(_))
    }

    pub fn is_uint(&self) -> bool {
        matches!(self.inner, hir_def::builtin_type::BuiltinType::Uint(_))
    }

    pub fn is_float(&self) -> bool {
        matches!(self.inner, hir_def::builtin_type::BuiltinType::Float(_))
    }

    pub fn is_f16(&self) -> bool {
        matches!(
            self.inner,
            hir_def::builtin_type::BuiltinType::Float(hir_def::builtin_type::BuiltinFloat::F16)
        )
    }

    pub fn is_f32(&self) -> bool {
        matches!(
            self.inner,
            hir_def::builtin_type::BuiltinType::Float(hir_def::builtin_type::BuiltinFloat::F32)
        )
    }

    pub fn is_f64(&self) -> bool {
        matches!(
            self.inner,
            hir_def::builtin_type::BuiltinType::Float(hir_def::builtin_type::BuiltinFloat::F64)
        )
    }

    pub fn is_f128(&self) -> bool {
        matches!(
            self.inner,
            hir_def::builtin_type::BuiltinType::Float(hir_def::builtin_type::BuiltinFloat::F128)
        )
    }

    pub fn is_char(&self) -> bool {
        matches!(self.inner, hir_def::builtin_type::BuiltinType::Char)
    }

    pub fn is_bool(&self) -> bool {
        matches!(self.inner, hir_def::builtin_type::BuiltinType::Bool)
    }

    pub fn is_str(&self) -> bool {
        matches!(self.inner, hir_def::builtin_type::BuiltinType::Str)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Macro {
    pub(crate) id: MacroId,
}

impl Macro {
    pub fn module(self, db: &Db) -> Module {
        Module { id: self.id.module(db) }
    }

    pub fn name(self, db: &Db) -> Name {
        match self.id {
            MacroId::Macro2Id(id) => {
                let loc = id.lookup(db);
                let source = loc.source_view(db);
                as_name_opt(source.value.name())
            }
            MacroId::MacroRulesId(id) => {
                let loc = id.lookup(db);
                let source = loc.source_view(db);
                as_name_opt(source.value.name())
            }
            MacroId::ProcMacroId(id) => {
                let loc = id.lookup(db);
                let source = loc.source_view(db);
                match loc.kind {
                    ProcMacroKind::CustomDerive => AttrFlags::derive_info(db, self.id).map_or_else(
                        || as_name_opt(source.value.name()),
                        |info| Name::new_symbol_root(info.trait_name),
                    ),
                    ProcMacroKind::Bang | ProcMacroKind::Attr => as_name_opt(source.value.name()),
                }
            }
        }
    }

    pub fn is_proc_macro(self) -> bool {
        matches!(self.id, MacroId::ProcMacroId(_))
    }

    pub fn kind(&self, db: &Db) -> MacroKind {
        self.id.definition(db).kind.macro_kind()
    }

    pub fn is_fn_like(&self, db: &Db) -> bool {
        matches!(
            self.kind(db),
            MacroKind::Declarative | MacroKind::DeclarativeBuiltIn | MacroKind::ProcMacro
        )
    }

    pub fn builtin_derive_kind(&self, db: &Db) -> Option<BuiltinDeriveMacroKind> {
        let expander = match self.id {
            MacroId::Macro2Id(it) => it.lookup(db).expander,
            MacroId::MacroRulesId(it) => it.lookup(db).expander,
            MacroId::ProcMacroId(_) => return None,
        };
        match expander {
            MacroExpander::BuiltInDerive(kind) => Some(BuiltinDeriveMacroKind(kind)),
            _ => None,
        }
    }

    pub fn is_env_or_option_env(&self, db: &Db) -> bool {
        match self.id {
            MacroId::Macro2Id(it) => {
                matches!(it.lookup(db).expander, MacroExpander::BuiltInEager(eager) if eager.is_env_or_option_env())
            }
            MacroId::MacroRulesId(it) => {
                matches!(it.lookup(db).expander, MacroExpander::BuiltInEager(eager) if eager.is_env_or_option_env())
            }
            MacroId::ProcMacroId(_) => false,
        }
    }

    /// Is this `asm!()`, or a variant of it (e.g. `global_asm!()`)?
    pub fn is_asm_like(&self, db: &Db) -> bool {
        match self.id {
            MacroId::Macro2Id(it) => {
                matches!(it.lookup(db).expander, MacroExpander::BuiltIn(m) if m.is_asm())
            }
            MacroId::MacroRulesId(it) => {
                matches!(it.lookup(db).expander, MacroExpander::BuiltIn(m) if m.is_asm())
            }
            MacroId::ProcMacroId(_) => false,
        }
    }

    pub fn is_attr(&self, db: &Db) -> bool {
        matches!(self.kind(db), MacroKind::Attr | MacroKind::AttrBuiltIn)
    }

    pub fn is_derive(&self, db: &Db) -> bool {
        matches!(self.kind(db), MacroKind::Derive | MacroKind::DeriveBuiltIn)
    }

    pub fn preferred_brace_style(&self, db: &Db) -> Option<MacroBraces> {
        let attrs = self.attrs(db);
        MacroBraces::extract(attrs.attrs)
    }
}

// Feature: Macro Brace Style Attribute
// Crate authors can declare the preferred brace style for their macro. This will affect how completion
// insert calls to it.
//
// This is only supported on function-like macros.
//
// To do that, insert the `#[rust_analyzer::macro_style(style)]` attribute on the macro (for proc macros,
// insert it for the macro's function). `style` can be one of:
//
//  - `braces` for `{...}` style.
//  - `brackets` for `[...]` style.
//  - `parentheses` for `(...)` style.
//
// Malformed attributes will be ignored without warnings.
//
// Note that users have no way to override this attribute, so be careful and only include things
// users definitely do not want to be completed!

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MacroBraces {
    Braces,
    Brackets,
    Parentheses,
}

impl MacroBraces {
    fn extract(attrs: AttrFlags) -> Option<Self> {
        if attrs.contains(AttrFlags::MACRO_STYLE_BRACES) {
            Some(Self::Braces)
        } else if attrs.contains(AttrFlags::MACRO_STYLE_BRACKETS) {
            Some(Self::Brackets)
        } else if attrs.contains(AttrFlags::MACRO_STYLE_PARENTHESES) {
            Some(Self::Parentheses)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BuiltinDeriveMacroKind(BuiltinDeriveExpander);

impl HasVisibility for Macro {
    fn visibility(&self, db: &Db) -> Visibility {
        match self.id {
            MacroId::Macro2Id(id) => {
                let loc = id.lookup(db);
                let source = loc.source_view(db);
                visibility_from_ast_view(db, id, source.map(|src| src.visibility()))
            }
            MacroId::MacroRulesId(id) => {
                if AttrFlags::query(db, id.into()).contains(AttrFlags::IS_MACRO_EXPORT) {
                    Visibility::Public
                } else {
                    Visibility::PubCrate(self.krate(db).id)
                }
            }
            MacroId::ProcMacroId(_) => Visibility::Public,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ItemInNs {
    Types(ModuleDef),
    Values(ModuleDef),
    Macros(Macro),
}

impl From<Macro> for ItemInNs {
    fn from(it: Macro) -> Self {
        Self::Macros(it)
    }
}

impl_from!(
    ModuleDef {
        Module => Types,
        Function => Values,
        Adt => Types,
        EnumVariant => Types,
        Const => Values,
        Static => Values,
        Trait => Types,
        TypeAlias => Types,
        BuiltinType => Types,
        Macro => Macros,
    }
    for ItemInNs
);

impl ItemInNs {
    pub fn into_module_def(self) -> ModuleDef {
        match self {
            ItemInNs::Types(id) | ItemInNs::Values(id) => id,
            ItemInNs::Macros(id) => ModuleDef::Macro(id),
        }
    }

    /// Returns the crate defining this item (or `None` if `self` is built-in).
    pub fn krate(&self, db: &Db) -> Option<Crate> {
        match self {
            ItemInNs::Types(did) | ItemInNs::Values(did) => did.module(db).map(|m| m.krate(db)),
            ItemInNs::Macros(id) => Some(id.module(db).krate(db)),
        }
    }

    pub fn attrs(&self, db: &Db) -> Option<AttrsWithOwner> {
        match self {
            ItemInNs::Types(it) | ItemInNs::Values(it) => it.attrs(db),
            ItemInNs::Macros(it) => Some(it.attrs(db)),
        }
    }
}

/// Invariant: `inner.as_extern_assoc_item(db).is_some()`
/// We do not actively enforce this invariant.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum ExternAssocItem {
    Function(Function),
    Static(Static),
    TypeAlias(TypeAlias),
}

pub trait AsExternAssocItem {
    fn as_extern_assoc_item(self, db: &Db) -> Option<ExternAssocItem>;
}

impl AsExternAssocItem for Function {
    fn as_extern_assoc_item(self, db: &Db) -> Option<ExternAssocItem> {
        let AnyFunctionId::FunctionId(id) = self.id else {
            return None;
        };
        as_extern_assoc_item(db, ExternAssocItem::Function, id)
    }
}

impl AsExternAssocItem for Static {
    fn as_extern_assoc_item(self, db: &Db) -> Option<ExternAssocItem> {
        as_extern_assoc_item(db, ExternAssocItem::Static, self.id)
    }
}

impl AsExternAssocItem for TypeAlias {
    fn as_extern_assoc_item(self, db: &Db) -> Option<ExternAssocItem> {
        as_extern_assoc_item(db, ExternAssocItem::TypeAlias, self.id)
    }
}

/// Invariant: `inner.as_assoc_item(db).is_some()`
/// We do not actively enforce this invariant.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum AssocItem {
    Function(Function),
    Const(Const),
    TypeAlias(TypeAlias),
}

impl From<method_resolution::CandidateId> for AssocItem {
    fn from(value: method_resolution::CandidateId) -> Self {
        match value {
            method_resolution::CandidateId::FunctionId(id) => AssocItem::Function(id.into()),
            method_resolution::CandidateId::ConstId(id) => AssocItem::Const(Const { id }),
        }
    }
}

#[derive(Debug, Clone)]
pub enum AssocItemContainer {
    Trait(Trait),
    Impl(Impl),
}

pub trait AsAssocItem {
    fn as_assoc_item(self, db: &Db) -> Option<AssocItem>;
}

impl AsAssocItem for Function {
    fn as_assoc_item(self, db: &Db) -> Option<AssocItem> {
        match self.id {
            AnyFunctionId::FunctionId(id) => as_assoc_item(db, AssocItem::Function, id),
            AnyFunctionId::BuiltinDeriveImplMethod { .. } => Some(AssocItem::Function(self)),
        }
    }
}

impl AsAssocItem for Const {
    fn as_assoc_item(self, db: &Db) -> Option<AssocItem> {
        as_assoc_item(db, AssocItem::Const, self.id)
    }
}

impl AsAssocItem for TypeAlias {
    fn as_assoc_item(self, db: &Db) -> Option<AssocItem> {
        as_assoc_item(db, AssocItem::TypeAlias, self.id)
    }
}

impl AsAssocItem for ModuleDef {
    fn as_assoc_item(self, db: &Db) -> Option<AssocItem> {
        match self {
            ModuleDef::Function(it) => it.as_assoc_item(db),
            ModuleDef::Const(it) => it.as_assoc_item(db),
            ModuleDef::TypeAlias(it) => it.as_assoc_item(db),
            _ => None,
        }
    }
}

impl AsAssocItem for DefWithBody {
    fn as_assoc_item(self, db: &Db) -> Option<AssocItem> {
        match self {
            DefWithBody::Function(it) => it.as_assoc_item(db),
            DefWithBody::Const(it) => it.as_assoc_item(db),
            DefWithBody::Static(_) | DefWithBody::EnumVariant(_) => None,
        }
    }
}

impl AsAssocItem for GenericDef {
    fn as_assoc_item(self, db: &Db) -> Option<AssocItem> {
        match self {
            GenericDef::Function(it) => it.as_assoc_item(db),
            GenericDef::Const(it) => it.as_assoc_item(db),
            GenericDef::TypeAlias(it) => it.as_assoc_item(db),
            _ => None,
        }
    }
}

fn as_assoc_item<'db, ID, DEF, LOC>(
    db: &Db,
    ctor: impl FnOnce(DEF) -> AssocItem,
    id: ID,
) -> Option<AssocItem>
where
    ID: Lookup<Data = AssocItemLoc<LOC>>,
    DEF: From<ID>,
    LOC: AstIdNode,
{
    match id.lookup(db).container {
        ItemContainerId::TraitId(_) | ItemContainerId::ImplId(_) => Some(ctor(DEF::from(id))),
        ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => None,
    }
}

fn as_extern_assoc_item<'db, ID, DEF, LOC>(
    db: &Db,
    ctor: impl FnOnce(DEF) -> ExternAssocItem,
    id: ID,
) -> Option<ExternAssocItem>
where
    ID: Lookup<Data = AssocItemLoc<LOC>>,
    DEF: From<ID>,
    LOC: AstIdNode,
{
    match id.lookup(db).container {
        ItemContainerId::ExternBlockId(_) => Some(ctor(DEF::from(id))),
        ItemContainerId::TraitId(_) | ItemContainerId::ImplId(_) | ItemContainerId::ModuleId(_) => {
            None
        }
    }
}

impl ExternAssocItem {
    pub fn name(self, db: &Db) -> Name {
        match self {
            Self::Function(it) => it.name(db),
            Self::Static(it) => it.name(db),
            Self::TypeAlias(it) => it.name(db),
        }
    }

    pub fn module(self, db: &Db) -> Module {
        match self {
            Self::Function(f) => f.module(db),
            Self::Static(c) => c.module(db),
            Self::TypeAlias(t) => t.module(db),
        }
    }

    pub fn as_function(self) -> Option<Function> {
        match self {
            Self::Function(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_static(self) -> Option<Static> {
        match self {
            Self::Static(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_type_alias(self) -> Option<TypeAlias> {
        match self {
            Self::TypeAlias(v) => Some(v),
            _ => None,
        }
    }
}

impl AssocItem {
    pub fn name(self, db: &Db) -> Option<Name> {
        match self {
            AssocItem::Function(it) => Some(it.name(db)),
            AssocItem::Const(it) => it.name(db),
            AssocItem::TypeAlias(it) => Some(it.name(db)),
        }
    }

    pub fn module(self, db: &Db) -> Module {
        match self {
            AssocItem::Function(f) => f.module(db),
            AssocItem::Const(c) => c.module(db),
            AssocItem::TypeAlias(t) => t.module(db),
        }
    }

    pub fn container(self, db: &Db) -> AssocItemContainer {
        let container = match self {
            AssocItem::Function(it) => match it.id {
                AnyFunctionId::FunctionId(id) => id.lookup(db).container,
                AnyFunctionId::BuiltinDeriveImplMethod { impl_, .. } => {
                    return AssocItemContainer::Impl(Impl {
                        id: AnyImplId::BuiltinDeriveImplId(impl_),
                    });
                }
            },
            AssocItem::Const(it) => it.id.lookup(db).container,
            AssocItem::TypeAlias(it) => it.id.lookup(db).container,
        };
        match container {
            ItemContainerId::TraitId(id) => AssocItemContainer::Trait(id.into()),
            ItemContainerId::ImplId(id) => AssocItemContainer::Impl(id.into()),
            ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => {
                panic!("invalid AssocItem")
            }
        }
    }

    pub fn container_trait(self, db: &Db) -> Option<Trait> {
        match self.container(db) {
            AssocItemContainer::Trait(t) => Some(t),
            _ => None,
        }
    }

    pub fn implemented_trait(self, db: &Db) -> Option<Trait> {
        match self.container(db) {
            AssocItemContainer::Impl(i) => i.trait_(db),
            _ => None,
        }
    }

    pub fn container_or_implemented_trait(self, db: &Db) -> Option<Trait> {
        match self.container(db) {
            AssocItemContainer::Trait(t) => Some(t),
            AssocItemContainer::Impl(i) => i.trait_(db),
        }
    }

    pub fn implementing_ty(self, db: &Db) -> Option<Type<'_>> {
        match self.container(db) {
            AssocItemContainer::Impl(i) => Some(i.self_ty(db)),
            _ => None,
        }
    }

    pub fn as_function(self) -> Option<Function> {
        match self {
            Self::Function(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_const(self) -> Option<Const> {
        match self {
            Self::Const(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_type_alias(self) -> Option<TypeAlias> {
        match self {
            Self::TypeAlias(v) => Some(v),
            _ => None,
        }
    }
}

impl HasVisibility for AssocItem {
    fn visibility(&self, db: &Db) -> Visibility {
        match self {
            AssocItem::Function(f) => f.visibility(db),
            AssocItem::Const(c) => c.visibility(db),
            AssocItem::TypeAlias(t) => t.visibility(db),
        }
    }
}

impl_from!(AssocItem { Function, Const, TypeAlias } for ModuleDef);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum GenericDef {
    Function(Function),
    Adt(Adt),
    Trait(Trait),
    TypeAlias(TypeAlias),
    Impl(Impl),
    // consts can have type parameters from their parents (i.e. associated consts of traits)
    Const(Const),
    Static(Static),
}
impl_from!(
    Function,
    Adt(Struct, Enum, Union),
    Trait,
    TypeAlias,
    Impl,
    Const,
    Static
    for GenericDef
);

impl GenericDef {
    pub fn name(self, db: &Db) -> Option<Name> {
        match self {
            GenericDef::Function(it) => Some(it.name(db)),
            GenericDef::Adt(it) => Some(it.name(db)),
            GenericDef::Trait(it) => Some(it.name(db)),
            GenericDef::TypeAlias(it) => Some(it.name(db)),
            GenericDef::Impl(_) => None,
            GenericDef::Const(it) => it.name(db),
            GenericDef::Static(it) => Some(it.name(db)),
        }
    }

    pub fn module(self, db: &Db) -> Module {
        match self {
            GenericDef::Function(it) => it.module(db),
            GenericDef::Adt(it) => it.module(db),
            GenericDef::Trait(it) => it.module(db),
            GenericDef::TypeAlias(it) => it.module(db),
            GenericDef::Impl(it) => it.module(db),
            GenericDef::Const(it) => it.module(db),
            GenericDef::Static(it) => it.module(db),
        }
    }

    pub fn params(self, db: &Db) -> Vec<GenericParam> {
        let Ok(id) = self.try_into() else {
            // Let's pretend builtin derive impls don't have generic parameters.
            return Vec::new(); // alloc: empty
        };
        let generics = GenericParams::of(db, id);
        let ty_params = generics.iter_type_or_consts().map(|(local_id, _)| {
            let toc = TypeOrConstParam { id: TypeOrConstParamId { parent: id, local_id } };
            match toc.split(db) {
                Either::Left(it) => GenericParam::ConstParam(it),
                Either::Right(it) => GenericParam::TypeParam(it),
            }
        });
        generics
            .iter_lt()
            .map(|(local_id, _)| {
                GenericParam::LifetimeParam(LifetimeParam {
                    id: LifetimeParamId { parent: id, local_id },
                })
            })
            .chain(ty_params)
            .collect() // alloc: editor edge, the owned parameter list callers take
    }

    pub fn lifetime_params(self, db: &Db) -> Vec<LifetimeParam> {
        let Ok(id) = self.try_into() else {
            // Let's pretend builtin derive impls don't have generic parameters.
            return Vec::new(); // alloc: empty
        };
        let generics = GenericParams::of(db, id);
        generics
            .iter_lt()
            .map(|(local_id, _)| LifetimeParam { id: LifetimeParamId { parent: id, local_id } })
            .collect() // alloc: editor edge
    }

    pub fn type_or_const_params(self, db: &Db) -> Vec<TypeOrConstParam> {
        let Ok(id) = self.try_into() else {
            // Let's pretend builtin derive impls don't have generic parameters.
            return Vec::new(); // alloc: empty
        };
        let generics = GenericParams::of(db, id);
        generics
            .iter_type_or_consts()
            .map(|(local_id, _)| TypeOrConstParam {
                id: TypeOrConstParamId { parent: id, local_id },
            })
            .collect() // alloc: editor edge
    }

    /// Every predicate of its own where clauses and parameter bounds, not its parent's, in the
    /// order written, then each type parameter's implicit `Sized` (rustc's
    /// `explicit_predicates_of` has them too).
    pub fn where_predicates(self, db: &Db) -> Vec<WherePredicate<'_>> {
        let Some(id) = self.id() else {
            return Vec::new(); // alloc: empty
        };
        let interner = DbInterner::new_no_crate(db);
        let owner = TypeOwnerId::GenericDefId(id);
        GenericPredicates::query_own_explicit(db, id)
            .iter_identity()
            .map(|clause| {
                let clause = rustc_type_ir::set_aliases_to_non_rigid(interner, clause.skip_norm_wip())
                    .skip_normalization();
                WherePredicate { clause, owner }
            })
            .collect() // alloc: editor edge, the owned predicate list callers take
    }

    fn id(self) -> Option<GenericDefId> {
        Some(match self {
            GenericDef::Function(it) => match it.id {
                AnyFunctionId::FunctionId(it) => it.into(),
                AnyFunctionId::BuiltinDeriveImplMethod { .. } => return None,
            },
            GenericDef::Adt(it) => it.into(),
            GenericDef::Trait(it) => it.id.into(),
            GenericDef::TypeAlias(it) => it.id.into(),
            GenericDef::Impl(it) => match it.id {
                AnyImplId::ImplId(it) => it.into(),
                AnyImplId::BuiltinDeriveImplId(_) => return None,
            },
            GenericDef::Const(it) => it.id.into(),
            GenericDef::Static(it) => it.id.into(),
        })
    }

    /// Returns a string describing the kind of this type.
    #[inline]
    pub fn description(self) -> &'static str {
        match self {
            GenericDef::Function(_) => "function",
            GenericDef::Adt(Adt::Struct(_)) => "struct",
            GenericDef::Adt(Adt::Enum(_)) => "enum",
            GenericDef::Adt(Adt::Union(_)) => "union",
            GenericDef::Trait(_) => "trait",
            GenericDef::TypeAlias(_) => "type alias",
            GenericDef::Impl(_) => "impl",
            GenericDef::Const(_) => "constant",
            GenericDef::Static(_) => "static",
        }
    }
}

// We cannot call this `Substitution` unfortunately...
#[derive(Debug)]
pub struct GenericSubstitution<'db> {
    owner: TypeOwnerId,
    def: GenericDefId,
    subst: GenericArgs<'db>,
}

impl<'db> GenericSubstitution<'db> {
    fn new(def: GenericDefId, subst: GenericArgs<'db>, owner: TypeOwnerId) -> Self {
        Self { owner, def, subst }
    }

    fn new_from_fn(def: Function, subst: GenericArgs<'db>, owner: TypeOwnerId) -> Option<Self> {
        match def.id {
            AnyFunctionId::FunctionId(def) => Some(Self::new(def.into(), subst, owner)),
            AnyFunctionId::BuiltinDeriveImplMethod { .. } => None,
        }
    }

    fn types_impl(
        &self,
        db: &'db Db,
    ) -> impl Iterator<Item = (Ty<'db>, Option<Name>)> {
        let container = match self.def {
            GenericDefId::ConstId(id) => Some(id.lookup(db).container),
            GenericDefId::FunctionId(id) => Some(id.lookup(db).container),
            GenericDefId::TypeAliasId(id) => Some(id.lookup(db).container),
            _ => None,
        };
        let container_type_params = container
            .and_then(|container| match container {
                ItemContainerId::ImplId(container) => Some(container.into()),
                ItemContainerId::TraitId(container) => Some(container.into()),
                _ => None,
            })
            .map(|container| {
                GenericParams::of(db, container).iter_type_or_consts().filter_map(|param| {
                    match param.1 {
                        TypeOrConstParamData::TypeParamData(param) => Some(param.name),
                        TypeOrConstParamData::ConstParamData(_) => None,
                    }
                })
            });
        let generics = GenericParams::of(db, self.def);
        let type_params = generics.iter_type_or_consts().filter_map(|param| match param.1 {
            TypeOrConstParamData::TypeParamData(param) => Some(param.name),
            TypeOrConstParamData::ConstParamData(_) => None,
        });
        self.subst.types().zip(container_type_params.into_iter().flatten().chain(type_params))
    }

    /// Returns type arguments with type parameter names.
    pub fn types(&self, db: &'db Db) -> Vec<(Symbol, Type<'db>)> {
        let interner = DbInterner::new_no_crate(db);
        self.types_impl(db)
            .filter_map(|(ty, name)| {
                Some((
                    *name?.symbol(),
                    Type { ty: EarlyBinder::bind(interner, ty), owner: self.owner },
                ))
            })
            .collect() // alloc: editor edge, the owned argument list callers take
    }

    /// Returns all type arguments, including unnamed ones such as implicit `impl Trait` parameters.
    ///
    /// Unlike [`Self::types`], this does not filter out arguments that have no
    /// corresponding type parameter name.
    // Note: this method is not used by rust-analyzer itself as of writing, but is
    // used by external tools, so please do not remove it even if it appears unused.
    pub fn all_types(&self, db: &'db Db) -> Vec<Type<'db>> {
        let interner = DbInterner::new_no_crate(db);
        self.types_impl(db)
            .map(|(ty, _)| Type { ty: EarlyBinder::bind(interner, ty), owner: self.owner })
            .collect() // alloc: editor edge
    }
}

/// A single local definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Local<'db> {
    pub(crate) parent: ExpressionStoreOwnerId,
    pub(crate) parent_infer: InferBodyId<'db>,
    pub(crate) binding_id: BindingId,
}

impl<'db> Local<'db> {
    pub fn as_self_param(self, db: &Db) -> Option<SelfParam> {
        match self.parent {
            ExpressionStoreOwnerId::Body(DefWithBodyId::FunctionId(func)) if self.is_self(db) => {
                Some(SelfParam { func: func.into() })
            }
            _ => None,
        }
    }

    pub fn name(self, db: &Db) -> Name {
        ExpressionStore::of(db, self.parent)[self.binding_id].name
    }

    pub fn is_self(self, db: &Db) -> bool {
        self.name(db) == sym::self_
    }

    pub fn is_mut(self, db: &Db) -> bool {
        ExpressionStore::of(db, self.parent)[self.binding_id].mode == BindingAnnotation::Mutable
    }

    pub fn is_ref(self, db: &Db) -> bool {
        matches!(
            ExpressionStore::of(db, self.parent)[self.binding_id].mode,
            BindingAnnotation::Ref | BindingAnnotation::RefMut
        )
    }

    pub fn parent(self, _db: &Db) -> ExpressionStoreOwner {
        self.parent.into()
    }

    pub fn module(self, db: &Db) -> Module {
        self.parent(db).module(db)
    }

    pub fn as_id(self) -> u32 {
        self.binding_id.into_raw().into_u32()
    }

    pub fn ty(self, db: &'db Db) -> Type<'db> {
        let def = self.parent;
        let infer = InferenceResult::of(db, self.parent_infer);
        let ty = infer.binding_ty(self.binding_id);
        Type::new_body(db, def, ty)
    }

}

impl PartialOrd for Local<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Local<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.binding_id.cmp(&other.binding_id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeriveHelper {
    pub(crate) derive: MacroId,
    pub(crate) idx: u32,
}

impl DeriveHelper {
    pub fn derive(&self) -> Macro {
        Macro { id: self.derive }
    }

    pub fn name(&self, db: &Db) -> Name {
        AttrFlags::derive_info(db, self.derive)
            .and_then(|it| it.helpers.get(self.idx as usize))
            .map(|helper| Name::new_symbol_root(*helper))
            .unwrap_or_else(Name::missing)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BuiltinAttr {
    idx: u32,
}

impl BuiltinAttr {
    fn builtin(name: &str) -> Option<Self> {
        hir_expand::inert_attr_macro::find_builtin_attr_idx(&Symbol::intern(name))
            .map(|idx| BuiltinAttr { idx: idx as u32 })
    }

    pub fn name(&self) -> Name {
        Name::new_symbol_root(Symbol::intern(
            hir_expand::inert_attr_macro::INERT_ATTRIBUTES[self.idx as usize].name,
        ))
    }

    pub fn template(&self) -> Option<AttributeTemplate> {
        Some(hir_expand::inert_attr_macro::INERT_ATTRIBUTES[self.idx as usize].template)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ToolModule {
    krate: base_db::Crate,
    idx: u32,
}

impl ToolModule {
    pub(crate) fn by_name(db: &Db, krate: Crate, name: &str) -> Option<Self> {
        let krate = krate.id;
        let idx =
            crate_def_map(db, krate).registered_tools().iter().position(|it| it.as_str() == name)?
                as u32;
        Some(ToolModule { krate, idx })
    }

    pub fn name(&self, db: &Db) -> Name {
        Name::new_symbol_root(crate_def_map(db, self.krate).registered_tools()[self.idx as usize])
    }

    pub fn krate(&self) -> Crate {
        Crate { id: self.krate }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Label {
    pub(crate) parent: ExpressionStoreOwnerId,
    pub(crate) label_id: LabelId,
}

impl Label {
    pub fn module(self, db: &Db) -> Module {
        self.parent(db).module(db)
    }

    pub fn parent(self, _db: &Db) -> ExpressionStoreOwner {
        self.parent.into()
    }

    pub fn name(self, db: &Db) -> Name {
        ExpressionStore::of(db, self.parent)[self.label_id].name
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GenericParam {
    TypeParam(TypeParam),
    ConstParam(ConstParam),
    LifetimeParam(LifetimeParam),
}
impl_from!(TypeParam, ConstParam, LifetimeParam for GenericParam);

impl GenericParam {
    pub fn module(self, db: &Db) -> Module {
        match self {
            GenericParam::TypeParam(it) => it.module(db),
            GenericParam::ConstParam(it) => it.module(db),
            GenericParam::LifetimeParam(it) => it.module(db),
        }
    }

    pub fn name(self, db: &Db) -> Name {
        match self {
            GenericParam::TypeParam(it) => it.name(db),
            GenericParam::ConstParam(it) => it.name(db),
            GenericParam::LifetimeParam(it) => it.name(db),
        }
    }

    pub fn parent(self) -> GenericDef {
        match self {
            GenericParam::TypeParam(it) => it.id.parent().into(),
            GenericParam::ConstParam(it) => it.id.parent().into(),
            GenericParam::LifetimeParam(it) => it.id.parent.into(),
        }
    }

    pub fn variance(self, db: &Db) -> Option<Variance> {
        let parent = match self {
            GenericParam::TypeParam(it) => it.id.parent(),
            // const parameters are always invariant
            GenericParam::ConstParam(_) => return None,
            GenericParam::LifetimeParam(it) => it.id.parent,
        };
        let index = match self {
            GenericParam::TypeParam(it) => hir_ty::type_or_const_param_idx(db, it.id.into()),
            GenericParam::ConstParam(_) => return None,
            GenericParam::LifetimeParam(it) => hir_ty::lifetime_param_idx(db, it.id),
        };
        db.variances_of(parent).get(index as usize).map(Into::into)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variance {
    Bivariant,
    Covariant,
    Contravariant,
    Invariant,
}

impl From<rustc_type_ir::Variance> for Variance {
    #[inline]
    fn from(value: rustc_type_ir::Variance) -> Self {
        match value {
            rustc_type_ir::Variance::Covariant => Variance::Covariant,
            rustc_type_ir::Variance::Invariant => Variance::Invariant,
            rustc_type_ir::Variance::Contravariant => Variance::Contravariant,
            rustc_type_ir::Variance::Bivariant => Variance::Bivariant,
        }
    }
}

impl fmt::Display for Variance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let description = match self {
            Variance::Bivariant => "bivariant",
            Variance::Covariant => "covariant",
            Variance::Contravariant => "contravariant",
            Variance::Invariant => "invariant",
        };
        f.pad(description)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TypeParam {
    pub(crate) id: TypeParamId,
}

impl TypeParam {
    pub fn merge(self) -> TypeOrConstParam {
        TypeOrConstParam { id: self.id.into() }
    }

    pub fn name(self, db: &Db) -> Name {
        self.merge().name(db)
    }

    pub fn parent(self, _db: &Db) -> GenericDef {
        self.id.parent().into()
    }

    pub fn module(self, db: &Db) -> Module {
        self.id.parent().module(db).into()
    }

    /// Is this type parameter implicitly introduced (eg. `Self` in a trait or an `impl Trait`
    /// argument)?
    pub fn is_implicit(self, db: &Db) -> bool {
        let params = GenericParams::of(db, self.id.parent());
        let data = &params[self.id.local_id()];
        match data.type_param().unwrap().provenance {
            TypeParamProvenance::TypeParamList => false,
            TypeParamProvenance::TraitSelf | TypeParamProvenance::ArgumentImplTrait => true,
        }
    }

    pub fn ty(self, db: &Db) -> Type<'_> {
        let interner = DbInterner::new_no_crate(db);
        let index = hir_ty::type_or_const_param_idx(db, self.id.into());
        let ty = Ty::new_param(interner, self.id, index);
        Type::new(db, self.id.parent(), ty)
    }

    /// FIXME: this only lists trait bounds from the item defining the type
    /// parameter, not additional bounds that might be added e.g. by a method if
    /// the parameter comes from an impl!
    pub fn trait_bounds(self, db: &Db) -> Vec<Trait> {
        let self_ty = self.ty(db).ty.instantiate_identity().skip_norm_wip();
        GenericPredicates::query_explicit(db, self.id.parent())
            .iter_identity()
            .filter_map(|pred| match &pred.kind().skip_binder() {
                ClauseKind::Trait(trait_ref) if trait_ref.self_ty() == self_ty => {
                    Some(Trait::from(trait_ref.def_id().0))
                }
                _ => None,
            })
            .collect() // alloc: editor edge, the owned bound list callers take
    }

    /// [`TypeParam::trait_bounds`] with the bounds' generic arguments, for blaze's lints.
    pub fn trait_refs(self, db: &Db) -> Vec<TraitRef<'_>> {
        let interner = DbInterner::new_no_crate(db);
        let owner = TypeOwnerId::GenericDefId(self.id.parent());
        let self_ty = self.ty(db).ty.instantiate_identity().skip_norm_wip();
        GenericPredicates::query_explicit(db, self.id.parent())
            .iter_identity()
            .filter_map(|pred| match &pred.kind().skip_binder() {
                ClauseKind::Trait(trait_ref) if trait_ref.self_ty() == self_ty => {
                    Some(TraitRef::new(interner, owner, trait_ref.trait_ref))
                }
                _ => None,
            })
            // alloc: lints ask only for a bare type parameter's argument, and walk it twice
            .collect()
    }

    pub fn default(self, db: &Db) -> Option<Type<'_>> {
        let ty = generic_arg_from_param(db, self.id.into())?;
        match ty.kind() {
            rustc_type_ir::GenericArgKind::Type(it) if !it.is_ty_error() => {
                Some(Type::new(db, self.id.parent(), it))
            }
            _ => None,
        }
    }

    pub fn is_unstable(self, db: &Db) -> bool {
        self.attrs(db).is_unstable()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LifetimeParam {
    pub(crate) id: LifetimeParamId,
}

impl LifetimeParam {
    pub fn name(self, db: &Db) -> Name {
        let params = GenericParams::of(db, self.id.parent);
        params[self.id.local_id].name
    }

    pub fn module(self, db: &Db) -> Module {
        self.id.parent.module(db).into()
    }

    pub fn parent(self, _db: &Db) -> GenericDef {
        self.id.parent.into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConstParam {
    pub(crate) id: ConstParamId,
}

impl ConstParam {
    pub fn merge(self) -> TypeOrConstParam {
        TypeOrConstParam { id: self.id.into() }
    }

    pub fn name(self, db: &Db) -> Name {
        let params = GenericParams::of(db, self.id.parent());
        match params[self.id.local_id()].name() {
            Some(it) => *it,
            None => {
                never!();
                Name::missing()
            }
        }
    }

    pub fn module(self, db: &Db) -> Module {
        self.id.parent().module(db).into()
    }

    pub fn parent(self, _db: &Db) -> GenericDef {
        self.id.parent().into()
    }

    pub fn ty(self, db: &Db) -> Type<'_> {
        Type::new(db, self.id.parent(), db.const_param_ty(self.id))
    }

    pub fn default(self, db: &Db, display_target: DisplayTarget) -> Option<String> {
        let arg = generic_arg_from_param(db, self.id.into())?;
        Some(arg.display(db, display_target).to_string()) // alloc: editor edge, for display
    }
}

fn generic_arg_from_param(db: &Db, id: TypeOrConstParamId) -> Option<GenericArg<'_>> {
    let local_idx = hir_ty::type_or_const_param_idx(db, id);
    let defaults = db.generic_defaults(id.parent);
    let ty = defaults.get(local_idx as usize)?;
    // FIXME: This shouldn't be `instantiate_identity()`, we shouldn't leak `TyKind::Param`s.
    Some(ty.instantiate_identity().skip_norm_wip())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TypeOrConstParam {
    pub(crate) id: TypeOrConstParamId,
}

impl TypeOrConstParam {
    pub fn name(self, db: &Db) -> Name {
        let params = GenericParams::of(db, self.id.parent);
        match params[self.id.local_id].name() {
            Some(n) => *n,
            _ => Name::missing(),
        }
    }

    pub fn module(self, db: &Db) -> Module {
        self.id.parent.module(db).into()
    }

    pub fn parent(self, _db: &Db) -> GenericDef {
        self.id.parent.into()
    }

    pub fn split(self, db: &Db) -> Either<ConstParam, TypeParam> {
        let params = GenericParams::of(db, self.id.parent);
        match &params[self.id.local_id] {
            TypeOrConstParamData::TypeParamData(_) => {
                Either::Right(TypeParam { id: TypeParamId::from_unchecked(self.id) })
            }
            TypeOrConstParamData::ConstParamData(_) => {
                Either::Left(ConstParam { id: ConstParamId::from_unchecked(self.id) })
            }
        }
    }

    pub fn ty(self, db: &Db) -> Type<'_> {
        match self.split(db) {
            Either::Left(it) => it.ty(db),
            Either::Right(it) => it.ty(db),
        }
    }

    pub fn as_type_param(self, db: &Db) -> Option<TypeParam> {
        let params = GenericParams::of(db, self.id.parent);
        match &params[self.id.local_id] {
            TypeOrConstParamData::TypeParamData(_) => {
                Some(TypeParam { id: TypeParamId::from_unchecked(self.id) })
            }
            TypeOrConstParamData::ConstParamData(_) => None,
        }
    }

    pub fn as_const_param(self, db: &Db) -> Option<ConstParam> {
        let params = GenericParams::of(db, self.id.parent);
        match &params[self.id.local_id] {
            TypeOrConstParamData::TypeParamData(_) => None,
            TypeOrConstParamData::ConstParamData(_) => {
                Some(ConstParam { id: ConstParamId::from_unchecked(self.id) })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Impl {
    pub(crate) id: AnyImplId,
}

impl Impl {
    pub fn all_in_crate(db: &Db, krate: Crate) -> Vec<Impl> {
        let mut result = Vec::new(); // alloc: editor edge, the owned impl list callers take
        extend_with_def_map(db, crate_def_map(db, krate.id), &mut result);
        return result;

        fn extend_with_def_map(db: &Db, def_map: &DefMap, result: &mut Vec<Impl>) {
            for (_, module) in def_map.modules(db) {
                result.extend(module.scope(db).impls().map(Impl::from));
                result.extend(module.scope(db).builtin_derive_impls().map(Impl::from));

                for unnamed_const in module.scope(db).unnamed_consts() {
                    for (_, block_def_map) in Body::of(db, unnamed_const.into()).blocks(db) {
                        extend_with_def_map(db, block_def_map, result);
                    }
                }
            }
        }
    }

    pub fn all_in_module(db: &Db, module: Module) -> Vec<Impl> {
        module.impl_defs(db)
    }

    /// **Note:** This is an **approximation** that strives to give the *human-perceived notion* of an "impl for type",
    /// **not** answer the technical question "what are all impls applying to this type". In particular, it excludes
    /// blanket impls, and only does a shallow type constructor check. In fact, this should've probably been on `Adt`
    /// etc., and not on `Type`. If you would want to create a precise list of all impls applying to a type,
    /// you would need to include blanket impls, and try to prove to predicates for each candidate.
    pub fn all_for_type<'db>(db: &'db Db, ty: Type<'db>) -> Vec<Impl> {
        let mut result = Vec::new(); // alloc: editor edge, the owned impl list callers take
        let interner = DbInterner::new_no_crate(db);
        let Some(simplified_ty) = fast_reject::simplify_type(
            interner,
            ty.ty.skip_binder(),
            fast_reject::TreatParams::AsRigid,
        ) else {
            return Vec::new(); // alloc: empty
        };
        let mut extend_with_impls = |impls: Either<&[ImplId], &[BuiltinDeriveImplId]>| match impls {
            Either::Left(impls) => result.extend(impls.iter().copied().map(Impl::from)),
            Either::Right(impls) => result.extend(impls.iter().copied().map(Impl::from)),
        };
        method_resolution::with_incoherent_inherent_impls(
            db,
            ty.krate(db),
            &simplified_ty,
            |impls| extend_with_impls(Either::Left(impls)),
        );
        if let Some(module) = method_resolution::simplified_type_module(db, &simplified_ty) {
            InherentImpls::for_each_crate_and_block(
                db,
                module.krate(db),
                module.block(db),
                &mut |impls| extend_with_impls(Either::Left(impls.for_self_ty(&simplified_ty))),
            );
            std::iter::successors(module.block(db), |block| block.module(db).block(db))
                .filter_map(|block| TraitImpls::for_block(db, block))
                .for_each(|impls| impls.for_self_ty(&simplified_ty, &mut extend_with_impls));
            for &krate in &*all_crates(db) {
                TraitImpls::for_crate(db, krate)
                    .for_self_ty(&simplified_ty, &mut extend_with_impls);
            }
        } else {
            for &krate in &*all_crates(db) {
                TraitImpls::for_crate(db, krate)
                    .for_self_ty(&simplified_ty, &mut extend_with_impls);
            }
        }
        // An impl in a block is one of its crate as well as of the block.
        let mut seen = HashSet::default(); // alloc: editor edge, sized by the impls found
        result.retain(|it| seen.insert(*it));
        result
    }

    pub fn all_for_trait(db: &Db, trait_: Trait) -> Vec<Impl> {
        let module = trait_.module(db).id;
        let mut all = Vec::new(); // alloc: editor edge, the owned impl list callers take
        let mut handle_impls = |impls: &TraitImpls<'_>| {
            impls.for_trait(trait_.id, |impls| match impls {
                Either::Left(impls) => all.extend(impls.iter().copied().map(Impl::from)),
                Either::Right(impls) => all.extend(impls.iter().copied().map(Impl::from)),
            });
        };
        for krate in module.krate(db).transitive_rev_deps(db).iter().copied() {
            handle_impls(TraitImpls::for_crate(db, krate));
        }
        if let Some(block) = module.block(db)
            && let Some(impls) = TraitImpls::for_block(db, block)
        {
            handle_impls(impls);
        }
        // An impl in a block is one of its crate as well as of the block.
        let mut seen = HashSet::default(); // alloc: editor edge, sized by the impls found
        all.retain(|it| seen.insert(*it));
        all
    }

    pub fn trait_(self, db: &Db) -> Option<Trait> {
        match self.id {
            AnyImplId::ImplId(id) => {
                let trait_ref = db.impl_trait(id)?;
                let id = trait_ref.skip_binder().def_id;
                Some(Trait { id: id.0 })
            }
            AnyImplId::BuiltinDeriveImplId(id) => {
                let loc = id.loc(db);
                let lang_items = hir_def::lang_item::lang_items(db, loc.adt.module(db).krate(db));
                loc.trait_.get_id(lang_items).map(Trait::from)
            }
        }
    }

    pub fn trait_ref(self, db: &Db) -> Option<TraitRef<'_>> {
        match self.id {
            AnyImplId::ImplId(id) => {
                let trait_ref = db.impl_trait(id)?.instantiate_identity().skip_norm_wip();
                let owner = TypeOwnerId::GenericDefId(id.into());
                Some(TraitRef::new(DbInterner::new_no_crate(db), owner, trait_ref))
            }
            AnyImplId::BuiltinDeriveImplId(id) => {
                let loc = id.loc(db);
                let krate = loc.module(db).krate(db);
                let interner = DbInterner::new_with(db, krate);
                let trait_ref = hir_ty::builtin_derive::impl_trait(interner, id)
                    .instantiate_identity()
                    .skip_norm_wip();
                Some(TraitRef::new(interner, TypeOwnerId::BuiltinDeriveImplId(id), trait_ref))
            }
        }
    }

    pub fn self_ty(self, db: &Db) -> Type<'_> {
        match self.id {
            AnyImplId::ImplId(id) => {
                let ty = db.impl_self_ty(id).instantiate_identity().skip_norm_wip();
                Type::new(db, id.into(), ty)
            }
            AnyImplId::BuiltinDeriveImplId(id) => {
                let loc = id.loc(db);
                let krate = loc.module(db).krate(db);
                let interner = DbInterner::new_with(db, krate);
                let ty =
                    hir_ty::builtin_derive::impl_trait(interner, id).map_bound(|it| it.self_ty());
                Type { owner: TypeOwnerId::BuiltinDeriveImplId(id), ty }
            }
        }
    }

    pub fn items(self, db: &Db) -> Vec<AssocItem> {
        // alloc: editor edge; hot callers use `items_iter`
        self.items_iter(db).collect()
    }

    /// [`Impl::items`], without the vector.
    pub fn items_iter(self, db: &Db) -> impl Iterator<Item = AssocItem> + '_ {
        match self.id {
            AnyImplId::ImplId(id) => {
                Either::Left(id.impl_items(db).items.iter().map(|&(_, it)| it.into()))
            }
            AnyImplId::BuiltinDeriveImplId(impl_) => Either::Right(
                impl_.loc(db).trait_.all_methods().iter().map(move |&method| {
                    AssocItem::Function(Function {
                        id: AnyFunctionId::BuiltinDeriveImplMethod { method, impl_ },
                    })
                }),
            ),
        }
    }

    pub fn is_negative(self, db: &Db) -> bool {
        match self.id {
            AnyImplId::ImplId(id) => ImplSignature::of(db, id).flags.contains(ImplFlags::NEGATIVE),
            AnyImplId::BuiltinDeriveImplId(_) => false,
        }
    }

    pub fn is_unsafe(self, db: &Db) -> bool {
        match self.id {
            AnyImplId::ImplId(id) => ImplSignature::of(db, id).flags.contains(ImplFlags::UNSAFE),
            AnyImplId::BuiltinDeriveImplId(_) => false,
        }
    }

    pub fn module(self, db: &Db) -> Module {
        match self.id {
            AnyImplId::ImplId(id) => id.module(db).into(),
            AnyImplId::BuiltinDeriveImplId(id) => id.module(db).into(),
        }
    }

    pub fn check_orphan_rules(self, db: &Db) -> bool {
        match self.id {
            AnyImplId::ImplId(id) => check_orphan_rules(db, id),
            AnyImplId::BuiltinDeriveImplId(_) => true,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct TraitRef<'db> {
    owner: TypeOwnerId,
    /// Holds no rigid aliases, so that its types can be bound without an interner.
    trait_ref: hir_ty::next_solver::TraitRef<'db>,
}

impl<'db> TraitRef<'db> {
    fn new(
        interner: DbInterner<'db>,
        owner: TypeOwnerId,
        trait_ref: hir_ty::next_solver::TraitRef<'db>,
    ) -> Self {
        let trait_ref = rustc_type_ir::set_aliases_to_non_rigid(interner, trait_ref);
        Self { owner, trait_ref: trait_ref.skip_normalization() }
    }

    pub fn trait_(&self) -> Trait {
        Trait { id: self.trait_ref.def_id.0 }
    }

    pub fn self_ty(&self) -> Type<'_> {
        let ty = self.trait_ref.self_ty();
        Type { owner: self.owner, ty: EarlyBinder::bind_no_rigid_aliases(ty) }
    }

    /// Returns `idx`-th argument of this trait reference if it is a type argument. Note that the
    /// first argument is the `Self` type.
    pub fn get_type_argument(&self, idx: usize) -> Option<Type<'db>> {
        self.trait_ref
            .args
            .as_slice()
            .get(idx)
            .and_then(|arg| arg.ty())
            .map(|ty| Type { owner: self.owner, ty: EarlyBinder::bind_no_rigid_aliases(ty) })
    }

    /// The trait ref as the front end has it, for rustc's printing outside `hir`.
    pub fn to_next_solver(&self) -> hir_ty::next_solver::TraitRef<'db> {
        self.trait_ref
    }

    /// A type of the same owner as the trait ref. `ty` holds no rigid aliases, as
    /// [`Type::derived_from_next_solver`]'s.
    pub fn derived_from_next_solver(&self, ty: Ty<'db>) -> Type<'db> {
        Type { owner: self.owner, ty: EarlyBinder::bind_no_rigid_aliases(ty) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum AnyClosureId<'db> {
    ClosureId(InternedClosureId<'db>),
    CoroutineClosureId(InternedCoroutineClosureId<'db>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Closure<'db> {
    owner: TypeOwnerId,
    id: AnyClosureId<'db>,
    subst: GenericArgs<'db>,
}

impl<'db> Closure<'db> {
    fn as_ty(&self, db: &'db Db) -> Ty<'db> {
        let interner = DbInterner::new_no_crate(db);
        match self.id {
            AnyClosureId::ClosureId(id) => Ty::new_closure(interner, id.into(), self.subst),
            AnyClosureId::CoroutineClosureId(id) => {
                Ty::new_coroutine_closure(interner, id.into(), self.subst)
            }
        }
    }

    pub fn display_with_id(&self, db: &Db, display_target: DisplayTarget) -> String {
        self.as_ty(db)
            .display(db, display_target)
            .with_closure_style(ClosureStyle::ClosureWithId)
            .to_string() // alloc: editor edge, rendered for display
    }

    pub fn display_with_impl(&self, db: &Db, display_target: DisplayTarget) -> String {
        self.as_ty(db)
            .display(db, display_target)
            .with_closure_style(ClosureStyle::ImplFn)
            .to_string() // alloc: editor edge, rendered for display
    }

    pub fn captured_items(&self, db: &'db Db) -> Vec<ClosureCapture<'db>> {
        let closure = match self.id {
            AnyClosureId::ClosureId(it) => it.loc(db),
            AnyClosureId::CoroutineClosureId(it) => it.loc(db),
        };
        captured_items(db, closure)
    }

    /// rustc's `closure_kind_origins`: for a closure whose kind was inferred `FnMut` or
    /// `FnOnce`, the use that made it so and the capture that use is of.
    pub fn kind_origin(
        &self,
        db: &'db Db,
    ) -> Option<(InFile<AstPtr<Either<ast::Expr, ast::Pat>>>, ClosureCapture<'db>)> {
        let InternedClosure { owner: infer_owner, expr: closure, .. } = match self.id {
            AnyClosureId::ClosureId(it) => it.loc(db),
            AnyClosureId::CoroutineClosureId(it) => it.loc(db),
        };
        let infer = InferenceResult::of(db, infer_owner);
        let data = &infer.closures_data[&closure];
        let (node, place) = data.kind_origin.as_ref()?;
        let owner = infer_owner.expression_store_owner(db);
        // The capture is of the place or of one it starts with: a capture is truncated, never
        // made more precise.
        let capture = data.min_captures.values().flatten().find(|it| {
            it.place.base == place.base && place.projections.starts_with(&it.place.projections)
        })?;
        let (_, source_map) = ExpressionStore::with_source_map(db, owner);
        let node = match node.unpack() {
            ExprOrPatId::ExprId(expr) => source_map.expr_syntax(expr).ok()?,
            ExprOrPatId::PatId(pat) => source_map.pat_syntax(pat).ok()?,
        };
        Some((node, ClosureCapture { owner, infer_owner, closure, capture }))
    }

    /// rustc's `place_to_string_for_capture` of [`Closure::kind_origin`]'s own place, which
    /// E0525 labels. Where the capture holding it was truncated (behind a reference, by `move`,
    /// at a packed or union field), the capture's place is shorter than this one. As rustc
    /// prints it: every deref a leading `*`, every field `.name` or `.index`.
    pub fn kind_origin_place(&self, db: &'db Db) -> Option<String> {
        let (_, capture) = self.kind_origin(db)?;
        let InternedClosure { owner: infer_owner, expr: closure, .. } = match self.id {
            AnyClosureId::ClosureId(it) => it.loc(db),
            AnyClosureId::CoroutineClosureId(it) => it.loc(db),
        };
        let infer = InferenceResult::of(db, infer_owner);
        let (_, place) = infer.closures_data[&closure].kind_origin.as_ref()?;
        // alloc: diagnostic edge, the label's text
        let mut result = capture.local().name(db).as_str().to_owned();
        for (i, proj) in place.projections.iter().enumerate() {
            match proj.kind {
                hir_ty::closure_analysis::ProjectionKind::Deref => result.insert(0, '*'),
                hir_ty::closure_analysis::ProjectionKind::Field { field_idx, variant_idx } => {
                    match place.ty_before_projection(i).kind() {
                        TyKind::Tuple(_) => format_to!(result, ".{field_idx}"),
                        TyKind::Adt(adt_def, _) => {
                            let variant = match adt_def.def_id() {
                                AdtId::StructId(id) => VariantId::from(id),
                                AdtId::UnionId(id) => id.into(),
                                AdtId::EnumId(id) => {
                                    id.enum_variants(db).variants[variant_idx as usize].0.into()
                                }
                            };
                            let field = &variant.fields(db).fields()
                                [LocalFieldId::from_raw(la_arena::RawIdx::from_u32(field_idx))];
                            format_to!(result, ".{}", field.name.as_str());
                        }
                        _ => {
                            never!("mismatching projection type");
                            return None;
                        }
                    }
                }
                _ => {
                    never!("unexpected projection kind");
                    return None;
                }
            }
        }
        Some(result)
    }

    pub fn fn_trait(&self, _db: &Db) -> FnTrait {
        match self.id {
            AnyClosureId::ClosureId(_) => match self.subst.as_closure().kind() {
                rustc_type_ir::ClosureKind::Fn => FnTrait::Fn,
                rustc_type_ir::ClosureKind::FnMut => FnTrait::FnMut,
                rustc_type_ir::ClosureKind::FnOnce => FnTrait::FnOnce,
            },
            AnyClosureId::CoroutineClosureId(_) => match self.subst.as_coroutine_closure().kind() {
                rustc_type_ir::ClosureKind::Fn => FnTrait::AsyncFn,
                rustc_type_ir::ClosureKind::FnMut => FnTrait::AsyncFnMut,
                rustc_type_ir::ClosureKind::FnOnce => FnTrait::AsyncFnOnce,
            },
        }
    }
}

/// A coroutine expression, including async, generator, and async-generator coroutines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Coroutine<'db> {
    id: InternedCoroutineId<'db>,
}

impl<'db> Coroutine<'db> {
    /// Returns the values captured by this coroutine.
    pub fn captured_items(&self, db: &'db Db) -> Vec<ClosureCapture<'db>> {
        captured_items(db, self.id.loc(db))
    }
}

fn captured_items<'db>(
    db: &'db Db,
    closure: InternedClosure<'db>,
) -> Vec<ClosureCapture<'db>> {
    let InternedClosure { owner: infer_owner, expr: closure, .. } = closure;
    let infer = InferenceResult::of(db, infer_owner);
    let owner = infer_owner.expression_store_owner(db);
    infer.closures_data[&closure]
        .min_captures
        .values()
        .flatten()
        .map(|capture| ClosureCapture { owner, infer_owner, closure, capture })
        .collect() // alloc: editor edge, the owned capture list callers take
}

pub use hir_ty::traits::FnTrait;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosureCapture<'db> {
    owner: ExpressionStoreOwnerId,
    infer_owner: InferBodyId<'db>,
    closure: ExprId,
    capture: &'db hir_ty::closure_analysis::CapturedPlace,
}

impl<'db> ClosureCapture<'db> {
    pub fn local(&self) -> Local<'db> {
        Local {
            parent: self.owner,
            parent_infer: self.infer_owner,
            binding_id: self.capture.captured_local(),
        }
    }

    /// Returns whether this place has any field (aka. non-deref) projections.
    pub fn has_field_projections(&self) -> bool {
        self.capture.place.projections.iter().any(|proj| {
            matches!(proj.kind, hir_ty::closure_analysis::ProjectionKind::Field { .. })
        })
    }

    pub fn usages(&self) -> CaptureUsages<'db> {
        CaptureUsages { parent: self.owner, sources: &self.capture.info.sources }
    }

    pub fn kind(&self) -> CaptureKind {
        match self.capture.info.capture_kind {
            hir_ty::closure_analysis::UpvarCapture::ByValue => CaptureKind::Move,
            hir_ty::closure_analysis::UpvarCapture::ByUse => CaptureKind::SharedRef, // Good enough?
            hir_ty::closure_analysis::UpvarCapture::ByRef(
                hir_ty::closure_analysis::BorrowKind::Immutable,
            ) => CaptureKind::SharedRef,
            hir_ty::closure_analysis::UpvarCapture::ByRef(
                hir_ty::closure_analysis::BorrowKind::UniqueImmutable,
            ) => CaptureKind::UniqueSharedRef,
            hir_ty::closure_analysis::UpvarCapture::ByRef(
                hir_ty::closure_analysis::BorrowKind::Mutable,
            ) => CaptureKind::MutableRef,
        }
    }

    /// Converts the place to a name that can be inserted into source code.
    pub fn place_to_name(&self, db: &Db, edition: Edition) -> String {
        // alloc: editor edge, a name for an assist to insert
        let mut result = self.local().name(db).display(db, edition).to_string();
        for (i, proj) in self.capture.place.projections.iter().enumerate() {
            match proj.kind {
                hir_ty::closure_analysis::ProjectionKind::Deref => {}
                hir_ty::closure_analysis::ProjectionKind::Field { field_idx, variant_idx } => {
                    let ty = self.capture.place.ty_before_projection(i);
                    match ty.kind() {
                        TyKind::Tuple(_) => format_to!(result, "_{field_idx}"),
                        TyKind::Adt(adt_def, _) => {
                            let variant = match adt_def.def_id() {
                                AdtId::StructId(id) => VariantId::from(id),
                                AdtId::UnionId(id) => id.into(),
                                AdtId::EnumId(id) => {
                                    id.enum_variants(db).variants[variant_idx as usize].0.into()
                                }
                            };
                            let field = &variant.fields(db).fields()
                                [LocalFieldId::from_raw(la_arena::RawIdx::from_u32(field_idx))];
                            format_to!(result, "_{}", field.name.display(db, edition));
                        }
                        _ => never!("mismatching projection type"),
                    }
                }
                _ => never!("unexpected projection kind"),
            }
        }
        result
    }

    pub fn display_place_source_code(&self, db: &Db, edition: Edition) -> String {
        // alloc: editor edge, source code for an assist to insert
        let mut result = self.local().name(db).display(db, edition).to_string();
        // We only need the derefs that have no field access after them, autoderef will do the rest.
        let mut last_derefs = 0;
        for (i, proj) in self.capture.place.projections.iter().enumerate() {
            match proj.kind {
                hir_ty::closure_analysis::ProjectionKind::Deref => last_derefs += 1,
                hir_ty::closure_analysis::ProjectionKind::Field { field_idx, variant_idx } => {
                    last_derefs = 0;

                    let ty = self.capture.place.ty_before_projection(i);
                    match ty.kind() {
                        TyKind::Tuple(_) => format_to!(result, ".{field_idx}"),
                        TyKind::Adt(adt_def, _) => {
                            let variant = match adt_def.def_id() {
                                AdtId::StructId(id) => VariantId::from(id),
                                AdtId::UnionId(id) => id.into(),
                                AdtId::EnumId(id) => {
                                    // Can't really do that for an enum, unfortunately, so try to do something alike.
                                    id.enum_variants(db).variants[variant_idx as usize].0.into()
                                }
                            };
                            let field = &variant.fields(db).fields()
                                [LocalFieldId::from_raw(la_arena::RawIdx::from_u32(field_idx))];
                            format_to!(result, ".{}", field.name.display(db, edition));
                        }
                        _ => never!("mismatching projection type"),
                    }
                }
                _ => never!("unexpected projection kind"),
            }
        }
        result.insert_str(0, &"*".repeat(last_derefs));
        result
    }

    pub fn ty(&self, db: &'db Db) -> Type<'db> {
        Type::new_body(db, self.owner, self.capture.place.ty())
    }

    /// The type that is stored in the closure, which is different from [`Self::ty()`], representing
    /// the place's type, when the capture is by ref.
    pub fn captured_ty(&self, db: &'db Db) -> Type<'db> {
        Type::new_body(db, self.owner, self.capture.captured_ty(db))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CaptureKind {
    SharedRef,
    UniqueSharedRef,
    MutableRef,
    Move,
}

#[derive(Debug, Clone)]
pub struct CaptureUsages<'db> {
    parent: ExpressionStoreOwnerId,
    sources: &'db [hir_ty::closure_analysis::CaptureSourceStack],
}

impl CaptureUsages<'_> {
    fn is_ref(store: &ExpressionStore, id: ExprOrPatId) -> bool {
        match id {
            ExprOrPatId::ExprId(expr) => matches!(store.expr(expr), Expr::Ref { .. }),
            // FIXME: Figure out if this is correct wrt. match ergonomics.
            ExprOrPatId::PatId(pat) => match store.pat(pat) {
                Pat::Bind { id: binding, .. } => matches!(
                    store[binding].mode,
                    BindingAnnotation::Ref | BindingAnnotation::RefMut
                ),
                _ => false,
            },
        }
    }

    pub fn sources(&self, db: &Db) -> Vec<CaptureUsageSource> {
        let (store, source_map) = ExpressionStore::with_source_map(db, self.parent);
        let mut result = Vec::with_capacity(self.sources.len());
        for source in self.sources {
            let source = source.final_source();
            let is_ref = Self::is_ref(store, source.unpack());
            match source.unpack() {
                ExprOrPatId::ExprId(expr) => {
                    if let Ok(expr) = source_map.expr_syntax(expr) {
                        result.push(CaptureUsageSource { is_ref, source: expr })
                    }
                }
                ExprOrPatId::PatId(pat) => {
                    if let Ok(pat) = source_map.pat_syntax(pat) {
                        result.push(CaptureUsageSource { is_ref, source: pat });
                    }
                }
            }
        }
        result
    }
}

#[derive(Debug)]
pub struct CaptureUsageSource {
    is_ref: bool,
    source: InFile<AstPtr<Either<ast::Expr, ast::Pat>>>,
}

impl CaptureUsageSource {
    pub fn source(&self) -> AstPtr<Either<ast::Expr, ast::Pat>> {
        self.source.value
    }

    pub fn file_id(&self) -> HirFileId {
        self.source.file_id
    }

    pub fn is_ref(&self) -> bool {
        self.is_ref
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
enum TypeOwnerId {
    GenericDefId(GenericDefId),
    BuiltinDeriveImplId(BuiltinDeriveImplId),
    // FIXME: What do when we unify two different crates? Currently we just randomly keep one.
    NoParams(base_db::Crate),
}

impl_from!(
    GenericDefId,
    BuiltinDeriveImplId
    for TypeOwnerId
);

impl TypeOwnerId {
    /// We associated anon consts with their parent, because they can never have generics of their own.
    /// It can have *less* than the parent, but providing more generic args is not a problem.
    fn from_anon_const<'db>(id: AnonConstId<'db>, db: &'db Db) -> TypeOwnerId {
        TypeOwnerId::GenericDefId(id.loc(db).owner.generic_def(db))
    }

    fn unify(self, other: Self) -> Option<Self> {
        match (self, other) {
            (TypeOwnerId::NoParams(_), owner) => Some(owner),
            (owner, TypeOwnerId::NoParams(_)) => Some(owner),
            (_, _) => {
                if self == other {
                    Some(self)
                } else {
                    None
                }
            }
        }
    }

    #[track_caller]
    fn must_unify(self, other: Self) -> Self {
        self.unify(other).expect("failed to unify type owners")
    }

    fn can_rebase_into(
        self,
        db: &Db,
        rebase_into: Self,
        self_ty: EarlyBinder<'_, Ty<'_>>,
    ) -> bool {
        if self == rebase_into || !self_ty.skip_binder().has_param() {
            return true;
        }
        let self_def = match self {
            TypeOwnerId::GenericDefId(def) => def,
            TypeOwnerId::BuiltinDeriveImplId(_) => return false,
            TypeOwnerId::NoParams(_) => return true,
        };
        let self_def = match self_def {
            GenericDefId::ImplId(def) => ItemContainerId::ImplId(def),
            GenericDefId::TraitId(def) => ItemContainerId::TraitId(def),
            GenericDefId::AdtId(_)
            | GenericDefId::ConstId(_)
            | GenericDefId::FunctionId(_)
            | GenericDefId::StaticId(_)
            | GenericDefId::TypeAliasId(_) => return false,
        };
        let rebase_into_def = match rebase_into {
            TypeOwnerId::GenericDefId(def) => def,
            TypeOwnerId::BuiltinDeriveImplId(_) | TypeOwnerId::NoParams(_) => return false,
        };
        let rebase_into_parent = match rebase_into_def {
            GenericDefId::ConstId(def) => def.loc(db).container,
            GenericDefId::FunctionId(def) => def.loc(db).container,
            GenericDefId::TypeAliasId(def) => def.loc(db).container,
            GenericDefId::AdtId(_)
            | GenericDefId::ImplId(_)
            | GenericDefId::StaticId(_)
            | GenericDefId::TraitId(_) => return false,
        };
        self_def == rebase_into_parent
    }
}

/// Note: A [`Type`] remembers its origin. Trying to do anything (except comparing)
/// with types of different origins will cause errors or panics. Instead, use the `instantiate` methods.
#[derive(Clone, Copy, Debug)]
pub struct Type<'db> {
    owner: TypeOwnerId,
    ty: EarlyBinder<'db, Ty<'db>>,
}

impl<'db> std::hash::Hash for Type<'db> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Do not hash the owner as different owners can compare the same.
        // self.owner.hash(state);
        self.ty.hash(state);
    }
}

impl<'db> PartialEq for Type<'db> {
    fn eq(&self, other: &Self) -> bool {
        if self.ty != other.ty {
            return false;
        }
        hir_ty::with_attached_db(|db| {
            self.owner.can_rebase_into(db, other.owner, self.ty)
                || other.owner.can_rebase_into(db, self.owner, other.ty)
        })
    }
}

impl<'db> Eq for Type<'db> {}

impl<'db> Type<'db> {
    fn new(db: &'db Db, owner: GenericDefId, ty: Ty<'db>) -> Self {
        Self::new_in(DbInterner::new_no_crate(db), owner, ty)
    }

    /// [`Type::new`] with the caller's interner, for loops over many types: making an interner
    /// costs more than binding a type with no rigid aliases (one flags load).
    fn new_in(interner: DbInterner<'db>, owner: GenericDefId, ty: Ty<'db>) -> Self {
        Type { owner: TypeOwnerId::GenericDefId(owner), ty: EarlyBinder::bind(interner, ty) }
    }

    fn new_body(db: &'db Db, owner: ExpressionStoreOwnerId, ty: Ty<'db>) -> Self {
        Self::new(db, owner.generic_def(db), ty)
    }

    fn no_params(db: &'db Db, krate: base_db::Crate, ty: Ty<'db>) -> Self {
        let ty = EarlyBinder::bind(DbInterner::new_no_crate(db), ty);
        Type { owner: TypeOwnerId::NoParams(krate), ty }
    }

    fn builtin_type_crate(db: &'db Db) -> base_db::Crate {
        // It doesn't really matter.
        all_crates(db)[0]
    }

    /// The type owned by no item's generics, when it names none of them, so that it can be
    /// asked of together with the types of any item: for blaze's lints.
    pub fn without_params(&self, db: &'db Db) -> Option<Type<'db>> {
        if self.ty.skip_binder().has_param() {
            return None;
        }
        Some(Type { owner: TypeOwnerId::NoParams(Self::builtin_type_crate(db)), ty: self.ty })
    }

    fn from_def(db: &'db Db, def: impl Into<TyDefId>) -> Self {
        let def = def.into();
        let ty = db.ty(def);
        let owner = match def {
            TyDefId::AdtId(it) => TypeOwnerId::GenericDefId(GenericDefId::AdtId(it)),
            TyDefId::TypeAliasId(it) => TypeOwnerId::GenericDefId(GenericDefId::TypeAliasId(it)),
            TyDefId::BuiltinType(_) => TypeOwnerId::NoParams(Self::builtin_type_crate(db)),
        };
        Type { owner, ty }
    }

    fn from_value_def(db: &'db Db, def: impl Into<ValueTyDefId>) -> Self {
        let def = def.into();
        let Some(ty) = db.value_ty(def) else {
            return Type::unknown(db);
        };
        let def = match def {
            ValueTyDefId::ConstId(it) => GenericDefId::ConstId(it),
            ValueTyDefId::FunctionId(it) => GenericDefId::FunctionId(it),
            ValueTyDefId::StructId(it) => GenericDefId::AdtId(AdtId::StructId(it)),
            ValueTyDefId::UnionId(it) => GenericDefId::AdtId(AdtId::UnionId(it)),
            ValueTyDefId::EnumVariantId(it) => {
                GenericDefId::AdtId(AdtId::EnumId(it.lookup(db).parent))
            }
            ValueTyDefId::StaticId(it) => {
                return Type::no_params(db, hir_def::HasModule::krate(&it, db), ty.skip_binder());
            }
        };
        Type::new(db, def, ty.instantiate_identity().skip_norm_wip())
    }

    /// Replace any generic parameters with error types.
    pub fn instantiate_with_errors(&self, db: &'db Db) -> Self {
        let interner = DbInterner::new_no_crate(db);
        let krate = self.krate(db);
        let args = match self.owner {
            TypeOwnerId::GenericDefId(def) => GenericArgs::error_for_item(interner, def.into()),
            TypeOwnerId::BuiltinDeriveImplId(def) => {
                GenericArgs::error_for_item(interner, def.into())
            }
            TypeOwnerId::NoParams(_) => GenericArgs::empty(),
        };
        Type::no_params(db, krate, self.ty.instantiate(interner, args).skip_norm_wip())
    }

    // FIXME: Find some way with const params, maybe even lifetimes?
    pub fn instantiate(
        &self,
        db: &'db Db,
        args: impl IntoIterator<Item: Borrow<Type<'db>>>,
    ) -> Type<'db> {
        let interner = DbInterner::new_no_crate(db);
        let (args, owner) = match self.owner {
            TypeOwnerId::GenericDefId(def) => generic_args_from_tys(interner, def.into(), args),
            TypeOwnerId::BuiltinDeriveImplId(def) => {
                generic_args_from_tys(interner, def.into(), args)
            }
            TypeOwnerId::NoParams(krate) => (GenericArgs::empty(), TypeOwnerId::NoParams(krate)),
        };
        let ty = self.ty.instantiate(interner, args).skip_norm_wip();
        Type { owner, ty: EarlyBinder::bind(interner, ty) }
    }

    /// The type with its type parameters filled from `args`, in order, and its lifetime and const
    /// parameters left as its own: `Foo<'a, i32, N>` of `Foo<'a, T, const N: usize>`. Unlike
    /// [`Type::instantiate`], which fills those with errors that every trait goal takes to hold,
    /// trait goals on the result are solved as in the type's own crate and environment.
    pub fn instantiate_type_params(
        &self,
        db: &'db Db,
        args: impl IntoIterator<Item: Borrow<Type<'db>>>,
    ) -> Type<'db> {
        let TypeOwnerId::GenericDefId(def) = self.owner else { return *self };
        let interner = DbInterner::new_no_crate(db);
        let mut args = args.into_iter();
        let args = GenericArgs::for_item(interner, def.into(), |index, id, _, _| match id {
            GenericParamId::TypeParamId(_) => match args.next() {
                Some(arg) => arg.borrow().ty.skip_binder().into(),
                None => hir_ty::next_solver::mk_param(interner, index, id),
            },
            _ => hir_ty::next_solver::mk_param(interner, index, id),
        });
        let ty = self.ty.instantiate(interner, args).skip_norm_wip();
        Type { owner: self.owner, ty: EarlyBinder::bind(interner, ty) }
    }

    /// Instantiates multiple types with infer vars, keeping the same infer vars for the same owners.
    fn instantiate_many_with_infer(
        tys: impl IntoIterator<Item: Borrow<Type<'db>>>,
        infcx: &InferCtxt<'db>,
    ) -> impl Iterator<Item = Ty<'db>> {
        let mut var_for_param = HashMap::default();
        tys.into_iter().map(move |ty| {
            let ty = ty.borrow();
            let owner = match ty.owner {
                TypeOwnerId::GenericDefId(def) => def.into(),
                TypeOwnerId::BuiltinDeriveImplId(def) => def.into(),
                TypeOwnerId::NoParams(_) => return ty.ty.skip_binder(),
            };
            let args = GenericArgs::for_item(infcx.interner, owner, |_, param, _, _| {
                *var_for_param
                    .entry(param)
                    .or_insert_with(|| infcx.var_for_def(param, hir_ty::Span::Dummy))
            });

            ty.ty.instantiate(infcx.interner, args).skip_norm_wip()
        })
    }

    /// Tries to put this type as-is in the context of `rebase_into`. This will return `Some(_)` if:
    ///
    ///  - The type does not reference generic parameters, or
    ///  - `rebase_into` is in the context of a child of our context (for example, a function in an impl).
    pub fn try_rebase_into(
        &self,
        db: &'db Db,
        rebase_into: &Type<'db>,
    ) -> Option<Self> {
        if self.owner.can_rebase_into(db, rebase_into.owner, self.ty) {
            Some(Type { owner: rebase_into.owner, ty: self.ty })
        } else {
            None
        }
    }

    /// If `self` can be rebased into `rebase_into`, returns that. Otherwise, instantiates `self` with errors
    /// and returns that.
    pub fn rebase_into_or_error(
        &self,
        db: &'db Db,
        rebase_into: &Type<'db>,
    ) -> Type<'db> {
        self.try_rebase_into(db, rebase_into).unwrap_or_else(|| self.instantiate_with_errors(db))
    }

    pub fn try_rebase_into_owner(
        &self,
        db: &'db Db,
        new_owner: GenericDef,
    ) -> Option<Self> {
        let new_owner = new_owner.id()?.into();
        if self.owner.can_rebase_into(db, new_owner, self.ty) {
            Some(Type { owner: new_owner, ty: self.ty })
        } else {
            None
        }
    }

    pub fn rebase_into_owner_or_error(
        &self,
        db: &'db Db,
        new_owner: GenericDef,
    ) -> Self {
        self.try_rebase_into_owner(db, new_owner).unwrap_or_else(|| self.instantiate_with_errors(db))
    }

    pub fn unknown(db: &'db Db) -> Self {
        let interner = DbInterner::new_no_crate(db);
        Type::no_params(db, Self::builtin_type_crate(db), Ty::new_error(interner, ErrorGuaranteed))
    }

    pub fn new_slice(db: &'db Db, ty: Self) -> Self {
        let interner = DbInterner::new_no_crate(db);
        Type { owner: ty.owner, ty: ty.ty.map_bound(|ty| Ty::new_slice(interner, ty)) }
    }

    pub fn new_tuple(
        db: &'db Db,
        tys: impl IntoIterator<Item: Borrow<Type<'db>>>,
    ) -> Self {
        let interner = DbInterner::new_no_crate(db);
        let mut owner = None::<TypeOwnerId>;
        let ty = Ty::new_tup_from_iter(
            interner,
            tys.into_iter().map(|ty| {
                let ty = ty.borrow();

                match &mut owner {
                    Some(owner) => *owner = owner.must_unify(ty.owner),
                    None => owner = Some(ty.owner),
                }

                ty.ty.skip_binder()
            }),
        );
        let owner =
            owner.unwrap_or_else(|| TypeOwnerId::NoParams(Self::builtin_type_crate(interner.db())));
        Type { owner, ty: EarlyBinder::bind(interner, ty) }
    }

    pub fn new_unit(db: &'db Db) -> Self {
        let interner = DbInterner::new_no_crate(db);
        Type::no_params(db, Self::builtin_type_crate(db), Ty::new_unit(interner))
    }

    pub fn is_unit(&self) -> bool {
        self.ty.skip_binder().is_unit()
    }

    pub fn is_bool(&self) -> bool {
        matches!(self.ty.skip_binder().kind(), TyKind::Bool)
    }

    pub fn is_str(&self) -> bool {
        matches!(self.ty.skip_binder().kind(), TyKind::Str)
    }

    pub fn is_never(&self) -> bool {
        matches!(self.ty.skip_binder().kind(), TyKind::Never)
    }

    pub fn is_mutable_reference(&self) -> bool {
        matches!(
            self.ty.skip_binder().kind(),
            TyKind::Ref(.., hir_ty::next_solver::Mutability::Mut)
        )
    }

    pub fn is_reference(&self) -> bool {
        matches!(self.ty.skip_binder().kind(), TyKind::Ref(..))
    }

    pub fn contains_reference(&self, db: &'db Db) -> bool {
        let interner = DbInterner::new_no_crate(db);
        return self
            .ty
            .instantiate_identity()
            .skip_norm_wip()
            .visit_with(&mut Visitor { interner })
            .is_break();

        fn is_phantom_data(db: &Db, adt_id: AdtId) -> bool {
            match adt_id {
                AdtId::StructId(s) => {
                    let flags = StructSignature::of(db, s).flags;
                    flags.contains(StructFlags::IS_PHANTOM_DATA)
                }
                AdtId::UnionId(_) | AdtId::EnumId(_) => false,
            }
        }

        struct Visitor<'db> {
            interner: DbInterner<'db>,
        }

        impl<'db> TypeVisitor<DbInterner<'db>> for Visitor<'db> {
            type Result = ControlFlow<()>;

            fn visit_ty(&mut self, ty: Ty<'db>) -> Self::Result {
                match ty.kind() {
                    // Reference itself
                    TyKind::Ref(..) => ControlFlow::Break(()),

                    // For non-phantom_data adts we check the generic arguments
                    TyKind::Adt(adt_def, args)
                        if !is_phantom_data(self.interner.db(), adt_def.def_id()) =>
                    {
                        args.visit_with(self)
                    }
                    // And for `PhantomData<T>`, we check `T`.
                    _ => ty.super_visit_with(self),
                }
            }
        }
    }

    pub fn as_reference(&self) -> Option<(Type<'db>, Mutability)> {
        let TyKind::Ref(_lt, ty, m) = self.ty.skip_binder().kind() else { return None };
        let m = Mutability::from_mutable(matches!(m, hir_ty::next_solver::Mutability::Mut));
        Some((self.derived(ty), m))
    }

    pub fn as_reference_inner(&self) -> Option<Type<'db>> {
        self.as_reference().map(|(inner, _)| inner)
    }

    pub fn add_reference(&self, db: &'db Db, mutability: Mutability) -> Self {
        let interner = DbInterner::new_no_crate(db);
        let ty_mutability = hir_ty::lower_mutability(mutability);
        self.derived_with(
            interner,
            Ty::new_ref(interner, Region::error(interner), self.ty.skip_binder(), ty_mutability),
        )
    }

    /// `&'erased self` (or `&'erased mut`) in this type's environment, as clippy's
    /// `Ty::new_imm_ref(tcx, re_erased, ty)`; [`Type::add_reference`] gives an error region.
    pub fn ref_to(&self, db: &'db Db, mutability: Mutability) -> Type<'db> {
        let interner = DbInterner::new_no_crate(db);
        let ty_mutability = hir_ty::lower_mutability(mutability);
        let region = Region::new_erased(interner);
        self.derived_with(interner, Ty::new_ref(interner, region, self.ty.skip_binder(), ty_mutability))
    }

    pub fn is_slice(&self) -> bool {
        matches!(self.ty.skip_binder().kind(), TyKind::Slice(..))
    }

    pub fn is_usize(&self) -> bool {
        matches!(self.ty.skip_binder().kind(), TyKind::Uint(rustc_type_ir::UintTy::Usize))
    }

    pub fn is_float(&self) -> bool {
        matches!(self.ty.skip_binder().kind(), TyKind::Float(_))
    }

    pub fn is_char(&self) -> bool {
        matches!(self.ty.skip_binder().kind(), TyKind::Char)
    }

    pub fn is_int_or_uint(&self) -> bool {
        matches!(self.ty.skip_binder().kind(), TyKind::Int(_) | TyKind::Uint(_))
    }

    pub fn is_scalar(&self) -> bool {
        matches!(
            self.ty.skip_binder().kind(),
            TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_) | TyKind::Float(_)
        )
    }

    pub fn is_tuple(&self) -> bool {
        matches!(self.ty.skip_binder().kind(), TyKind::Tuple(..))
    }

    pub fn as_slice(&self) -> Option<Type<'db>> {
        self.ty.skip_binder().as_slice().map(|ty| self.derived(ty))
    }

    pub fn strip_references(&self) -> Self {
        self.derived(self.ty.skip_binder().strip_references())
    }

    // FIXME: This is the same as `remove_ref()`, remove one of these methods.
    pub fn strip_reference(&self) -> Self {
        self.derived(self.ty.skip_binder().strip_reference())
    }

    pub fn is_unknown(&self) -> bool {
        self.ty.skip_binder().is_ty_error()
    }

    fn krate(&self, db: &'db Db) -> base_db::Crate {
        match self.owner {
            TypeOwnerId::GenericDefId(def) => hir_def::HasModule::krate(&def, db),
            TypeOwnerId::BuiltinDeriveImplId(def) => {
                hir_def::HasModule::krate(&def.loc(db).adt, db)
            }
            TypeOwnerId::NoParams(krate) => krate,
        }
    }

    fn param_env(&self, db: &'db Db) -> ParamEnvAndCrate<'db> {
        let krate = self.krate(db);
        match self.owner {
            TypeOwnerId::GenericDefId(def) => {
                ParamEnvAndCrate { param_env: db.trait_environment(def), krate }
            }
            TypeOwnerId::BuiltinDeriveImplId(def) => ParamEnvAndCrate {
                param_env: hir_ty::builtin_derive::param_env(DbInterner::new_with(db, krate), def),
                krate,
            },
            TypeOwnerId::NoParams(_) => ParamEnvAndCrate { param_env: ParamEnv::empty(), krate },
        }
    }

    /// Checks that particular type `ty` implements `std::future::IntoFuture` or
    /// `std::future::Future` and returns the `Output` associated type.
    /// This function is used in `.await` syntax completion.
    pub fn into_future_output(&self, db: &'db Db) -> Option<Type<'db>> {
        let env = self.param_env(db);
        let lang_items = hir_def::lang_item::lang_items(db, env.krate);
        let (trait_, output_assoc_type) = lang_items
            .IntoFuture
            .zip(lang_items.IntoFutureOutput)
            .or(lang_items.Future.zip(lang_items.FutureOutput))?;

        if !traits::implements_trait_unique(
            self.ty.instantiate_identity().skip_norm_wip(),
            db,
            env,
            trait_,
        ) {
            return None;
        }

        self.normalize_trait_assoc_type(db, &[], output_assoc_type.into())
    }

    /// This does **not** resolve `IntoFuture`, only `Future`.
    pub fn future_output(self, db: &'db Db) -> Option<Type<'db>> {
        let krate = self.krate(db);
        let lang_items = hir_def::lang_item::lang_items(db, krate);
        let future_output = lang_items.FutureOutput?;
        self.normalize_trait_assoc_type(db, &[], future_output.into())
    }

    /// This does **not** resolve `IntoIterator`, only `Iterator`.
    pub fn iterator_item(self, db: &'db Db) -> Option<Type<'db>> {
        let krate = self.krate(db);
        let lang_items = hir_def::lang_item::lang_items(db, krate);
        let iterator_item = lang_items.IteratorItem?;
        self.normalize_trait_assoc_type(db, &[], iterator_item.into())
    }

    pub fn impls_iterator(self, db: &'db Db) -> bool {
        let env = self.param_env(db);
        let lang_items = hir_def::lang_item::lang_items(db, env.krate);
        let Some(iterator_trait) = lang_items.Iterator else {
            return false;
        };
        traits::implements_trait_unique(
            self.ty.instantiate_identity().skip_norm_wip(),
            db,
            env,
            iterator_trait,
        )
    }

    /// Resolves the projection `<Self as IntoIterator>::IntoIter` and returns the resulting type
    pub fn into_iterator_iter(self, db: &'db Db) -> Option<Type<'db>> {
        let env = self.param_env(db);
        let lang_items = hir_def::lang_item::lang_items(db, env.krate);
        let trait_ = lang_items.IntoIterator?;

        if !traits::implements_trait_unique(
            self.ty.instantiate_identity().skip_norm_wip(),
            db,
            env,
            trait_,
        ) {
            return None;
        }

        let into_iter_assoc_type = lang_items.IntoIterIntoIterType?;
        self.normalize_trait_assoc_type(db, &[], into_iter_assoc_type.into())
    }

    /// Checks that particular type `ty` implements `std::ops::FnOnce`.
    ///
    /// This function can be used to check if a particular type is callable, since FnOnce is a
    /// supertrait of Fn and FnMut, so all callable types implements at least FnOnce.
    pub fn impls_fnonce(&self, db: &'db Db) -> bool {
        let env = self.param_env(db);
        let lang_items = hir_def::lang_item::lang_items(db, env.krate);
        let fnonce_trait = match lang_items.FnOnce {
            Some(it) => it,
            None => return false,
        };

        traits::implements_trait_unique(
            self.ty.instantiate_identity().skip_norm_wip(),
            db,
            env,
            fnonce_trait,
        )
    }

    // FIXME: Find better API that also handles const generics
    /// Whether the type implements `trait_` with `args` after `Self`; a parameter `args` leaves
    /// out is an error, and a goal with a type or const error never holds here, though the
    /// solver, as rustc's, proves one (`consider_error_guaranteed_candidate`) to stop errors
    /// cascading. Every region of an inference result is an error region; those are erased
    /// first, as clippy's types from typeck results are, since the solver would prove any goal
    /// holding one and trait goals are solved modulo regions (`&str: Display` holds).
    pub fn impls_trait(&self, db: &'db Db, trait_: Trait, args: &[Type<'db>]) -> bool {
        let env = self.param_env(db);
        let interner = DbInterner::new_no_crate(db);
        let (args, _owner) =
            generic_args_from_tys(interner, trait_.id.into(), iter::once(self).chain(args));
        let args = rustc_type_ir::fold_regions(interner, args, |region, _| match region.kind() {
            rustc_type_ir::RegionKind::ReError(_) => Region::new_erased(interner),
            _ => region,
        });
        if args.references_error() {
            return false;
        }
        traits::implements_trait_unique_with_args(db, env, trait_.id, args)
    }

    /// Whether the type implements `trait_` for some arguments after `Self`, each a fresh
    /// inference variable: clippy's `implements_trait(cx, ty, trait_, &[])`, which leaves them to
    /// inference. [`Type::impls_trait`] with no arguments makes them errors, which never hold.
    pub fn impls_trait_fresh(&self, db: &'db Db, trait_: Trait) -> bool {
        traits::implements_trait_unique(
            self.ty.instantiate_identity().skip_norm_wip(),
            db,
            self.param_env(db),
            trait_.id,
        )
    }

    /// Unlike [`Type::impls_trait()`], which checks whether the type always implements the trait,
    /// this check whether there are any generic args substitution for `args`` that will cause the
    /// trait to be implemented.
    ///
    /// For example, suppose we're there's `struct Foo<T>` and we're checking `Foo<T>: Trait`.
    /// `impls_trait()` will return true only if there is `impl<T> Trait for Foo<T>`, while this
    /// method will also return true if there is only `impl Trait for Foo<i32>`.
    ///
    /// Note that you can of course instantiate `Foo<T>` with `<i32>` and then the checks will
    /// be the same, but this check for *any* substitution.
    ///
    /// Unlike almost anything that takes more than one type, you *can* pass types from different origins
    /// to this function.
    pub fn has_any_impl(
        &self,
        db: &'db Db,
        trait_: Trait,
        args: &[Type<'db>],
    ) -> bool {
        let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: self.krate(db) };
        traits::implements_trait_unique_with_infcx(db, env, trait_.id, |infcx| {
            let mut args = Self::instantiate_many_with_infer(iter::once(self).chain(args), infcx);
            GenericArgs::for_item(infcx.interner, trait_.id.into(), |_, param, _, _| {
                if let GenericParamId::TypeParamId(_) = param
                    && let Some(arg) = args.next()
                {
                    arg.into()
                } else {
                    infcx.var_for_def(param, hir_ty::Span::Dummy)
                }
            })
        })
    }

    /// Whether the two types unify with each one's generic parameters as inference variables,
    /// whatever owns them: `[u8]` and an impl's `[T]` do, `[u8]` and `[MaybeUninit<T>]` do not.
    pub fn unifies_with_any_params(&self, db: &'db Db, other: &Type<'db>) -> bool {
        traits::could_unify_with_infcx(db, self.krate(db), |infcx| {
            let a = Self::instantiate_many_with_infer([self], infcx).next().unwrap();
            let b = Self::instantiate_many_with_infer([other], infcx).next().unwrap();
            (a, b)
        })
    }

    /// Whether the trait impl may apply to the type, the type's generic parameters and the
    /// impl's as inference variables and an ambiguous answer counting: rustdoc's test of the
    /// blanket impls a type gets. Solved in `krate`, rustdoc's local crate, whose dependencies
    /// may reach an impl the type's own crate cannot see.
    pub fn may_have_impl(&self, db: &'db Db, impl_: Impl, krate: Crate) -> bool {
        let Ok(id) = ImplId::try_from(impl_) else { return false };
        traits::impl_may_apply(db, krate.id, id, |infcx| {
            Self::instantiate_many_with_infer([self], infcx).next().unwrap()
        })
    }

    pub fn normalize_trait_assoc_type(
        &self,
        db: &'db Db,
        args: &[Type<'db>],
        alias: TypeAlias,
    ) -> Option<Type<'db>> {
        let env = self.param_env(db);
        let interner = DbInterner::new_with(db, env.krate);
        let (args, owner) =
            generic_args_from_tys(interner, alias.id.into(), iter::once(self).chain(args));
        // FIXME: We don't handle GATs yet.
        let projection = Ty::new_alias(
            interner,
            rustc_type_ir::IsRigid::No,
            AliasTy::new_from_args(
                interner,
                AliasTyKind::Projection { def_id: alias.id.into() },
                args,
            ),
        );

        let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
        let ty = structurally_normalize_ty(&infcx, projection, env.param_env);
        if ty.is_ty_error() {
            None
        } else {
            Some(Type { owner, ty: EarlyBinder::bind(interner, ty) })
        }
    }

    pub fn is_copy(&self, db: &'db Db) -> bool {
        let env = self.param_env(db);
        let lang_items = hir_def::lang_item::lang_items(db, env.krate);
        let Some(copy_trait) = lang_items.Copy else {
            return false;
        };
        self.impls_trait(db, copy_trait.into(), &[])
    }

    /// rustc's `Ty::is_sized` in the type's environment: an error type is sized, as
    /// `is_trivially_sized` has it, and any other type is when `Sized` holds of it.
    pub fn is_sized(&self, db: &'db Db) -> bool {
        if self.is_unknown() {
            return true;
        }
        let env = self.param_env(db);
        let Some(sized_trait) = hir_def::lang_item::lang_items(db, env.krate).Sized else {
            return false;
        };
        traits::implements_trait_unique(
            self.ty.instantiate_identity().skip_norm_wip(),
            db,
            env,
            sized_trait,
        )
    }

    /// rustc's `type_allowed_to_implement_copy(..).is_ok()` with `Safety::Safe`, as clippy asks
    /// it: a primitive core implements `Copy` for, or an ADT with no unsafe field and no `Drop`
    /// impl whose every field, with this type's arguments, is `Copy` in its environment. A field
    /// whose type is an error is skipped, as rustc skips it. Modulo regions, as
    /// [`Type::is_copy`]: rustc also resolves each field's region constraints.
    pub fn may_implement_copy(&self, db: &'db Db) -> bool {
        let (adt_def, args) = match self.ty.skip_binder().kind() {
            TyKind::Uint(_)
            | TyKind::Int(_)
            | TyKind::Bool
            | TyKind::Float(_)
            | TyKind::Char
            | TyKind::RawPtr(..)
            | TyKind::Never
            | TyKind::Ref(_, _, hir_ty::next_solver::Mutability::Not)
            | TyKind::Array(..) => return true,
            TyKind::Adt(adt_def, args) => (adt_def, args),
            _ => return false,
        };
        let adt = adt_def.def_id();
        if hir_ty::drop::destructor(db, adt).is_some() {
            return false;
        }
        let interner = DbInterner::new_no_crate(db);
        let allowed = |variant: hir_def::VariantId| {
            if variant.fields(db).fields().iter().any(|(_, field)| field.is_unsafe) {
                return false;
            }
            db.field_types(variant).iter().all(|(_, field)| {
                let ty = field.ty().instantiate(interner, args).skip_norm_wip();
                let ty = self.derived_with(interner, ty);
                ty.contains_unknown() || ty.is_copy(db)
            })
        };
        match adt {
            AdtId::StructId(id) => allowed(id.into()),
            AdtId::UnionId(id) => allowed(id.into()),
            AdtId::EnumId(id) => {
                id.enum_variants(db).variants.values().all(|&(variant, _)| allowed(variant.into()))
            }
        }
    }

    pub fn as_callable(&self, db: &'db Db) -> Option<Callable<'db>> {
        let interner = DbInterner::new_no_crate(db);
        let callee = match self.ty.skip_binder().kind() {
            TyKind::Closure(id, subst) => Callee::Closure(id.0, subst),
            TyKind::CoroutineClosure(id, subst) => Callee::CoroutineClosure(id.0, subst),
            TyKind::FnPtr(..) => Callee::FnPtr,
            TyKind::FnDef(id, _) => Callee::Def(id.0),
            // This will happen when it implements fn or fn mut, since we add an autoborrow adjustment
            TyKind::Ref(_, inner_ty, _) => return self.derived(inner_ty).as_callable(db),
            _ => {
                let env = self.param_env(db);
                let (fn_trait, sig) =
                    hir_ty::callable_sig_from_fn_trait(self.ty.skip_binder(), env, db)?;
                let erased = interner.instantiate_bound_regions_with_erased(sig);
                return Some(Callable {
                    ty: *self,
                    sig,
                    erased: rustc_type_ir::set_aliases_to_non_rigid(interner, erased)
                        .skip_normalization(),
                    callee: Callee::FnImpl(fn_trait),
                    is_bound_method: false,
                });
            }
        };

        let sig = self.ty.skip_binder().callable_sig(interner)?;
        let erased = interner.instantiate_bound_regions_with_erased(sig);
        Some(Callable {
            ty: *self,
            sig,
            erased: rustc_type_ir::set_aliases_to_non_rigid(interner, erased).skip_normalization(),
            callee,
            is_bound_method: false,
        })
    }

    pub fn is_closure(&self) -> bool {
        matches!(self.ty.skip_binder().kind(), TyKind::Closure { .. })
    }

    pub fn as_closure(&self) -> Option<Closure<'db>> {
        match self.ty.skip_binder().kind() {
            TyKind::Closure(id, subst) => {
                Some(Closure { id: AnyClosureId::ClosureId(id.0), subst, owner: self.owner })
            }
            TyKind::CoroutineClosure(id, subst) => Some(Closure {
                id: AnyClosureId::CoroutineClosureId(id.0),
                subst,
                owner: self.owner,
            }),
            _ => None,
        }
    }

    /// Returns this type as a coroutine.
    pub fn as_coroutine(&self) -> Option<Coroutine<'db>> {
        match self.ty.skip_binder().kind() {
            TyKind::Coroutine(id, _) => Some(Coroutine { id: id.0 }),
            _ => None,
        }
    }

    pub fn is_fn(&self) -> bool {
        matches!(self.ty.skip_binder().kind(), TyKind::FnDef(..) | TyKind::FnPtr { .. })
    }

    pub fn is_array(&self) -> bool {
        matches!(self.ty.skip_binder().kind(), TyKind::Array(..))
    }

    pub fn is_packed(&self, _db: &'db Db) -> bool {
        match self.ty.skip_binder().kind() {
            TyKind::Adt(adt_def, ..) => adt_def.is_packed(),
            _ => false,
        }
    }

    pub fn is_raw_ptr(&self) -> bool {
        matches!(self.ty.skip_binder().kind(), TyKind::RawPtr(..))
    }

    pub fn is_mutable_raw_ptr(&self) -> bool {
        // Used outside of rust-analyzer (e.g. by `ra_ap_hir` consumers).
        matches!(
            self.ty.skip_binder().kind(),
            TyKind::RawPtr(.., hir_ty::next_solver::Mutability::Mut)
        )
    }

    pub fn as_raw_ptr(&self) -> Option<(Type<'db>, Mutability)> {
        // Used outside of rust-analyzer (e.g. by `ra_ap_hir` consumers).
        let TyKind::RawPtr(ty, m) = self.ty.skip_binder().kind() else { return None };
        let m = Mutability::from_mutable(matches!(m, hir_ty::next_solver::Mutability::Mut));
        Some((self.derived(ty), m))
    }

    pub fn remove_raw_ptr(&self) -> Option<Type<'db>> {
        if let TyKind::RawPtr(ty, _) = self.ty.skip_binder().kind() {
            Some(self.derived(ty))
        } else {
            None
        }
    }

    pub fn contains_unknown(&self) -> bool {
        self.ty.skip_binder().references_non_lt_error()
    }

    pub fn fields(&self, db: &'db Db) -> Vec<(Field, Self)> {
        let interner = DbInterner::new_no_crate(db);
        let (variant_id, substs) = match self.ty.skip_binder().kind() {
            TyKind::Adt(adt_def, substs) => {
                let id = match adt_def.def_id() {
                    AdtId::StructId(id) => id.into(),
                    AdtId::UnionId(id) => id.into(),
                    AdtId::EnumId(_) => return Vec::new(), // alloc: empty
                };
                (id, substs)
            }
            _ => return Vec::new(), // alloc: empty
        };

        db.field_types(variant_id)
            .iter()
            .map(|(local_id, field)| {
                let def = Field { parent: variant_id.into(), id: local_id };
                let ty = field.ty().instantiate(interner, substs).skip_norm_wip();
                (def, self.derived_with(interner, ty))
            })
            .collect() // alloc: editor edge, the owned field list callers take
    }

    /// [`Type::fields`] of `variant` of this enum type, its fields typed with this type's
    /// arguments. Empty when this is not the enum `variant` belongs to.
    pub fn variant_fields(
        &self,
        db: &'db Db,
        variant: EnumVariant,
    ) -> Vec<(Field, Self)> {
        let interner = DbInterner::new_no_crate(db);
        let TyKind::Adt(adt_def, substs) = self.ty.skip_binder().kind() else {
            return Vec::new(); // alloc: empty
        };
        if adt_def.def_id() != AdtId::EnumId(variant.id.lookup(db).parent) {
            return Vec::new(); // alloc: empty
        }
        let variant_id = hir_def::VariantId::EnumVariantId(variant.id);
        db.field_types(variant_id)
            .iter()
            .map(|(local_id, field)| {
                let def = Field { parent: variant_id.into(), id: local_id };
                let ty = field.ty().instantiate(interner, substs).skip_norm_wip();
                (def, self.derived_with(interner, ty))
            })
            .collect() // alloc: editor edge, the owned field list callers take
    }

    pub fn tuple_fields(&self, _db: &'db Db) -> Vec<Self> {
        if let TyKind::Tuple(substs) = self.ty.skip_binder().kind() {
            substs.iter().map(|ty| self.derived(ty)).collect() // alloc: editor edge
        } else {
            Vec::new() // alloc: empty
        }
    }

    pub fn as_array(&self, db: &'db Db) -> Option<(Self, usize)> {
        if let TyKind::Array(ty, len) = self.ty.skip_binder().kind() {
            try_const_usize(db, len).map(|it| (self.derived(ty), it as usize))
        } else {
            None
        }
    }

    /// The element type of `[T; N]`, whatever `N` is (a parameter, a constant not evaluated).
    pub fn as_array_element(&self, _db: &'db Db) -> Option<Self> {
        match self.ty.skip_binder().kind() {
            TyKind::Array(ty, _) => Some(self.derived(ty)),
            _ => None,
        }
    }

    /// The `extern { type T; }` this type is.
    pub fn as_foreign(&self, _db: &'db Db) -> Option<TypeAlias> {
        match self.ty.skip_binder().kind() {
            TyKind::Foreign(def) => Some(TypeAlias { id: def.0 }),
            _ => None,
        }
    }

    // FIXME: We should probably remove this.
    pub fn fingerprint_for_trait_impl(
        &self,
        db: &'db Db,
    ) -> Option<SimplifiedType<'db>> {
        fast_reject::simplify_type(
            DbInterner::new_no_crate(db),
            self.ty.skip_binder(),
            fast_reject::TreatParams::AsRigid,
        )
    }

    /// Returns types that this type dereferences to (including this type itself). The returned
    /// iterator won't yield the same type more than once even if the deref chain contains a cycle.
    pub fn autoderef(
        &self,
        db: &'db Db,
    ) -> impl Iterator<Item = Type<'db>> + use<'_, 'db> {
        let interner = DbInterner::new_no_crate(db);
        self.autoderef_(db).map(move |ty| self.derived_with(interner, ty))
    }

    fn autoderef_(&self, db: &'db Db) -> impl Iterator<Item = Ty<'db>> {
        let interner = DbInterner::new_no_crate(db);
        let env = self.param_env(db);
        // There should be no inference vars in types passed here
        let canonical = hir_ty::replace_errors_with_variables(interner, &self.ty.skip_binder());
        autoderef(db, env, canonical)
    }

    // This would be nicer if it just returned an iterator, but that runs into
    // lifetime problems, because we need to borrow temp `CrateImplDefs`.
    pub fn iterate_assoc_items<T>(
        &self,
        db: &'db Db,
        mut callback: impl FnMut(AssocItem) -> Option<T>,
    ) -> Option<T> {
        let mut slot = None;
        self.iterate_assoc_item_ids(db, &mut |assoc_item_id| {
            slot = callback(assoc_item_id.into());
            slot.is_some()
        });
        slot
    }

    fn iterate_assoc_item_ids(
        &self,
        db: &'db Db,
        callback: &mut impl FnMut(AssocItemId) -> bool,
    ) {
        let mut handle_impls = |impls: &[ImplId]| {
            for &impl_def in impls {
                for &(_, item) in impl_def.impl_items(db).items.iter() {
                    if callback(item) {
                        return;
                    }
                }
            }
        };
        let krate = self.krate(db);

        let interner = DbInterner::new_no_crate(db);
        let Some(simplified_type) = fast_reject::simplify_type(
            interner,
            self.ty.skip_binder(),
            fast_reject::TreatParams::AsRigid,
        ) else {
            return;
        };

        method_resolution::with_incoherent_inherent_impls(
            db,
            krate,
            &simplified_type,
            &mut handle_impls,
        );

        if let Some(module) = method_resolution::simplified_type_module(db, &simplified_type) {
            InherentImpls::for_each_crate_and_block(
                db,
                module.krate(db),
                module.block(db),
                &mut |impls| {
                    handle_impls(impls.for_self_ty(&simplified_type));
                },
            );
        }
    }

    /// Iterates its type arguments
    ///
    /// It iterates the actual type arguments when concrete types are used
    /// and otherwise the generic names.
    /// It does not include `const` arguments.
    ///
    /// For code, such as:
    /// ```text
    /// struct Foo<T, U>
    ///
    /// impl<U> Foo<String, U>
    /// ```
    ///
    /// It iterates:
    /// ```text
    /// - "String"
    /// - "U"
    /// ```
    pub fn type_arguments(&self) -> impl Iterator<Item = Type<'db>> + '_ {
        match self.ty.skip_binder().strip_references().kind() {
            TyKind::Adt(_, substs) => Either::Left(substs.types().map(move |ty| self.derived(ty))),
            TyKind::Tuple(substs) => {
                Either::Right(Either::Left(substs.iter().map(move |ty| self.derived(ty))))
            }
            _ => Either::Right(Either::Right(iter::empty())),
        }
    }

    /// Iterates its type and const arguments
    ///
    /// It iterates the actual type and const arguments when concrete types
    /// are used and otherwise the generic names.
    ///
    /// For code, such as:
    /// ```text
    /// struct Foo<T, const U: usize, const X: usize>
    ///
    /// impl<U> Foo<String, U, 12>
    /// ```
    ///
    /// It iterates:
    /// ```text
    /// - "String"
    /// - "U"
    /// - "12"
    /// ```
    pub fn type_and_const_arguments<'a>(
        &'a self,
        db: &'a Db,
        display_target: DisplayTarget,
    ) -> impl Iterator<Item = SmolStr> + 'a {
        self.ty
            .skip_binder()
            .strip_references()
            .as_adt()
            .into_iter()
            .flat_map(|(_, substs)| substs.iter())
            .filter_map(move |arg| match arg.kind() {
                rustc_type_ir::GenericArgKind::Type(ty) => {
                    Some(format_smolstr!("{}", ty.display(db, display_target)))
                }
                rustc_type_ir::GenericArgKind::Const(const_) => {
                    Some(format_smolstr!("{}", const_.display(db, display_target)))
                }
                rustc_type_ir::GenericArgKind::Lifetime(_) => None,
            })
    }

    /// Combines lifetime indicators, type and constant parameters into a single `Iterator`
    pub fn generic_parameters<'a>(
        &'a self,
        db: &'a Db,
        display_target: DisplayTarget,
    ) -> impl Iterator<Item = SmolStr> + 'a {
        // iterate the lifetime
        self.as_adt()
            .and_then(|a| {
                // Lifetimes do not need edition-specific handling as they cannot be escaped.
                a.lifetime(db).map(|lt| lt.name.display_no_db(Edition::Edition2015).to_smolstr())
            })
            .into_iter()
            // add the type and const parameters
            .chain(self.type_and_const_arguments(db, display_target))
    }

    pub fn iterate_method_candidates_with_traits<T>(
        &self,
        db: &'db Db,
        scope: &SemanticsScope<'db>,
        traits_in_scope: &HashSet<TraitId>,
        name: Option<&Name>,
        mut callback: impl FnMut(Function) -> Option<T>,
    ) -> Option<T> {
        let _p = tracing::info_span!("iterate_method_candidates_with_traits").entered();
        let mut slot = None;
        self.iterate_method_candidates_split_inherent(db, scope, traits_in_scope, name, |f| {
            match callback(f) {
                it @ Some(_) => {
                    slot = it;
                    ControlFlow::Break(())
                }
                None => ControlFlow::Continue(()),
            }
        });
        slot
    }

    pub fn iterate_method_candidates<T>(
        &self,
        db: &'db Db,
        scope: &SemanticsScope<'db>,
        name: Option<&Name>,
        callback: impl FnMut(Function) -> Option<T>,
    ) -> Option<T> {
        self.iterate_method_candidates_with_traits(
            db,
            scope,
            &scope.visible_traits().0,
            name,
            callback,
        )
    }

    fn with_method_resolution<R>(
        &self,
        db: &'db Db,
        resolver: &Resolver<'db>,
        traits_in_scope: &HashSet<TraitId>,
        f: impl FnOnce(&MethodResolutionContext<'_, 'db>) -> R,
    ) -> R {
        let module = resolver.module();
        let interner = DbInterner::new_with(db, module.krate(db));
        // Most IDE operations want to operate in PostAnalysis mode, revealing opaques. This makes
        // for a nicer IDE experience. However, method resolution is always done on real code (either
        // existing code or code to be inserted), and there using PostAnalysis is dangerous - we may
        // suggest invalid methods. So we're using the TypingMode of the body we're in.
        let typing_mode = if let Some(store_owner) = resolver.expression_store_owner() {
            TypingMode::analysis_in_body(interner, store_owner.into())
        } else {
            TypingMode::non_body_analysis()
        };
        let infcx = interner.infer_ctxt().build(typing_mode);
        let features = resolver.top_level_def_map().features();
        let environment = self.param_env(db);
        blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
            let ctx = MethodResolutionContext {
                infcx: &infcx,
                resolver,
                param_env: environment.param_env,
                traits_in_scope: TraitsInScope::Set(TraitSet::collect(
                    traits_in_scope.iter().copied(),
                )),
                edition: resolver.krate().data(db).edition,
                features,
                call_span: hir_ty::Span::Dummy,
                receiver_span: hir_ty::Span::Dummy,
                frame,
                errors: None,
            };
            f(&ctx)
        })
    }

    /// Allows you to treat inherent and non-inherent methods differently.
    ///
    /// Note that inherent methods may actually be trait methods! For example, in `dyn Trait`, the trait's methods
    /// are considered inherent methods.
    pub fn iterate_method_candidates_split_inherent(
        &self,
        db: &'db Db,
        scope: &SemanticsScope<'db>,
        traits_in_scope: &HashSet<TraitId>,
        name: Option<&Name>,
        mut callback: impl MethodCandidateCallback,
    ) {
        let _p = tracing::info_span!(
            "iterate_method_candidates_split_inherent",
            traits_in_scope = traits_in_scope.len(),
            ?name,
        )
        .entered();

        self.with_method_resolution(db, scope.resolver(), traits_in_scope, |ctx| {
            // There should be no inference vars in types passed here
            let canonical =
                hir_ty::replace_errors_with_variables(ctx.infcx.interner, &self.ty.skip_binder());
            let (self_ty, _) = ctx.infcx.instantiate_canonical(hir_ty::Span::Dummy, &canonical);

            match name {
                Some(name) => {
                    match ctx.probe_for_name(
                        method_resolution::Mode::MethodCall,
                        *name,
                        self_ty,
                    ) {
                        Ok(candidate)
                        | Err(method_resolution::MethodError::PrivateMatch(candidate)) => {
                            let method_resolution::CandidateId::FunctionId(id) = candidate.item
                            else {
                                unreachable!("`Mode::MethodCall` can only return functions");
                            };
                            let id = Function { id: AnyFunctionId::FunctionId(id) };
                            match candidate.kind {
                                method_resolution::PickKind::InherentImplPick(_)
                                | method_resolution::PickKind::ObjectPick(..)
                                | method_resolution::PickKind::WhereClausePick(..) => {
                                    // Candidates from where clauses and trait objects are considered inherent.
                                    _ = callback.on_inherent_method(id);
                                }
                                method_resolution::PickKind::TraitPick(..) => {
                                    _ = callback.on_trait_method(id);
                                }
                            }
                        }
                        Err(_) => {}
                    };
                }
                None => {
                    _ = ctx.probe_all(method_resolution::Mode::MethodCall, self_ty).try_for_each(
                        |candidate| {
                            let method_resolution::CandidateId::FunctionId(id) =
                                candidate.candidate.item
                            else {
                                unreachable!("`Mode::MethodCall` can only return functions");
                            };
                            let id = Function { id: AnyFunctionId::FunctionId(id) };
                            match candidate.candidate.kind {
                                method_resolution::CandidateKind::InherentImplCandidate {
                                    ..
                                }
                                | method_resolution::CandidateKind::ObjectCandidate(..)
                                | method_resolution::CandidateKind::WhereClauseCandidate(..) => {
                                    // Candidates from where clauses and trait objects are considered inherent.
                                    callback.on_inherent_method(id)
                                }
                                method_resolution::CandidateKind::TraitCandidate(..) => {
                                    callback.on_trait_method(id)
                                }
                            }
                        },
                    );
                }
            }
        })
    }

    #[tracing::instrument(skip_all, fields(name = ?name))]
    pub fn iterate_path_candidates<T>(
        &self,
        db: &'db Db,
        scope: &SemanticsScope<'db>,
        traits_in_scope: &HashSet<TraitId>,
        name: Option<&Name>,
        mut callback: impl FnMut(AssocItem) -> Option<T>,
    ) -> Option<T> {
        let _p = tracing::info_span!("iterate_path_candidates").entered();
        let mut slot = None;

        self.iterate_path_candidates_split_inherent(db, scope, traits_in_scope, name, |item| {
            match callback(item) {
                it @ Some(_) => {
                    slot = it;
                    ControlFlow::Break(())
                }
                None => ControlFlow::Continue(()),
            }
        });
        slot
    }

    /// Iterates over inherent methods.
    ///
    /// In some circumstances, inherent methods methods may actually be trait methods!
    /// For example, when `dyn Trait` is a receiver, _trait_'s methods would be considered
    /// to be inherent methods.
    #[tracing::instrument(skip_all, fields(name = ?name))]
    pub fn iterate_path_candidates_split_inherent(
        &self,
        db: &'db Db,
        scope: &SemanticsScope<'db>,
        traits_in_scope: &HashSet<TraitId>,
        name: Option<&Name>,
        mut callback: impl PathCandidateCallback,
    ) {
        let _p = tracing::info_span!(
            "iterate_path_candidates_split_inherent",
            traits_in_scope = traits_in_scope.len(),
            ?name,
        )
        .entered();

        self.with_method_resolution(db, scope.resolver(), traits_in_scope, |ctx| {
            // There should be no inference vars in types passed here
            let canonical =
                hir_ty::replace_errors_with_variables(ctx.infcx.interner, &self.ty.skip_binder());
            let (self_ty, _) = ctx.infcx.instantiate_canonical(hir_ty::Span::Dummy, &canonical);

            match name {
                Some(name) => {
                    match ctx.probe_for_name(method_resolution::Mode::Path, *name, self_ty) {
                        Ok(candidate)
                        | Err(method_resolution::MethodError::PrivateMatch(candidate)) => {
                            let id = candidate.item.into();
                            match candidate.kind {
                                method_resolution::PickKind::InherentImplPick(_)
                                | method_resolution::PickKind::ObjectPick(..)
                                | method_resolution::PickKind::WhereClausePick(..) => {
                                    // Candidates from where clauses and trait objects are considered inherent.
                                    _ = callback.on_inherent_item(id);
                                }
                                method_resolution::PickKind::TraitPick(..) => {
                                    _ = callback.on_trait_item(id);
                                }
                            }
                        }
                        Err(_) => {}
                    };
                }
                None => {
                    _ = ctx.probe_all(method_resolution::Mode::Path, self_ty).try_for_each(
                        |candidate| {
                            let id = candidate.candidate.item.into();
                            match candidate.candidate.kind {
                                method_resolution::CandidateKind::InherentImplCandidate {
                                    ..
                                }
                                | method_resolution::CandidateKind::ObjectCandidate(..)
                                | method_resolution::CandidateKind::WhereClauseCandidate(..) => {
                                    // Candidates from where clauses and trait objects are considered inherent.
                                    callback.on_inherent_item(id)
                                }
                                method_resolution::CandidateKind::TraitCandidate(..) => {
                                    callback.on_trait_item(id)
                                }
                            }
                        },
                    );
                }
            }
        })
    }

    pub fn as_adt(&self) -> Option<Adt> {
        let (adt, _subst) = self.ty.skip_binder().as_adt()?;
        Some(adt.into())
    }

    /// Holes in the args can come from lifetime/const params.
    pub fn as_adt_with_args(&self) -> Option<(Adt, Vec<Option<Type<'db>>>)> {
        let (adt, args) = self.ty.skip_binder().as_adt()?;
        // alloc: editor edge, the owned argument list callers take
        let args = args.iter().map(|arg| Some(self.derived(arg.ty()?))).collect();
        Some((adt.into(), args))
    }

    pub fn as_builtin(&self) -> Option<BuiltinType> {
        self.ty.skip_binder().as_builtin().map(|inner| BuiltinType { inner })
    }

    pub fn as_dyn_trait(&self) -> Option<Trait> {
        self.ty.skip_binder().dyn_trait().map(Into::into)
    }

    /// If a type can be represented as `dyn Trait`, returns all traits accessible via this type,
    /// or an empty iterator otherwise.
    pub fn applicable_inherent_traits(
        &self,
        db: &'db Db,
    ) -> impl Iterator<Item = Trait> {
        let _p = tracing::info_span!("applicable_inherent_traits").entered();
        self.autoderef_(db)
            .filter_map(|ty| ty.dyn_trait())
            .flat_map(move |dyn_trait_id| hir_ty::all_super_traits(db, dyn_trait_id))
            .copied()
            .map(Trait::from)
    }

    pub fn env_traits(&self, db: &'db Db) -> impl Iterator<Item = Trait> {
        let _p = tracing::info_span!("env_traits").entered();
        let env = self.param_env(db);
        self.autoderef_(db)
            .filter(|ty| matches!(ty.kind(), TyKind::Param(_)))
            .flat_map(move |ty| {
                env.param_env
                    .clauses()
                    .iter()
                    .filter_map(move |pred| match pred.kind().skip_binder() {
                        ClauseKind::Trait(tr) if tr.self_ty() == ty => Some(tr.def_id().0),
                        _ => None,
                    })
                    .flat_map(|t| hir_ty::all_super_traits(db, t))
                    .copied()
            })
            .map(Trait::from)
    }

    /// The trait clauses of the type's environment, elaborated, that are not global, bind no
    /// variables and are not `Sized` or `MetaSized`, whose self type is exactly this type (no
    /// autoderef, unlike [`Type::env_traits`]), each with its type arguments after `Self`.
    /// clippy's `elaborate(param_env.caller_bounds()).filter(|t| t.self_ty() == ty)`.
    pub fn env_bounds(&self, db: &'db Db) -> Vec<(Trait, Vec<Type<'db>>)> {
        let env = self.param_env(db);
        let interner = DbInterner::new_with(db, env.krate);
        let lang_items = hir_def::lang_item::lang_items(db, env.krate);
        let ty = self.ty.skip_binder();
        rustc_type_ir::elaborate::elaborate(interner, env.param_env.clauses().iter())
            .filter(|clause| !clause.is_global())
            .filter_map(|clause| match clause.kind().no_bound_vars()? {
                ClauseKind::Trait(pred) => Some(pred),
                _ => None,
            })
            .filter(|pred| {
                let id = pred.def_id().0;
                Some(id) != lang_items.Sized && Some(id) != lang_items.MetaSized
            })
            .filter(|pred| pred.self_ty() == ty)
            .map(|pred| {
                let args = pred
                    .trait_ref
                    .args
                    .iter()
                    .skip(1)
                    .filter_map(|arg| arg.ty())
                    .map(|arg| self.derived_with(interner, arg))
                    .collect(); // alloc: lint edge, once per by-value parameter, the owned arguments callers take
                (Trait::from(pred.def_id().0), args)
            })
            .collect() // alloc: lint edge, once per by-value parameter, the owned bound list callers take
    }

    pub fn as_impl_traits(&self, db: &'db Db) -> Option<impl Iterator<Item = Trait>> {
        self.ty.skip_binder().impl_trait_bounds(db).map(|it| {
            it.into_iter().filter_map(|pred| match pred.kind().skip_binder() {
                ClauseKind::Trait(trait_ref) => Some(Trait::from(trait_ref.def_id().0)),
                _ => None,
            })
        })
    }

    pub fn as_associated_type_parent_trait(&self, db: &'db Db) -> Option<Trait> {
        let TyKind::Alias(_, AliasTy { kind: AliasTyKind::Projection { def_id }, .. }) =
            self.ty.skip_binder().kind()
        else {
            return None;
        };
        match def_id.0.loc(db).container {
            ItemContainerId::TraitId(id) => Some(Trait { id }),
            _ => None,
        }
    }

    /// A type within this one, which holds no rigid aliases as this one holds none.
    fn derived(&self, ty: Ty<'db>) -> Self {
        Type { owner: self.owner, ty: EarlyBinder::bind_no_rigid_aliases(ty) }
    }

    /// The type as the front end has it, for rustc's printing and checks outside `hir`.
    pub fn to_next_solver(&self) -> Ty<'db> {
        self.ty.skip_binder()
    }

    /// A type of the same owner as `self`. `ty` holds no rigid aliases, as no type `hir` hands
    /// out does and no type built from them without normalizing.
    pub fn derived_from_next_solver(&self, ty: Ty<'db>) -> Type<'db> {
        self.derived(ty)
    }

    /// A type computed from this one, with this one's generics.
    fn derived_with(&self, interner: DbInterner<'db>, ty: Ty<'db>) -> Self {
        Type { owner: self.owner, ty: EarlyBinder::bind(interner, ty) }
    }

    /// Visits every type, including generic arguments, in this type. `callback` is called with type
    /// itself first, and then with its generic arguments.
    pub fn walk(&self, db: &'db Db, callback: impl FnMut(Type<'db>)) {
        struct Visitor<'db, F> {
            db: &'db Db,
            interner: DbInterner<'db>,
            owner: TypeOwnerId,
            callback: F,
            visited: HashSet<Ty<'db>>,
        }
        impl<'db, F> TypeVisitor<DbInterner<'db>> for Visitor<'db, F>
        where
            F: FnMut(Type<'db>),
        {
            type Result = ();

            fn visit_ty(&mut self, ty: Ty<'db>) -> Self::Result {
                if !self.visited.insert(ty) {
                    return;
                }

                let bound = EarlyBinder::bind(self.interner, ty);
                (self.callback)(Type { owner: self.owner, ty: bound });

                if let Some(bounds) = ty.impl_trait_bounds(self.db) {
                    bounds.visit_with(self);
                }

                ty.super_visit_with(self);
            }
        }

        let mut visitor = Visitor {
            db,
            interner: DbInterner::new_no_crate(db),
            owner: self.owner,
            callback,
            visited: HashSet::default(),
        };
        self.ty.skip_binder().visit_with(&mut visitor);
    }
    /// Check if type unifies with another type.
    ///
    /// Note that we consider placeholder types to unify with everything.
    /// For example `Option<T>` and `Option<U>` unify although there is unresolved goal `T = U`.
    pub fn could_unify_with(&self, db: &'db Db, other: &Type<'db>) -> bool {
        self.owner.must_unify(other.owner);
        let env = self.param_env(db);
        let interner = DbInterner::new_no_crate(db);
        let tys = hir_ty::replace_errors_with_variables(
            interner,
            &(self.ty.skip_binder(), other.ty.skip_binder()),
        );
        hir_ty::could_unify(db, env, &tys)
    }

    /// Check if type unifies with another type eagerly making sure there are no unresolved goals.
    ///
    /// This means that placeholder types are not considered to unify if there are any bounds set on
    /// them. For example `Option<T>` and `Option<U>` do not unify as we cannot show that `T = U`
    pub fn could_unify_with_deeply(&self, db: &'db Db, other: &Type<'db>) -> bool {
        self.owner.must_unify(other.owner);
        let env = self.param_env(db);
        let interner = DbInterner::new_no_crate(db);
        let tys = hir_ty::replace_errors_with_variables(
            interner,
            &(self.ty.skip_binder(), other.ty.skip_binder()),
        );
        hir_ty::could_unify_deeply(db, env, &tys)
    }

    pub fn could_coerce_to(&self, db: &'db Db, to: &Type<'db>) -> bool {
        self.owner.must_unify(to.owner);
        let env = self.param_env(db);
        let interner = DbInterner::new_no_crate(db);
        let tys = hir_ty::replace_errors_with_variables(
            interner,
            &(self.ty.skip_binder(), to.ty.skip_binder()),
        );
        hir_ty::could_coerce(db, env, &tys)
    }

    pub fn as_type_param(&self, _db: &'db Db) -> Option<TypeParam> {
        match self.ty.skip_binder().kind() {
            TyKind::Param(param) => Some(TypeParam { id: param.id }),
            _ => None,
        }
    }

    /// Returns unique `GenericParam`s contained in this type.
    pub fn generic_params(&self, db: &'db Db) -> HashSet<GenericParam> {
        hir_ty::collect_params(&self.ty.skip_binder())
            .into_iter()
            .map(|id| TypeOrConstParam { id }.split(db).either_into())
            .collect() // alloc: editor edge, the owned set callers take
    }

    pub fn layout(&self, db: &'db Db) -> Result<Layout<'db>, LayoutError> {
        let env = self.param_env(db);
        db.layout_of_ty(self.ty.skip_binder().store(), env.store())
            .map(|layout| Layout(layout, db.target_data_layout(env.krate).unwrap()))
    }

    pub fn drop_glue(&self, db: &'db Db) -> DropGlue {
        let env = self.param_env(db);
        let interner = DbInterner::new_with(db, env.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
        hir_ty::drop::has_drop_glue(&infcx, self.ty.skip_binder(), env.param_env)
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone, Hash)]
pub struct InlineAsmOperand {
    owner: ExpressionStoreOwnerId,
    expr: ExprId,
    index: usize,
}

impl InlineAsmOperand {
    pub fn parent(self, _db: &Db) -> ExpressionStoreOwner {
        self.owner.into()
    }

    pub fn name(&self, db: &Db) -> Option<Name> {
        let body = ExpressionStore::of(db, self.owner);
        match body.expr(self.expr) {
            hir_def::hir::Expr::InlineAsm(e) => e.operands.get(self.index)?.0,
            _ => None,
        }
    }
}

// FIXME: Document this
#[derive(Debug)]
pub struct Callable<'db> {
    ty: Type<'db>,
    sig: PolyFnSig<'db>,
    /// `sig` with its bound regions erased, as `params` and `return_type` give it. Holds no rigid
    /// aliases, as [`Type::derived`] needs.
    erased: FnSig<'db>,
    callee: Callee<'db>,
    /// Whether this is a method that was called with method call syntax.
    is_bound_method: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Callee<'db> {
    Def(CallableDefId),
    Closure(InternedClosureId<'db>, GenericArgs<'db>),
    CoroutineClosure(InternedCoroutineClosureId<'db>, GenericArgs<'db>),
    FnPtr,
    FnImpl(traits::FnTrait),
    BuiltinDeriveImplMethod { method: BuiltinDeriveImplMethod, impl_: BuiltinDeriveImplId },
}

pub enum CallableKind<'db> {
    Function(Function),
    TupleStruct(Struct),
    TupleEnumVariant(EnumVariant),
    Closure(Closure<'db>),
    FnPtr,
    FnImpl(FnTrait),
}

impl<'db> Callable<'db> {
    pub fn kind(&self) -> CallableKind<'db> {
        match self.callee {
            Callee::Def(CallableDefId::FunctionId(it)) => CallableKind::Function(it.into()),
            Callee::BuiltinDeriveImplMethod { method, impl_ } => CallableKind::Function(Function {
                id: AnyFunctionId::BuiltinDeriveImplMethod { method, impl_ },
            }),
            Callee::Def(CallableDefId::StructId(it)) => CallableKind::TupleStruct(it.into()),
            Callee::Def(CallableDefId::EnumVariantId(it)) => {
                CallableKind::TupleEnumVariant(it.into())
            }
            Callee::Closure(id, subst) => CallableKind::Closure(Closure {
                id: AnyClosureId::ClosureId(id),
                subst,
                owner: self.ty.owner,
            }),
            Callee::CoroutineClosure(id, subst) => CallableKind::Closure(Closure {
                id: AnyClosureId::CoroutineClosureId(id),
                subst,
                owner: self.ty.owner,
            }),
            Callee::FnPtr => CallableKind::FnPtr,
            Callee::FnImpl(fn_) => CallableKind::FnImpl(fn_),
        }
    }

    pub fn as_function(&self) -> Option<Function> {
        match self.callee {
            Callee::Def(CallableDefId::FunctionId(it)) => Some(it.into()),
            Callee::BuiltinDeriveImplMethod { method, impl_ } => {
                Some(Function { id: AnyFunctionId::BuiltinDeriveImplMethod { method, impl_ } })
            }
            _ => None,
        }
    }

    pub fn receiver_param(&self, db: &'db Db) -> Option<(SelfParam, Type<'db>)> {
        if !self.is_bound_method {
            return None;
        }
        let func = self.as_function()?;
        Some((func.self_param(db)?, self.ty.derived(self.erased.inputs()[0])))
    }
    /// The signature's safety: `Unsafe` for an `unsafe fn` item or pointer, `Safe` for a
    /// closure or a call through an `Fn` trait.
    pub fn safety(&self) -> Safety {
        self.erased.safety()
    }
    pub fn n_params(&self) -> usize {
        self.sig.skip_binder().inputs_and_output.inputs().len()
            - if self.is_bound_method { 1 } else { 0 }
    }
    pub fn params(&self) -> Vec<Param<'db>> {
        self.erased
            .inputs()
            .iter()
            .enumerate()
            .skip(if self.is_bound_method { 1 } else { 0 })
            .map(|(idx, ty)| (idx, self.ty.derived(*ty)))
            .map(|(idx, ty)| Param { func: self.callee, idx, ty })
            .collect() // alloc: editor edge, the owned parameter list callers take
    }
    pub fn return_type(&self) -> Type<'db> {
        self.ty.derived(self.erased.output())
    }
    pub fn sig(&self) -> impl Eq {
        &self.sig
    }

    pub fn ty(&self) -> &Type<'db> {
        &self.ty
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layout<'db>(Interned<'static, InternedLayout>, &'db TargetDataLayout);

impl<'db> Layout<'db> {
    pub fn size(&self) -> u64 {
        self.0.size.bytes()
    }

    pub fn align(&self) -> u64 {
        self.0.align.bytes()
    }

    pub fn niches(&self) -> Option<u128> {
        Some(self.0.largest_niche?.available(self.1))
    }

    pub fn field_offset(&self, field: Field) -> Option<u64> {
        match self.0.fields {
            layout::FieldsShape::Primitive => None,
            layout::FieldsShape::Union(_) => Some(0),
            layout::FieldsShape::Array { stride, count } => {
                let i = u64::try_from(field.index()).ok()?;
                (i < count).then_some((stride * i).bytes())
            }
            layout::FieldsShape::Arbitrary { ref offsets, .. } => {
                Some(offsets.get(RustcFieldIdx(field.id))?.bytes())
            }
        }
    }

    pub fn tuple_field_offset(&self, field: usize) -> Option<u64> {
        match self.0.fields {
            layout::FieldsShape::Primitive => None,
            layout::FieldsShape::Union(_) => Some(0),
            layout::FieldsShape::Array { stride, count } => {
                let i = u64::try_from(field).ok()?;
                (i < count).then_some((stride * i).bytes())
            }
            layout::FieldsShape::Arbitrary { ref offsets, .. } => {
                Some(offsets.get(RustcFieldIdx::new(field))?.bytes())
            }
        }
    }

    pub fn tail_padding(&self, field_size: &mut impl FnMut(usize) -> Option<u64>) -> Option<u64> {
        match self.0.fields {
            layout::FieldsShape::Primitive => None,
            layout::FieldsShape::Union(_) => None,
            layout::FieldsShape::Array { stride, count } => count.checked_sub(1).and_then(|tail| {
                let tail_field_size = field_size(tail as usize)?;
                let offset = stride.bytes() * tail;
                self.0.size.bytes().checked_sub(offset)?.checked_sub(tail_field_size)
            }),
            layout::FieldsShape::Arbitrary { ref offsets, ref in_memory_order } => {
                let tail = in_memory_order[in_memory_order.len().checked_sub(1)? as u32];
                let tail_field_size = field_size(tail.0.into_raw().into_u32() as usize)?;
                let offset = offsets.get(tail)?.bytes();
                self.0.size.bytes().checked_sub(offset)?.checked_sub(tail_field_size)
            }
        }
    }

    pub fn largest_padding(
        &self,
        field_size: &mut impl FnMut(usize) -> Option<u64>,
    ) -> Option<u64> {
        match self.0.fields {
            layout::FieldsShape::Primitive => None,
            layout::FieldsShape::Union(_) => None,
            layout::FieldsShape::Array { stride: _, count: 0 } => None,
            layout::FieldsShape::Array { stride, .. } => {
                let size = field_size(0)?;
                stride.bytes().checked_sub(size)
            }
            layout::FieldsShape::Arbitrary { ref offsets, ref in_memory_order } => {
                in_memory_order
                    .iter()
                    .map(|src| (src.0.into_raw().into_u32() as usize, offsets[*src].bytes()))
                    .chain(iter::once((0, self.0.size.bytes())))
                    .array_windows()
                    .filter_map(|[(i, start), (_, end)]| {
                        let size = field_size(i)?;
                        end.checked_sub(start)?.checked_sub(size)
                    })
                    .max()
            }
        }
    }

    pub fn enum_tag_size(&self) -> Option<usize> {
        let tag_size =
            if let layout::Variants::Multiple { tag, tag_encoding, .. } = &self.0.variants {
                match tag_encoding {
                    TagEncoding::Direct => tag.size(self.1).bytes_usize(),
                    TagEncoding::Niche { .. } => 0,
                }
            } else {
                return None;
            };
        Some(tag_size)
    }

    /// For an enum whose tag holds its discriminant directly (`Variants::Multiple` with
    /// `TagEncoding::Direct`): the tag's size and each variant's size, in bytes, in declaration
    /// order (rustc's `variants[i].size`, tag included, as `variant_size_differences` reads
    /// them). `None` for a niche-encoded enum or any layout with one variant.
    pub fn direct_tag_variant_sizes(
        &self,
    ) -> Option<(u64, impl ExactSizeIterator<Item = u64>)> {
        let layout::Variants::Multiple { tag, tag_encoding: TagEncoding::Direct, variants, .. } =
            &self.0.variants
        else {
            return None;
        };
        Some((tag.size(self.1).bytes(), variants.iter().map(|variant| variant.size.bytes())))
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BindingMode {
    Move,
    Ref(Mutability),
}

/// For IDE only
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ScopeDef<'db> {
    ModuleDef(ModuleDef),
    GenericParam(GenericParam),
    ImplSelfType(Impl),
    AdtSelfType(Adt),
    Local(Local<'db>),
    Label(Label),
    Unknown,
}

impl ScopeDef<'_> {
    pub fn all_items(def: PerNs) -> ArrayVec<Self, 3> {
        let mut items = ArrayVec::new();

        match (def.take_types(), def.take_values()) {
            (Some(m1), None) => items.push(ScopeDef::ModuleDef(m1.into())),
            (None, Some(m2)) => items.push(ScopeDef::ModuleDef(m2.into())),
            (Some(m1), Some(m2)) => {
                // Some items, like unit structs and enum variants, are
                // returned as both a type and a value. Here we want
                // to de-duplicate them.
                if m1 != m2 {
                    items.push(ScopeDef::ModuleDef(m1.into()));
                    items.push(ScopeDef::ModuleDef(m2.into()));
                } else {
                    items.push(ScopeDef::ModuleDef(m1.into()));
                }
            }
            (None, None) => {}
        };

        if let Some(macro_def_id) = def.take_macros() {
            items.push(ScopeDef::ModuleDef(ModuleDef::Macro(macro_def_id.into())));
        }

        if items.is_empty() {
            items.push(ScopeDef::Unknown);
        }

        items
    }

    pub fn attrs(&self, db: &Db) -> Option<AttrsWithOwner> {
        match self {
            ScopeDef::ModuleDef(it) => it.attrs(db),
            ScopeDef::GenericParam(it) => Some(it.attrs(db)),
            ScopeDef::ImplSelfType(_)
            | ScopeDef::AdtSelfType(_)
            | ScopeDef::Local(_)
            | ScopeDef::Label(_)
            | ScopeDef::Unknown => None,
        }
    }

    pub fn krate(&self, db: &Db) -> Option<Crate> {
        match self {
            ScopeDef::ModuleDef(it) => it.module(db).map(|m| m.krate(db)),
            ScopeDef::GenericParam(it) => Some(it.module(db).krate(db)),
            ScopeDef::ImplSelfType(_) => None,
            ScopeDef::AdtSelfType(it) => Some(it.module(db).krate(db)),
            ScopeDef::Local(it) => Some(it.module(db).krate(db)),
            ScopeDef::Label(it) => Some(it.module(db).krate(db)),
            ScopeDef::Unknown => None,
        }
    }
}

impl_from!(
    impl<'db>
    ItemInNs { Types => ModuleDef, Values => ModuleDef, Macros => ModuleDef }
    for ScopeDef<'db>
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adjustment<'db> {
    pub source: Type<'db>,
    pub target: Type<'db>,
    pub kind: Adjust,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Adjust {
    /// Go from ! to any type.
    NeverToAny,
    /// Dereference once, producing a place.
    Deref(Option<OverloadedDeref>),
    /// Take the address and produce either a `&` or `*` pointer.
    Borrow(AutoBorrow),
    Pointer(PointerCast),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AutoBorrow {
    /// Converts from T to &T.
    Ref(Mutability),
    /// Converts from T to *T.
    RawPtr(Mutability),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OverloadedDeref(pub Mutability);

impl From<hir_ty::Adjust> for Adjust {
    fn from(adjust: hir_ty::Adjust) -> Adjust {
        match adjust {
            hir_ty::Adjust::NeverToAny => Adjust::NeverToAny,
            hir_ty::Adjust::Deref(Some(hir_ty::OverloadedDeref(m))) => {
                Adjust::Deref(Some(OverloadedDeref(hir_ty::hir_mutability(m))))
            }
            hir_ty::Adjust::Deref(None) => Adjust::Deref(None),
            hir_ty::Adjust::Borrow(hir_ty::AutoBorrow::RawPtr(m)) => {
                Adjust::Borrow(AutoBorrow::RawPtr(hir_ty::hir_mutability(m)))
            }
            hir_ty::Adjust::Borrow(hir_ty::AutoBorrow::Ref(m)) => {
                Adjust::Borrow(AutoBorrow::Ref(hir_ty::hir_mutability(m.into())))
            }
            hir_ty::Adjust::Pointer(pc) => Adjust::Pointer(pc),
        }
    }
}

pub trait HasVisibility {
    fn visibility(&self, db: &Db) -> Visibility;
    fn is_visible_from(&self, db: &Db, module: Module) -> bool {
        let vis = self.visibility(db);
        vis.is_visible_from(db, module.id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClausePolarity {
    /// `T: Trait`
    Positive,
    /// `T: !Trait`
    Negative,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitClause<'db> {
    /// Holds no rigid aliases, like [`TraitRef`]'s.
    inner: hir_ty::next_solver::TraitClause<'db>,
    owner: TypeOwnerId,
}

impl<'db> TraitClause<'db> {
    fn new(
        interner: DbInterner<'db>,
        owner: TypeOwnerId,
        inner: hir_ty::next_solver::TraitClause<'db>,
    ) -> Self {
        let inner = rustc_type_ir::set_aliases_to_non_rigid(interner, inner).skip_normalization();
        Self { inner, owner }
    }

    pub fn polarity(&self) -> ClausePolarity {
        match self.inner.polarity {
            rustc_type_ir::ClausePolarity::Positive => ClausePolarity::Positive,
            rustc_type_ir::ClausePolarity::Negative => ClausePolarity::Negative,
        }
    }

    pub fn trait_ref(&self) -> TraitRef<'db> {
        TraitRef { owner: self.owner, trait_ref: self.inner.trait_ref }
    }

    /// The clause as the front end has it, for rustc's printing outside `hir`.
    pub fn to_next_solver(&self) -> hir_ty::next_solver::TraitClause<'db> {
        self.inner
    }
}

/// One predicate of an item's own where clauses and parameter bounds, as rustc's
/// `explicit_predicates_of` lists them: `T: Trait`, `<T as Trait>::A == U`, `T: 'a`, ...
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WherePredicate<'db> {
    /// Holds no rigid aliases, like [`TraitRef`]'s.
    clause: hir_ty::next_solver::Clause<'db>,
    owner: TypeOwnerId,
}

impl<'db> WherePredicate<'db> {
    /// The trait bound it is, with its bounded type as the trait ref's self type.
    pub fn as_trait(&self) -> Option<TraitClause<'db>> {
        match self.clause.kind().skip_binder() {
            ClauseKind::Trait(inner) => Some(TraitClause { inner, owner: self.owner }),
            _ => None,
        }
    }

    /// The types the predicate names, outermost only, in order: the bounded type and the
    /// bound's arguments, a projection's arguments and its value, an outlived type, a const
    /// argument's type. A type under `for<'a>` keeps its bound regions.
    pub fn types(&self) -> Vec<Type<'db>> {
        struct Outermost<'db> {
            owner: TypeOwnerId,
            out: Vec<Type<'db>>,
        }
        impl<'db> TypeVisitor<DbInterner<'db>> for Outermost<'db> {
            type Result = ();
            fn visit_ty(&mut self, ty: Ty<'db>) {
                let ty = EarlyBinder::bind_no_rigid_aliases(ty);
                self.out.push(Type { owner: self.owner, ty });
            }
        }
        // alloc: editor edge, the owned type list callers take
        let mut visitor = Outermost { owner: self.owner, out: Vec::new() };
        self.clause.visit_with(&mut visitor);
        visitor.out
    }

    /// The predicate as the front end has it, for rustc's printing outside `hir`.
    pub fn to_next_solver(&self) -> hir_ty::next_solver::Clause<'db> {
        self.clause
    }
}

/// A function's signature in its own generics, with no bound lifetimes: [`Function::signature`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FnSignature<'db> {
    sig: FnSig<'db>,
    owner: GenericDefId,
}

impl<'db> FnSignature<'db> {
    /// The parameters' types, `self` included.
    pub fn params(&self, db: &'db Db) -> Vec<Type<'db>> {
        let interner = DbInterner::new_no_crate(db);
        // alloc: editor edge, the owned parameter list callers take
        self.sig.inputs().iter().map(|&ty| Type::new_in(interner, self.owner, ty)).collect()
    }

    pub fn ret(&self, db: &'db Db) -> Type<'db> {
        Type::new(db, self.owner, self.sig.output())
    }

    pub fn safety(&self) -> Safety {
        self.sig.safety()
    }

    /// `ExternAbi::Rust` for a function with no `extern`.
    pub fn abi(&self) -> hir_def::layout::ExternAbi {
        self.sig.abi()
    }

    /// Whether the last parameter is C's `...`.
    pub fn c_variadic(&self) -> bool {
        self.sig.c_variadic()
    }

    pub fn to_next_solver(&self) -> FnSig<'db> {
        self.sig
    }
}

/// [`Function::impl_signature_mismatch`]: where a trait impl's method's signature differs
/// from its trait's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplSignatureMismatch<'db> {
    pub trait_method: Function,
    pub at: SignatureMismatchAt,
    /// The trait's type and the impl's at `at`, when it is a parameter's or the return type.
    pub types: Option<(Type<'db>, Type<'db>)>,
    /// The trait method's signature with the impl's arguments, in the impl method's generics.
    pub expected: FnSignature<'db>,
    pub found: FnSignature<'db>,
}

/// [`Function::proc_macro_signature_mismatch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcMacroSignatureMismatch<'db> {
    pub at: SignatureMismatchAt,
    /// `fn(TokenStream) -> TokenStream`, or with two `TokenStream`s.
    pub expected: FnSignature<'db>,
    pub found: FnSignature<'db>,
}

/// Trait for obtaining the defining crate of an item.
pub trait HasCrate {
    fn krate(&self, db: &Db) -> Crate;
}

impl<T: hir_def::HasModule> HasCrate for T {
    fn krate(&self, db: &Db) -> Crate {
        self.module(db).krate(db).into()
    }
}

impl HasCrate for AssocItem {
    fn krate(&self, db: &Db) -> Crate {
        self.module(db).krate(db)
    }
}

impl HasCrate for Struct {
    fn krate(&self, db: &Db) -> Crate {
        self.module(db).krate(db)
    }
}

impl HasCrate for Union {
    fn krate(&self, db: &Db) -> Crate {
        self.module(db).krate(db)
    }
}

impl HasCrate for Enum {
    fn krate(&self, db: &Db) -> Crate {
        self.module(db).krate(db)
    }
}

impl HasCrate for Field {
    fn krate(&self, db: &Db) -> Crate {
        self.parent_def(db).module(db).krate(db)
    }
}

impl HasCrate for EnumVariant {
    fn krate(&self, db: &Db) -> Crate {
        self.module(db).krate(db)
    }
}

impl HasCrate for Function {
    fn krate(&self, db: &Db) -> Crate {
        self.module(db).krate(db)
    }
}

impl HasCrate for Const {
    fn krate(&self, db: &Db) -> Crate {
        self.module(db).krate(db)
    }
}

impl HasCrate for TypeAlias {
    fn krate(&self, db: &Db) -> Crate {
        self.module(db).krate(db)
    }
}

impl HasCrate for Type<'_> {
    fn krate(&self, db: &Db) -> Crate {
        self.krate(db).into()
    }
}

impl HasCrate for Macro {
    fn krate(&self, db: &Db) -> Crate {
        self.module(db).krate(db)
    }
}

impl HasCrate for Trait {
    fn krate(&self, db: &Db) -> Crate {
        self.module(db).krate(db)
    }
}

impl HasCrate for Static {
    fn krate(&self, db: &Db) -> Crate {
        self.module(db).krate(db)
    }
}

impl HasCrate for Adt {
    fn krate(&self, db: &Db) -> Crate {
        self.module(db).krate(db)
    }
}

impl HasCrate for Impl {
    fn krate(&self, db: &Db) -> Crate {
        self.module(db).krate(db)
    }
}

impl HasCrate for Module {
    fn krate(&self, db: &Db) -> Crate {
        Module::krate(*self, db)
    }
}

impl<'db> HasCrate for AnonConst<'db> {
    fn krate(&self, db: &Db) -> Crate {
        hir_def::HasModule::krate(&self.id.loc(db).owner, db).into()
    }
}

pub trait HasContainer {
    fn container(&self, db: &Db) -> ItemContainer;
}

impl HasContainer for ExternCrateDecl {
    fn container(&self, db: &Db) -> ItemContainer {
        container_id_to_hir(self.id.lookup(db).container.into())
    }
}

impl HasContainer for Module {
    fn container(&self, db: &Db) -> ItemContainer {
        // FIXME: handle block expressions as modules (their parent is in a different DefMap)
        let def_map = self.id.def_map(db);
        match def_map.module(db, self.id).parent {
            Some(parent_id) => ItemContainer::Module(Module { id: parent_id }),
            None => ItemContainer::Crate(def_map.krate().into()),
        }
    }
}

impl HasContainer for Function {
    fn container(&self, db: &Db) -> ItemContainer {
        match self.id {
            AnyFunctionId::FunctionId(id) => container_id_to_hir(id.lookup(db).container),
            AnyFunctionId::BuiltinDeriveImplMethod { impl_, .. } => {
                ItemContainer::Impl(Impl { id: AnyImplId::BuiltinDeriveImplId(impl_) })
            }
        }
    }
}

impl HasContainer for Struct {
    fn container(&self, db: &Db) -> ItemContainer {
        ItemContainer::Module(Module { id: self.id.lookup(db).container })
    }
}

impl HasContainer for Union {
    fn container(&self, db: &Db) -> ItemContainer {
        ItemContainer::Module(Module { id: self.id.lookup(db).container })
    }
}

impl HasContainer for Enum {
    fn container(&self, db: &Db) -> ItemContainer {
        ItemContainer::Module(Module { id: self.id.lookup(db).container })
    }
}

impl HasContainer for TypeAlias {
    fn container(&self, db: &Db) -> ItemContainer {
        container_id_to_hir(self.id.lookup(db).container)
    }
}

impl HasContainer for Const {
    fn container(&self, db: &Db) -> ItemContainer {
        container_id_to_hir(self.id.lookup(db).container)
    }
}

impl HasContainer for Static {
    fn container(&self, db: &Db) -> ItemContainer {
        container_id_to_hir(self.id.lookup(db).container)
    }
}

impl HasContainer for Trait {
    fn container(&self, db: &Db) -> ItemContainer {
        ItemContainer::Module(Module { id: self.id.lookup(db).container })
    }
}

impl HasContainer for ExternBlock {
    fn container(&self, db: &Db) -> ItemContainer {
        ItemContainer::Module(Module { id: self.id.lookup(db).container })
    }
}

pub trait HasName {
    fn name(&self, db: &Db) -> Option<Name>;
}

macro_rules! impl_has_name {
    ( $( $ty:ident ),* $(,)? ) => {
        $(
            impl HasName for $ty {
                fn name(&self, db: &Db) -> Option<Name> {
                    (*self).name(db).into()
                }
            }
        )*
    };
}

impl_has_name!(
    ModuleDef,
    Module,
    Field,
    Struct,
    Union,
    Enum,
    EnumVariant,
    Adt,
    Variant,
    DefWithBody,
    Function,
    ExternCrateDecl,
    Const,
    Static,
    Trait,
    TypeAlias,
    Macro,
    ExternAssocItem,
    AssocItem,
    DeriveHelper,
    ToolModule,
    Label,
    GenericParam,
    TypeParam,
    LifetimeParam,
    ConstParam,
    TypeOrConstParam,
    InlineAsmOperand,
);

macro_rules! impl_has_name_no_db {
    ( $( $ty:ident ),* $(,)? ) => {
        $(
            impl HasName for $ty {
                fn name(&self, _db: &Db) -> Option<Name> {
                    (*self).name().into()
                }
            }
        )*
    };
}

impl_has_name_no_db!(StaticLifetime, BuiltinType, BuiltinAttr);

impl HasName for Local<'_> {
    fn name(&self, db: &Db) -> Option<Name> {
        (*self).name(db).into()
    }
}

impl HasName for TupleField<'_> {
    fn name(&self, _db: &Db) -> Option<Name> {
        (*self).name().into()
    }
}

impl HasName for Param<'_> {
    fn name(&self, db: &Db) -> Option<Name> {
        self.name(db)
    }
}

fn container_id_to_hir(c: ItemContainerId) -> ItemContainer {
    match c {
        ItemContainerId::ExternBlockId(id) => ItemContainer::ExternBlock(ExternBlock { id }),
        ItemContainerId::ModuleId(id) => ItemContainer::Module(Module { id }),
        ItemContainerId::ImplId(id) => ItemContainer::Impl(id.into()),
        ItemContainerId::TraitId(id) => ItemContainer::Trait(Trait { id }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemContainer {
    Trait(Trait),
    Impl(Impl),
    Module(Module),
    ExternBlock(ExternBlock),
    Crate(Crate),
}

/// What a doc link can resolve to.
pub enum DocLinkDef {
    ModuleDef(ModuleDef),
    Field(Field),
    SelfType(Trait),
}

pub trait MethodCandidateCallback {
    fn on_inherent_method(&mut self, f: Function) -> ControlFlow<()>;

    fn on_trait_method(&mut self, f: Function) -> ControlFlow<()>;
}

impl<F> MethodCandidateCallback for F
where
    F: FnMut(Function) -> ControlFlow<()>,
{
    fn on_inherent_method(&mut self, f: Function) -> ControlFlow<()> {
        self(f)
    }

    fn on_trait_method(&mut self, f: Function) -> ControlFlow<()> {
        self(f)
    }
}

pub trait PathCandidateCallback {
    fn on_inherent_item(&mut self, item: AssocItem) -> ControlFlow<()>;

    fn on_trait_item(&mut self, item: AssocItem) -> ControlFlow<()>;
}

impl<F> PathCandidateCallback for F
where
    F: FnMut(AssocItem) -> ControlFlow<()>,
{
    fn on_inherent_item(&mut self, item: AssocItem) -> ControlFlow<()> {
        self(item)
    }

    fn on_trait_item(&mut self, item: AssocItem) -> ControlFlow<()> {
        self(item)
    }
}

pub fn resolve_absolute_path<'a, I: Iterator<Item = Symbol> + Clone + 'a>(
    db: &'a Db,
    mut segments: I,
) -> impl Iterator<Item = ItemInNs> + use<'a, I> {
    segments
        .next()
        .into_iter()
        .flat_map(move |crate_name| {
            all_crates(db)
                .iter()
                .filter(|&krate| {
                    krate
                        .extra_data(db)
                        .display_name
                        .as_ref()
                        .is_some_and(|name| *name.crate_name().symbol() == crate_name)
                })
                .filter_map(|&krate| {
                    let segments = segments.clone(); // clone: the path, walked again per crate
                    let mut def_map = crate_def_map(db, krate);
                    let mut module = def_map.module(db, def_map.root_module_id());
                    let mut segments = segments.with_position().peekable();
                    while let Some((_, segment)) =
                        segments.next_if(|&(position, _)| !position.is_last)
                    {
                        let res = module
                            .scope(db)
                            .get(&Name::new_symbol_root(segment))
                            .take_types()
                            .and_then(|res| match res {
                                ModuleDefId::ModuleId(it) => Some(it),
                                _ => None,
                            })?;
                        def_map = res.def_map(db);
                        module = def_map.module(db, res);
                    }
                    let (_, item_name) = segments.next()?;
                    let res = module.scope(db).get(&Name::new_symbol_root(item_name));
                    Some(res.iter_items().map(|(item, _)| item.into()))
                })
                // alloc: editor edge; outlives the `all_crates` handle, one per crate of that name
                .collect::<Vec<_>>()
        })
        .flatten()
}

fn as_name_opt(name: Option<impl AsName>) -> Name {
    name.map_or_else(Name::missing, |name| name.as_name())
}

#[track_caller]
fn generic_args_from_tys<'db>(
    interner: DbInterner<'db>,
    def_id: SolverDefId<'db>,
    args: impl IntoIterator<Item: Borrow<Type<'db>>>,
) -> (GenericArgs<'db>, TypeOwnerId) {
    let mut owner = None::<TypeOwnerId>;
    let mut args = args.into_iter();
    let args = GenericArgs::for_item(interner, def_id, |_, id, _, _| {
        if matches!(id, GenericParamId::TypeParamId(_))
            && let Some(arg) = args.next()
        {
            let arg = arg.borrow();

            match &mut owner {
                Some(owner) => *owner = owner.must_unify(arg.owner),
                None => owner = Some(arg.owner),
            }

            arg.ty.skip_binder().into()
        } else {
            next_solver::GenericArg::error_from_id(interner, id)
        }
    });
    let owner =
        owner.unwrap_or_else(|| TypeOwnerId::NoParams(Type::builtin_type_crate(interner.db())));
    (args, owner)
}

fn has_non_default_type_params(db: &Db, generic_def: GenericDefId) -> bool {
    let params = GenericParams::of(db, generic_def);
    let defaults = db.generic_defaults(generic_def);
    params
        .iter_type_or_consts()
        .filter(|(_, param)| matches!(param, TypeOrConstParamData::TypeParamData(_)))
        .map(|(local_id, _)| TypeOrConstParamId { parent: generic_def, local_id })
        .any(|param| {
            let param = hir_ty::type_or_const_param_idx(db, param);
            defaults.get(param as usize).is_none()
        })
}

fn param_env_from_has_crate<'db>(
    db: &'db Db,
    id: impl hir_def::HasModule + Into<GenericDefId> + Copy,
) -> ParamEnvAndCrate<'db> {
    ParamEnvAndCrate { param_env: db.trait_environment(id.into()), krate: id.krate(db) }
}

// FIXME: We probably don't want to expose this.
pub trait MacroCallIdExt {
    fn loc(self, db: &Db) -> &hir_expand::MacroCallLoc;
}
impl MacroCallIdExt for span::MacroCallId {
    #[inline]
    fn loc(self, db: &Db) -> &hir_expand::MacroCallLoc {
        hir_expand::MacroCallId::from(self).loc(db)
    }
}

// Like https://github.com/rust-lang/rust/blob/7c3c88f42ad444f4688b865591d84660be4ece2f/compiler/rustc_middle/src/ty/util.rs#L254-L310
fn struct_tail_raw<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    mut ty: Ty<'db>,
    mut normalize: impl FnMut(Ty<'db>) -> Ty<'db>,
) -> Ty<'db> {
    let recursion_limit = 16;
    for iteration in 0.. {
        if iteration >= recursion_limit {
            return Ty::new_error(interner, ErrorGuaranteed);
        }
        match ty.kind() {
            TyKind::Adt(def, args) => {
                let AdtId::StructId(def_id) = def.def_id() else { break };
                let last_field = db.field_types(def_id.into()).iter().next_back();
                match last_field {
                    Some((_, field)) => {
                        ty = normalize(field.ty().instantiate(interner, args).skip_norm_wip())
                    }
                    None => break,
                }
            }
            TyKind::Tuple(tys) if let Some((&last_ty, _)) = tys.split_last() => {
                ty = last_ty;
            }
            TyKind::Tuple(_) => break,
            TyKind::Pat(inner, _) => {
                ty = inner;
            }
            _ => {
                break;
            }
        }
    }
    ty
}
