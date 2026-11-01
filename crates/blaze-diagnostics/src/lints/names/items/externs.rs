//! What `rustc_lint` finds of the types items hand to foreign code:
//!
//! - `improper_ctypes`: a parameter, return or static type of an `extern` block with a non-Rust
//!   ABI that is not FFI-safe, and the non-Rust `fn` pointer types in the signatures of a Rust-ABI
//!   block's functions;
//! - `improper_ctypes_definitions`: the same of a function defined with a non-Rust ABI, and of the
//!   non-Rust `fn` pointer types in other functions, statics, consts, type aliases and fields;
//! - `uses_power_alignment`: on AIX, a field after the first of a `repr(C)` struct that holds a
//!   float wider than 4 bytes first, which the power alignment rule places differently;
//! - `clashing_extern_declarations`: a function of an `extern` block whose symbol a function
//!   declared before it has too, with a signature that is not structurally the same (its own
//!   query, after the late lint passes).
//!
//! Follows `compiler/rustc_lint/src/types/improper_ctypes.rs`, `compiler/rustc_lint/src/types.rs`
//! (`transparent_newtype_field`, `ty_is_known_nonnull`, `get_nullable_type`,
//! `is_niche_optimization_candidate`, `repr_nullable_ptr`),
//! `compiler/rustc_lint/src/foreign_modules.rs` and `compiler/rustc_lint/src/lints.rs`
//! (`ImproperCTypes`, `UsesPowerAlignment`, `BuiltinClashingExtern`) (MIT or Apache-2.0).

use hir_ty::db::HirDatabase;
use std::{cell::OnceCell, str::FromStr};

use base_db::Db;
use blaze_hash::{HashMap, HashSet};
use blaze_items::{Fact, ItemId};
use blaze_mem::Frame;
use either::Either;
use hir::{InFile, ModuleDef};
use hir_def::{
    AdtId, AttrDefId, CallableDefId, EnumId, FunctionId, GenericDefId, HasModule as _, Lookup,
    VariantId, attrs::AttrFlags, item_tree::FieldsShape, nameres::crate_def_map,
    signatures::FunctionSignature, src::HasSource as _,
};
use hir_ty::{
    ParamEnvAndCrate, TyDefId, ValueTyDefId,
    consteval::try_const_usize,
    next_solver::{
        AdtDef, AliasTy, CallableIdWrapper, DbInterner, FnSig, GenericArgs, ParamEnv, Pattern,
        Region, RegionExt as _, Ty, TyKind, TypingMode, ensure_sufficient_stack,
        infer::{DbInternerInferExt, InferCtxt, traits::ObligationCause},
    },
};
use intern::{Symbol, sym};
use rustc_abi::{BackendRepr, ExternAbi, ReprOptions};
use rustc_type_ir::{
    AliasTyKind, PatternKind,
    inherent::{AdtDef as _, GenericArgs as _, IntoKind as _, Ty as _},
};
use smallvec::SmallVec;
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange, WalkEvent,
    view::{self as ast, AstNode},
};

use super::{
    Krate, def_span,
    walk::{self, Container, Item, Orders},
};
use crate::{
    cx::{Cx, item_range_view},
    lower::{Found, Phase},
    model::Diag,
};

/// Whether the item is defined in Rust and handed to foreign code, or declared in Rust and defined
/// elsewhere.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CItemKind {
    Declaration,
    Definition,
}

#[derive(Clone, Copy)]
enum FnPos {
    Arg,
    Ret,
}

enum FfiResult<'db> {
    Safe,
    Phantom(Ty<'db>),
    Unsafe { ty: Ty<'db>, reason: &'static str, help: Option<&'static str> },
}

/// rustc's `RootUseFlags`: how the type the visitor started at is used.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Root {
    func: bool,
    ret: bool,
    defined: bool,
    /// Only the type of something: a `fn` pointer's.
    theoretical: bool,
}

const STATIC_TY: Root = Root { func: false, ret: false, defined: false, theoretical: false };

/// rustc's `OuterTyKind`: what the current type is directly in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outer {
    None,
    NoneThroughFnPtr,
    Other,
}

#[derive(Clone, Copy)]
struct State {
    root: Root,
    outer: Outer,
    depth: usize,
}

impl State {
    fn next(self) -> State {
        State { root: self.root, outer: Outer::Other, depth: self.depth + 1 }
    }

    fn next_in_fnptr(self, pos: FnPos) -> State {
        let ret = matches!(pos, FnPos::Ret);
        State {
            root: Root { func: true, ret, defined: false, theoretical: true },
            outer: Outer::NoneThroughFnPtr,
            depth: self.depth + 1,
        }
    }

    fn fn_entry(mode: CItemKind, pos: FnPos) -> State {
        let root = Root {
            func: true,
            ret: matches!(pos, FnPos::Ret),
            defined: mode == CItemKind::Definition,
            theoretical: false,
        };
        State { root, outer: Outer::None, depth: 0 }
    }

    fn static_entry() -> State {
        State { root: STATIC_TY, outer: Outer::None, depth: 0 }
    }

    fn is_in_defined_function(self) -> bool {
        self.root.defined && self.root.func
    }

    fn is_in_fnptr(self) -> bool {
        self.root.theoretical && self.root.func
    }

    fn can_expect_ty_params(self) -> bool {
        self.root.theoretical || self.is_in_defined_function()
    }
}

/// The types of one item, in its param env.
struct Types<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    interner: DbInterner<'db>,
    /// Built when a type is first normalized or tested for `Sized`: most items never are.
    infcx: OnceCell<InferCtxt<'db>>,
    env: ParamEnv<'db>,
    krate: base_db::Crate,
    limit: usize,
    /// The ADTs whose representation was read since the last `emit`, for the facts.
    adts: Vec<AdtId, &'a Frame>,
}

impl<'c, 'a, 'db> Types<'c, 'a, 'db> {
    fn new(cx: &'c Cx<'a, 'db>, owner: GenericDefId) -> Self {
        let db = cx.db;
        let krate = cx.krate.base();
        let interner = DbInterner::new_with(db, krate);
        let env = db.trait_environment(owner);
        let limit = crate_def_map(db, krate).recursion_limit() as usize;
        Types { cx, interner, infcx: OnceCell::new(), env, krate, limit, adts: Vec::new_in(cx.frame()) }
    }

    fn infcx(&self) -> &InferCtxt<'db> {
        self.infcx.get_or_init(|| self.interner.infer_ctxt().build(TypingMode::non_body_analysis()))
    }

    fn repr(&self, adt: AdtId) -> ReprOptions {
        AdtDef::new(adt, self.interner).repr(self.cx.db)
    }

    fn erased(&self, ty: Ty<'db>) -> Ty<'db> {
        let interner = self.interner;
        rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_erased(interner))
    }

    /// rustc's `maybe_normalize_erasing_regions`.
    fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
        let ty = self.erased(ty);
        let cause = ObligationCause::dummy();
        match self.infcx().at(&cause, self.env).deeply_normalize(ty) {
            Ok(it) => self.erased(it),
            Err(_) => ty,
        }
    }

    fn is_sized(&self, ty: Ty<'db>) -> bool {
        self.infcx().type_is_sized_modulo_regions(self.env, ty)
    }

    fn is_1zst(&self, ty: Ty<'db>, env: ParamEnv<'db>) -> Option<bool> {
        let env = ParamEnvAndCrate { param_env: env, krate: self.krate };
        self.cx.db.layout_of_ty(ty.store(), env.store()).ok().map(|it| it.is_1zst())
    }

    fn is_local(&self, adt: AdtId) -> bool {
        adt.krate(self.cx.db) == self.krate
    }

    /// rustc's `VariantDef::field_list_has_applicable_non_exhaustive`.
    fn fields_non_exhaustive(&self, adt: AdtId, variant: VariantId) -> bool {
        let owner = match variant {
            VariantId::EnumVariantId(it) => AttrDefId::EnumVariantId(it),
            _ => AttrDefId::AdtId(adt),
        };
        !self.is_local(adt) && AttrFlags::query(self.cx.db, owner).contains(AttrFlags::NON_EXHAUSTIVE)
    }

    fn variants_non_exhaustive(&self, adt: EnumId) -> bool {
        AttrFlags::query(self.cx.db, AttrDefId::AdtId(AdtId::EnumId(adt))).contains(AttrFlags::NON_EXHAUSTIVE)
    }

    /// rustc's `transparent_newtype_field`: the type, given `args`, of the first field of
    /// `variant` that is not a 1-ZST as declared.
    fn transparent_field(&self, adt: AdtId, variant: VariantId, args: GenericArgs<'db>) -> Option<Ty<'db>> {
        let db = self.cx.db;
        let env = db.trait_environment(GenericDefId::AdtId(adt));
        let types = db.field_types(variant);
        variant.fields(db).fields().iter().find_map(|(field, _)| {
            let ty = types[field].ty();
            if self.is_1zst(ty.skip_binder(), env) == Some(true) {
                return None;
            }
            Some(ty.instantiate(self.interner, args).skip_norm_wip())
        })
    }

    fn nonnull_optimization_guaranteed(&self, adt: AdtId) -> bool {
        walk::attr(self.cx, adt_source(self.cx, adt).value, "rustc_nonnull_optimization_guaranteed").is_some()
    }

    /// rustc's `ty_is_known_nonnull`.
    fn is_known_nonnull(&self, ty: Ty<'db>) -> bool {
        let ty = self.normalize(ty);
        match ty.kind() {
            TyKind::FnPtr(..) | TyKind::Ref(..) => true,
            TyKind::Adt(def, _) if def.is_box() => true,
            TyKind::Adt(def, args) if self.repr(def.def_id()).transparent() && !def.is_union() => {
                let adt = def.def_id();
                if self.nonnull_optimization_guaranteed(adt) {
                    return true;
                }
                let lang = AttrFlags::lang_item(self.cx.db, AttrDefId::AdtId(adt));
                if lang.is_some_and(|it| it == sym::unsafe_cell || it == sym::unsafe_pinned) {
                    return false;
                }
                variants(self.cx.db, adt)
                    .filter_map(|variant| self.transparent_field(adt, variant, args))
                    .any(|field| self.is_known_nonnull(field))
            }
            TyKind::Pat(base, pat) => self.is_known_nonnull(base) || self.pat_is_known_nonnull(pat),
            _ => false,
        }
    }

    fn pat_is_known_nonnull(&self, pat: Pattern<'db>) -> bool {
        match pat.kind() {
            PatternKind::Range { start, end } => {
                match (try_const_usize(self.cx.db, start), try_const_usize(self.cx.db, end)) {
                    (Some(start), Some(end)) => start > 0 && end >= start,
                    _ => false,
                }
            }
            PatternKind::NotNull => true,
            PatternKind::Or(pats) => pats.iter().all(|it| self.pat_is_known_nonnull(it)),
        }
    }

    /// rustc's `get_nullable_type`.
    fn nullable(&self, ty: Ty<'db>) -> Option<Ty<'db>> {
        let ty = self.normalize(ty);
        match ty.kind() {
            TyKind::Adt(def, args) => {
                let adt = def.def_id();
                let inner = variants(self.cx.db, adt).filter_map(|variant| self.transparent_field(adt, variant, args)).last()?;
                self.nullable(inner)
            }
            TyKind::Pat(base, _) => self.nullable(base),
            TyKind::Int(_) | TyKind::Uint(_) | TyKind::Char | TyKind::RawPtr(..) | TyKind::FnPtr(..) => Some(ty),
            TyKind::Ref(_, inner, mutbl) => Some(Ty::new_ptr(self.interner, inner, mutbl)),
            _ => None,
        }
    }

    /// rustc's `is_niche_optimization_candidate`.
    fn is_niche_candidate(&self, ty: Ty<'db>) -> bool {
        if self.is_1zst(ty, self.env) == Some(false) {
            return false;
        }
        match ty.kind() {
            TyKind::Adt(def, _) => match def.def_id() {
                AdtId::StructId(it) => VariantId::StructId(it).fields(self.cx.db).fields().is_empty(),
                AdtId::EnumId(it) => !self.variants_non_exhaustive(it) && variants(self.cx.db, def.def_id()).next().is_none(),
                AdtId::UnionId(_) => false,
            },
            TyKind::Tuple(tys) => tys.is_empty(),
            _ => false,
        }
    }

    /// rustc's `repr_nullable_ptr`: the nullable type an `Option`-like enum of a non-null type is
    /// represented as.
    fn repr_nullable_ptr(&self, ty: Ty<'db>) -> Option<Ty<'db>> {
        match ty.kind() {
            TyKind::Adt(def, args) => {
                let db = self.cx.db;
                let mut all = variants(db, def.def_id());
                let (Some(one), Some(two), None) = (all.next(), all.next(), all.next()) else { return None };
                let (mut one, mut two) = (field_tys(db, self.interner, one, args), field_tys(db, self.interner, two, args));
                let field = match ((one.next(), one.next()), (two.next(), two.next())) {
                    ((None, _), (Some(field), None)) | ((Some(field), None), (None, _)) => field,
                    ((Some(a), None), (Some(b), None)) => {
                        if self.is_niche_candidate(a) {
                            b
                        } else if self.is_niche_candidate(b) {
                            a
                        } else {
                            return None;
                        }
                    }
                    _ => return None,
                };
                if !self.is_known_nonnull(field) {
                    return None;
                }
                let db = self.cx.db;
                let env = ParamEnvAndCrate { param_env: self.env, krate: self.krate };
                let layout = db.layout_of_ty(field.store(), env.store()).ok()?;
                let data_layout = db.target_data_layout(self.krate).ok()?;
                let BackendRepr::Scalar(scalar) = &layout.backend_repr else { return None };
                let range = scalar.valid_range(data_layout);
                let full = scalar.size(data_layout).unsigned_int_max();
                if (range.start == 0 && range.end == full - 1) || range.start == 1 {
                    return self.nullable(field);
                }
                None
            }
            TyKind::Pat(base, _) => self.nullable(base),
            _ => None,
        }
    }

    fn is_diagnostic_item(&self, adt: AdtId, names: &[&str]) -> bool {
        AttrFlags::diagnostic_item(self.cx.db, AttrDefId::AdtId(adt)).is_some_and(|it| names.contains(&it.as_str()))
    }
}

/// rustc's `ImproperCTypesVisitor`: whether a type is FFI-safe.
struct Visitor<'t, 'c, 'a, 'db> {
    types: &'t mut Types<'c, 'a, 'db>,
    cache: HashSet<Ty<'db>>,
    base_ty: Ty<'db>,
    base_mode: CItemKind,
}

impl<'t, 'c, 'a, 'db> Visitor<'t, 'c, 'a, 'db> {
    fn new(types: &'t mut Types<'c, 'a, 'db>, base_ty: Ty<'db>, base_mode: CItemKind) -> Self {
        let base_ty = if opaque(base_ty).is_some() { base_ty } else { types.normalize(base_ty) };
        Visitor { types, cache: HashSet::default(), base_ty, base_mode }
    }

    fn check_type(&mut self, state: State, ty: Ty<'db>) -> FfiResult<'db> {
        if let Some(ty) = opaque(ty) {
            return FfiResult::Unsafe { ty, reason: "opaque types have no C equivalent", help: None };
        }
        let ty = self.types.normalize(ty);
        if let Some(ty) = opaque(ty) {
            return FfiResult::Unsafe { ty, reason: "opaque types have no C equivalent", help: None };
        }
        self.visit_type(state, ty)
    }

    fn visit_indirection(&mut self, state: State, ty: Ty<'db>, inner: Ty<'db>, boxed: bool) -> FfiResult<'db> {
        if boxed {
            if state.is_in_defined_function() || (state.is_in_fnptr() && self.base_mode == CItemKind::Definition) {
                if self.types.is_sized(inner) {
                    return FfiResult::Safe;
                }
                return FfiResult::Unsafe { ty, reason: "box cannot be represented as a single pointer", help: None };
            }
            return match ty.kind() {
                TyKind::Adt(def, args) => self.visit_struct_or_union(state, ty, def.def_id(), args),
                _ => FfiResult::Safe,
            };
        }
        if (state.is_in_defined_function() || state.is_in_fnptr()) && self.types.is_sized(inner) {
            FfiResult::Safe
        } else {
            self.visit_type(state.next(), inner)
        }
    }

    fn visit_variant_fields(
        &mut self,
        state: State,
        ty: Ty<'db>,
        adt: AdtId,
        variant: VariantId,
        args: GenericArgs<'db>,
    ) -> FfiResult<'db> {
        let transparent = self.types.repr(adt).transparent();
        let all_zst = if transparent {
            match self.types.transparent_field(adt, variant, args) {
                Some(field) => {
                    let field = self.types.normalize(field);
                    match self.visit_type(state.next(), field) {
                        FfiResult::Unsafe { ty, .. } if ty.is_unit() => {}
                        r => return r,
                    }
                    false
                }
                None => true,
            }
        } else {
            false
        };
        let mut fields = field_tys(self.types.cx.db, self.types.interner, variant, args).peekable();
        let mut all_phantom = fields.peek().is_some();
        for field in fields {
            let field = self.types.normalize(field);
            all_phantom &= match self.visit_type(state.next(), field) {
                FfiResult::Safe => false,
                FfiResult::Unsafe { ty, .. } if ty.is_unit() => false,
                FfiResult::Phantom(_) => true,
                r @ FfiResult::Unsafe { .. } => return r,
            };
        }
        if all_phantom {
            FfiResult::Phantom(ty)
        } else if all_zst {
            FfiResult::Unsafe { ty, reason: "this struct contains only zero-sized fields", help: None }
        } else {
            FfiResult::Safe
        }
    }

    fn visit_struct_or_union(&mut self, state: State, ty: Ty<'db>, adt: AdtId, args: GenericArgs<'db>) -> FfiResult<'db> {
        self.types.adts.push(adt);
        let repr = self.types.repr(adt);
        let is_struct = matches!(adt, AdtId::StructId(_));
        if !repr.c() && !repr.transparent() {
            let (reason, help) = if is_struct {
                (
                    "this struct has unspecified layout",
                    "consider adding a `#[repr(C)]` or `#[repr(transparent)]` attribute to this struct",
                )
            } else {
                (
                    "this union has unspecified layout",
                    "consider adding a `#[repr(C)]` or `#[repr(transparent)]` attribute to this union",
                )
            };
            return FfiResult::Unsafe { ty, reason, help: Some(help) };
        }
        let variant = match adt {
            AdtId::StructId(it) => VariantId::StructId(it),
            AdtId::UnionId(it) => VariantId::UnionId(it),
            AdtId::EnumId(_) => return FfiResult::Safe,
        };
        if self.types.fields_non_exhaustive(adt, variant) {
            let reason = if is_struct { "this struct is non-exhaustive" } else { "this union is non-exhaustive" };
            return FfiResult::Unsafe { ty, reason, help: None };
        }
        if variant.fields(self.types.cx.db).fields().is_empty() {
            let (reason, help) = if is_struct {
                ("this struct has no fields", "consider adding a member to this struct")
            } else {
                ("this union has no fields", "consider adding a member to this union")
            };
            return FfiResult::Unsafe { ty, reason, help: Some(help) };
        }
        self.visit_variant_fields(state, ty, adt, variant, args)
    }

    fn visit_enum(&mut self, state: State, ty: Ty<'db>, adt: EnumId, args: GenericArgs<'db>) -> FfiResult<'db> {
        let id = AdtId::EnumId(adt);
        self.types.adts.push(id);
        let mut all = variants(self.types.cx.db, id).peekable();
        if all.peek().is_none() {
            return FfiResult::Safe;
        }
        let repr = self.types.repr(id);
        if !repr.c() && !repr.transparent() && repr.int.is_none() {
            if let Some(inner) = self.types.repr_nullable_ptr(ty) {
                return self.visit_type(state.next(), inner);
            }
            return FfiResult::Unsafe {
                ty,
                reason: "enum has no representation hint",
                help: Some(
                    "consider adding a `#[repr(C)]`, `#[repr(transparent)]`, or integer `#[repr(...)]` attribute to this enum",
                ),
            };
        }
        let non_exhaustive = !self.types.is_local(id) && self.types.variants_non_exhaustive(adt);
        for variant in all {
            let unit = matches!(variant.fields(self.types.cx.db).shape, FieldsShape::Unit);
            if non_exhaustive && !unit {
                return FfiResult::Unsafe { ty, reason: "this enum is non-exhaustive", help: None };
            }
            if self.types.fields_non_exhaustive(id, variant) {
                return FfiResult::Unsafe { ty, reason: "this enum has non-exhaustive variants", help: None };
            }
            match self.visit_variant_fields(state, ty, id, variant, args) {
                FfiResult::Safe => {}
                r => return r,
            }
        }
        FfiResult::Safe
    }

    fn visit_type(&mut self, state: State, ty: Ty<'db>) -> FfiResult<'db> {
        if !self.cache.insert(ty) || state.depth > self.types.limit {
            return FfiResult::Safe;
        }
        let unsafe_ = |reason, help| FfiResult::Unsafe { ty, reason, help };
        match ty.kind() {
            TyKind::Adt(def, args) => {
                if def.is_box() {
                    return self.visit_indirection(state, ty, args.type_at(0), true);
                }
                if def.is_phantom_data() {
                    return FfiResult::Phantom(ty);
                }
                match def.def_id() {
                    AdtId::EnumId(it) => self.visit_enum(state, ty, it, args),
                    adt => {
                        if self.types.is_diagnostic_item(adt, &["cstring_type", "cstr_type"])
                            && !is_mutable_ptr(self.base_ty)
                        {
                            return unsafe_(
                                "`CStr`/`CString` do not have a guaranteed layout",
                                Some("consider passing a `*const std::ffi::c_char` instead, and use `CStr::as_ptr()`"),
                            );
                        }
                        self.visit_struct_or_union(state, ty, adt, args)
                    }
                }
            }
            TyKind::Pat(base, _) => self.visit_type(state, base),
            TyKind::Int(_) | TyKind::Uint(_) | TyKind::Float(_) | TyKind::Bool => FfiResult::Safe,
            TyKind::Char => {
                unsafe_("the `char` type has no C equivalent", Some("consider using `u32` or `libc::wchar_t` instead"))
            }
            TyKind::Slice(_) => unsafe_("slices have no C equivalent", Some("consider using a raw pointer instead")),
            TyKind::Dynamic(..) => unsafe_("trait objects have no C equivalent", None),
            TyKind::Str => {
                unsafe_("string slices have no C equivalent", Some("consider using `*const u8` and a length instead"))
            }
            TyKind::Tuple(tys) => {
                if tys.is_empty() && state.root.ret && matches!(state.outer, Outer::None | Outer::NoneThroughFnPtr) {
                    FfiResult::Safe
                } else {
                    unsafe_("tuples have unspecified layout", Some("consider using a struct instead"))
                }
            }
            TyKind::RawPtr(inner, _) if matches!(inner.kind(), TyKind::Tuple(tys) if tys.is_empty()) => FfiResult::Safe,
            TyKind::RawPtr(inner, _) => self.visit_indirection(state, ty, inner, false),
            TyKind::Ref(_, inner, _) => self.visit_indirection(state, ty, inner, false),
            TyKind::Array(inner, _) => {
                if state.root.func && state.outer == Outer::None {
                    unsafe_("passing raw arrays by value is not FFI-safe", Some("consider passing a pointer to the array"))
                } else {
                    self.visit_type(state.next(), inner)
                }
            }
            TyKind::FnPtr(sig_tys, hdr) => {
                let sig = sig_tys.with(hdr);
                if sig.abi().is_rustic_abi() {
                    return unsafe_(
                        "this function pointer has Rust-specific calling convention",
                        Some("consider using an `extern fn(...) -> ...` function pointer instead"),
                    );
                }
                let sig = self.types.interner.instantiate_bound_regions_with_erased(sig);
                for &arg in sig.inputs() {
                    match self.visit_type(state.next_in_fnptr(FnPos::Arg), arg) {
                        FfiResult::Safe => {}
                        r => return r,
                    }
                }
                self.visit_type(state.next_in_fnptr(FnPos::Ret), sig.output())
            }
            TyKind::Foreign(_) | TyKind::Never => FfiResult::Safe,
            TyKind::Alias(_, AliasTy { kind: AliasTyKind::Opaque { .. }, .. }) => {
                unsafe_("opaque types have no C equivalent", None)
            }
            TyKind::Param(_) | TyKind::Alias(_, AliasTy { kind: AliasTyKind::Projection { .. } | AliasTyKind::Inherent { .. }, .. })
                if state.can_expect_ty_params() =>
            {
                FfiResult::Safe
            }
            TyKind::UnsafeBinder(_) => unsafe_("unsafe binders are incompatible with foreign function interfaces", None),
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) | TyKind::Coroutine(..) | TyKind::CoroutineWitness(..) => {
                unsafe_("closures and coroutines are not FFI-safe", None)
            }
            // rustc's `bug!`s: parameters where none can be, inference and bound variables,
            // placeholders, errors and `fn` items, none of which an item's type has once it
            // has been checked.
            TyKind::Param(_)
            | TyKind::Alias(..)
            | TyKind::Infer(_)
            | TyKind::Bound(..)
            | TyKind::Placeholder(_)
            | TyKind::Error(_)
            | TyKind::FnDef(..) => FfiResult::Safe,
        }
    }
}

/// The first opaque type in `ty`, as rustc's `ProhibitOpaqueTypes` finds it.
fn opaque(ty: Ty<'_>) -> Option<Ty<'_>> {
    ty.walk()
        .filter_map(|it| it.ty())
        .find(|it| matches!(it.kind(), TyKind::Alias(_, AliasTy { kind: AliasTyKind::Opaque { .. }, .. })))
}

fn is_mutable_ptr(ty: Ty<'_>) -> bool {
    matches!(
        ty.kind(),
        TyKind::RawPtr(_, rustc_type_ir::Mutability::Mut) | TyKind::Ref(_, _, rustc_type_ir::Mutability::Mut)
    )
}

fn adt_source<'db>(cx: &Cx<'_, 'db>, adt: AdtId) -> InFile<ast::Adt<'db>> {
    let db = cx.db;
    match adt {
        AdtId::StructId(it) => it.lookup(db).source_view(db).map(ast::Adt::Struct),
        AdtId::UnionId(it) => it.lookup(db).source_view(db).map(ast::Adt::Union),
        AdtId::EnumId(it) => it.lookup(db).source_view(db).map(ast::Adt::Enum),
    }
}

/// The variants of an ADT, in order: a struct's or a union's one.
fn variants(db: &Db, adt: AdtId) -> impl Iterator<Item = VariantId> + '_ {
    match adt {
        AdtId::StructId(it) => Either::Left(std::iter::once(VariantId::StructId(it))),
        AdtId::UnionId(it) => Either::Left(std::iter::once(VariantId::UnionId(it))),
        AdtId::EnumId(it) => {
            Either::Right(it.enum_variants(db).variants.values().map(|&(it, _)| VariantId::EnumVariantId(it)))
        }
    }
}

/// The types of `variant`'s fields given `args`, in order.
fn field_tys<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    variant: VariantId,
    args: GenericArgs<'db>,
) -> impl Iterator<Item = Ty<'db>> + 'db {
    let types = db.field_types(variant);
    variant.fields(db).fields().iter().map(move |(field, _)| types[field].ty().instantiate(interner, args).skip_norm_wip())
}

/// The ABI an `extern` block or a `fn` pointer type is written with, `None` if it is not one.
pub(super) fn abi_of(abi: Option<ast::Abi<'_>>, default: ExternAbi) -> Option<ExternAbi> {
    let Some(abi) = abi else { return Some(default) };
    match abi.abi_string() {
        Some(it) => ExternAbi::from_str(&it.value().ok()?).ok(),
        None => Some(ExternAbi::C { unwind: false }),
    }
}

/// Where a lint of a type is emitted.
struct Site<'i, 'db> {
    at: InFile<SyntaxNodePtr>,
    /// The item whose signature is the lint's fact, named only once a lint fires.
    item: &'i Item<'db>,
}

struct Pass<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    found: Vec<Found>,
}

impl<'db> Pass<'_, '_, 'db> {
    /// rustc's `check_type_for_external_abi_fnptr`: each non-Rust `fn` pointer type in `ty`,
    /// paired with the `fn` pointer types written in `hir_ty` in the order both are visited.
    #[allow(clippy::too_many_arguments)]
    fn fnptrs(
        &mut self,
        types: &mut Types<'_, '_, 'db>,
        site: &Site<'_, 'db>,
        state: State,
        hir_ty: InFile<ast::Type<'_>>,
        ty: Ty<'db>,
        mode: CItemKind,
    ) {
        let mut tys = ty
            .walk()
            .filter_map(|it| it.ty())
            .filter(|it| match it.kind() {
                TyKind::FnPtr(_, hdr) => !hdr.abi().is_rustic_abi(),
                _ => false,
            })
            .peekable();
        if tys.peek().is_none() {
            return;
        }
        let mut written: SmallVec<[(usize, TextRange); 4]> = SmallVec::new();
        let mut depth = 0;
        for event in hir_ty.value.syntax().preorder() {
            match event {
                WalkEvent::Enter(node) => {
                    if !counted(node) {
                        continue;
                    }
                    depth += 1;
                    if let Some(ptr) = ast::FnPtrType::cast(node)
                        && abi_of(ptr.abi(), ExternAbi::Rust).is_some_and(|it| !it.is_rustic_abi())
                    {
                        let range = match node.parent().filter(|it| ast::ForType::can_cast(it.kind())) {
                            Some(for_ty) => for_ty.text_range(),
                            None => node.text_range(),
                        };
                        written.push((depth, range));
                    }
                }
                WalkEvent::Leave(node) => {
                    if counted(node) {
                        depth -= 1;
                    }
                }
            }
        }
        for (fn_ptr, (depth, range)) in tys.zip(written) {
            let mut visitor = Visitor::new(&mut *types, fn_ptr, mode);
            let result = visitor.check_type(State { depth, ..state }, fn_ptr);
            self.emit(types, site, hir_ty.with_value(range), result, mode);
        }
    }

    /// rustc's `check_foreign_fn` and `check_fn_for_external_abi_fnptr` of a function, by its ABI.
    fn function(&mut self, item: &Item<'db>, f: ast::Fn<'db>, mode: CItemKind) {
        let cx = self.cx;
        let db = cx.db;
        let node = item.node.with_value(f);
        let Some(def) = cx.vsema.to_def2(node) else { return };
        let Ok(id) = FunctionId::try_from(def) else { return };
        let sig = db.callable_item_signature(CallableDefId::FunctionId(id)).skip_binder();
        let abi = sig.abi();
        let mut types = Types::new(cx, GenericDefId::FunctionId(id));
        let sig = types.interner.instantiate_bound_regions_with_erased(sig);
        let site = Site { at: item.ptr(), item };
        let params = f.param_list();
        let this = params.and_then(|it| it.self_param());
        let written = || params.into_iter().flat_map(|it| it.params()).filter_map(|it| it.ty());
        let output = f.ret_type().and_then(|it| it.ty());
        if !abi.is_rustic_abi() {
            let this_span = this.map(|this| this.ty().map_or(this.syntax().text_range(), |it| it.syntax().text_range()));
            let spans = this_span.into_iter().chain(written().map(|ty| ty.syntax().text_range()));
            for (&input, range) in sig.inputs().iter().zip(spans) {
                let mut visitor = Visitor::new(&mut types, input, mode);
                let result = visitor.check_type(State::fn_entry(mode, FnPos::Arg), input);
                self.emit(&mut types, &site, item.node.with_value(range), result, mode);
            }
            if let Some(output) = output {
                let ret = sig.output();
                let mut visitor = Visitor::new(&mut types, ret, mode);
                let result = visitor.check_type(State::fn_entry(mode, FnPos::Ret), ret);
                self.emit(&mut types, &site, item.node.with_value(output.syntax().text_range()), result, mode);
            }
        } else {
            let this_ty = this.and_then(|it| it.ty());
            let implicit_self = this.is_some() && this_ty.is_none();
            let tys = sig.inputs().iter().copied().skip(usize::from(implicit_self));
            for (input, hir_ty) in tys.zip(this_ty.into_iter().chain(written())) {
                let state = State::fn_entry(mode, FnPos::Arg);
                self.fnptrs(&mut types, &site, state, item.node.with_value(hir_ty), input, mode);
            }
            if let Some(output) = output {
                let state = State::fn_entry(mode, FnPos::Ret);
                self.fnptrs(&mut types, &site, state, item.node.with_value(output), sig.output(), mode);
            }
        }
    }

    /// rustc's `check_foreign_static`.
    fn foreign_static(&mut self, item: &Item<'db>, s: ast::Static<'db>) {
        let cx = self.cx;
        let Some(def) = cx.vsema.to_def2(item.node.with_value(s)) else { return };
        let Some(hir_ty) = s.ty() else { return };
        let id: hir_def::StaticId = def.into();
        let ty = cx.db.type_for_static(id).skip_binder();
        let mut types = Types::new(cx, GenericDefId::StaticId(id));
        let site = Site { at: item.ptr(), item };
        let mode = CItemKind::Declaration;
        let mut visitor = Visitor::new(&mut types, ty, mode);
        let result = visitor.check_type(State::static_entry(), ty);
        self.emit(&mut types, &site, item.node.with_value(hir_ty.syntax().text_range()), result, mode);
    }

    /// rustc's `check_item` of a static, a const or a type alias.
    fn typed(&mut self, item: &Item<'db>, owner: GenericDefId, ty: Ty<'db>, hir_ty: Option<ast::Type<'db>>) {
        let Some(hir_ty) = hir_ty else { return };
        let mut types = Types::new(self.cx, owner);
        let site = Site { at: item.ptr(), item };
        let state = State::static_entry();
        self.fnptrs(&mut types, &site, state, item.node.with_value(hir_ty), ty, CItemKind::Definition);
    }

    /// rustc's `check_struct_for_power_alignment`, on AIX.
    fn power_alignment(&mut self, item: &Item<'db>) {
        let cx = self.cx;
        let Some(adt) = ast::Adt::cast(item.node.value.syntax()) else { return };
        let Some(def) = cx.vsema.to_def2(item.node.with_value(adt)) else { return };
        let id = AdtId::from(def);
        let types = Types::new(cx, GenericDefId::AdtId(id));
        let repr = types.repr(id);
        if !repr.c() || repr.packed() || repr.align.is_some() {
            return;
        }
        for &field in walk::fields(item).iter().skip(1) {
            let Some(ty) = field_ty(cx, field) else { continue };
            if !power_aligned(&types, ty) {
                continue;
            }
            let span = cx.span(field.map(def_span));
            let mut diag = Diag::lint(
                "uses_power_alignment",
                "repr(C) does not follow the power alignment rule. This may affect platform C ABI compatibility for this type",
                span,
            );
            if let Some((it, name)) = super::item_id(cx, item) {
                diag.fact(Fact::Layout(cx.key(it)), format!("the layout of `{name}`")); // alloc: the firing lint's fact
            }
            self.found.push(Found::lint(diag, Phase::Late, field.map(ast::ptr)));
        }
    }

    /// rustc's `check_field_def` of each field of an ADT.
    fn fields(&mut self, item: &Item<'db>) {
        let cx = self.cx;
        let Some(adt) = ast::Adt::cast(item.node.value.syntax()) else { return };
        let Some(def) = cx.vsema.to_def2(item.node.with_value(adt)) else { return };
        let mut types = Types::new(cx, GenericDefId::AdtId(AdtId::from(def)));
        for &field in &walk::fields(item) {
            let Some(ty) = field_ty(cx, field) else { continue };
            let Some(hir_ty) = field.value.children().find_map(ast::Type::cast) else { continue };
            let site = Site { at: field.map(ast::ptr), item };
            let state = State::static_entry();
            self.fnptrs(&mut types, &site, state, field.with_value(hir_ty), ty, CItemKind::Definition);
        }
    }

    fn emit(
        &mut self,
        types: &mut Types<'_, '_, 'db>,
        site: &Site<'_, 'db>,
        range: InFile<TextRange>,
        result: FfiResult<'db>,
        mode: CItemKind,
    ) {
        let (ty, note, help) = match result {
            FfiResult::Safe => {
                types.adts.clear();
                return;
            }
            FfiResult::Phantom(ty) => (ty, "composed only of `PhantomData`", None),
            FfiResult::Unsafe { ty, reason, help } => (ty, reason, help),
        };
        let cx = self.cx;
        let (lint, desc) = match mode {
            CItemKind::Declaration => ("improper_ctypes", "block"),
            CItemKind::Definition => ("improper_ctypes_definitions", "fn"),
        };
        let printed = crate::print::solver_ty(cx, ty);
        let span = cx.span(range);
        // alloc: the firing lint's message; clone: its span, labelled too
        let mut diag = Diag::lint(lint, format!("`extern` {desc} uses type `{printed}`, which is not FFI-safe"), span.clone());
        diag.span_label(span, "not FFI-safe");
        if let Some(help) = help {
            diag.help(help);
        }
        diag.note(note);
        if let TyKind::Adt(def, _) = ty.kind()
            && types.is_local(def.def_id())
        {
            let source = adt_source(cx, def.def_id());
            diag.span_note(cx.span(source.map(|it| def_span(it.syntax()))), "the type is defined here");
        }
        if let Some((id, name)) = super::item_id(cx, site.item) {
            diag.fact(Fact::Signature(cx.key(id)), format!("the signature of `{name}`")); // alloc: the firing lint's fact
        }
        let mut seen = HashSet::default();
        for &adt in &types.adts {
            if seen.insert(adt) {
                let name = cx.def_path(ModuleDef::Adt(adt.into()));
                // alloc: the firing lint's fact
                diag.fact(Fact::Layout(cx.key(ItemId::Adt(adt))), format!("the representation of `{name}`"));
            }
        }
        types.adts.clear();
        self.found.push(Found::lint(diag, Phase::Late, site.at));
    }
}

/// Whether a type does what rustc's `check_arg_for_power_alignment` looks for: a float wider
/// than 4 bytes, or a `repr(C)` struct holding one.
fn power_aligned<'db>(types: &Types<'_, '_, 'db>, ty: Ty<'db>) -> bool {
    match ty.kind() {
        TyKind::Float(it) => matches!(it, rustc_type_ir::FloatTy::F64 | rustc_type_ir::FloatTy::F128),
        TyKind::Adt(def, _) => {
            let AdtId::StructId(it) = def.def_id() else { return false };
            let repr = types.repr(def.def_id());
            if !repr.c() || repr.packed() || repr.align.is_some() {
                return false;
            }
            let variant = VariantId::StructId(it);
            let field_types = types.cx.db.field_types(variant);
            variant
                .fields(types.cx.db)
                .fields()
                .iter()
                .any(|(field, _)| power_aligned(types, field_types[field].ty().skip_binder()))
        }
        _ => false,
    }
}

fn field_def<'db>(cx: &Cx<'_, 'db>, field: InFile<ast::Node<'db>>) -> Option<hir::Field> {
    match field.value.kind() {
        SyntaxKind::RECORD_FIELD => {
            let it = ast::RecordField::cast(field.value)?;
            cx.vsema.to_def2(field.with_value(it))
        }
        SyntaxKind::TUPLE_FIELD => {
            let it = ast::TupleField::cast(field.value)?;
            cx.vsema.to_def2(field.with_value(it))
        }
        _ => None,
    }
}

/// A field's type as declared.
fn field_ty<'db>(cx: &Cx<'_, 'db>, field: InFile<ast::Node<'db>>) -> Option<Ty<'db>> {
    let id = hir_def::FieldId::from(field_def(cx, field)?);
    Some(cx.db.field_types(id.parent)[id.local_id].ty().skip_binder())
}

/// Whether a syntax node is a type rustc's HIR has one of: not the parentheses around one,
/// nor the `for<..>` of a `fn` pointer type.
fn counted(node: ast::Node<'_>) -> bool {
    ast::Type::can_cast(node.kind()) && !ast::ParenType::can_cast(node.kind()) && !ast::ForType::can_cast(node.kind())
}

/// rustc's `ImproperCTypesLint` at an item itself: `check_foreign_item`, and `check_item` of a
/// static, a const, a type alias or a `repr(C)` struct.
pub(super) fn ctypes_item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, krate: &Krate) -> Vec<Found> {
    let mut pass = Pass { cx, found: Vec::new() }; // alloc: empty until a lint fires; holds only what fires
    let db = cx.db;
    match (item.node.value, item.container) {
        (ast::Item::Fn(f), Container::Extern(_)) => pass.function(item, f, CItemKind::Declaration),
        (ast::Item::Static(s), Container::Extern(block)) => {
            if abi_of(block.value.abi(), ExternAbi::C { unwind: false }).is_some_and(|it| !it.is_rustic_abi()) {
                pass.foreign_static(item, s);
            }
        }
        (ast::Item::Static(s), Container::Module) => {
            if let Some(def) = cx.vsema.to_def2(item.node.with_value(s)) {
                let id: hir_def::StaticId = def.into();
                let ty = db.type_for_static(id).skip_binder();
                pass.typed(item, GenericDefId::StaticId(id), ty, s.ty());
            }
        }
        (ast::Item::Const(c), Container::Module) => {
            if let Some(def) = cx.vsema.to_def2(item.node.with_value(c)) {
                let id: hir_def::ConstId = def.into();
                if let Some(ty) = db.value_ty(ValueTyDefId::ConstId(id)) {
                    pass.typed(item, GenericDefId::ConstId(id), ty.skip_binder(), c.ty());
                }
            }
        }
        (ast::Item::TypeAlias(t), Container::Module) => {
            if let Some(def) = cx.vsema.to_def2(item.node.with_value(t)) {
                let id: hir_def::TypeAliasId = def.into();
                let ty = db.ty(TyDefId::TypeAliasId(id)).skip_binder();
                pass.typed(item, GenericDefId::TypeAliasId(id), ty, t.ty());
            }
        }
        (ast::Item::Struct(_), _) if krate.aix => pass.power_alignment(item),
        _ => {}
    }
    pass.found
}

/// rustc's `ImproperCTypesLint` as the walk of an item reaches what is in it, after every pass's
/// `check_item`: `check_field_def` of each field, `check_fn` of a function with a body.
pub(super) fn ctypes_nested<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Vec<Found> {
    let mut pass = Pass { cx, found: Vec::new() }; // alloc: empty until a lint fires; holds only what fires
    match (item.node.value, item.container) {
        (ast::Item::Fn(_), Container::Extern(_)) => {}
        (ast::Item::Fn(f), _) if f.body().is_some() => pass.function(item, f, CItemKind::Definition),
        (ast::Item::Struct(_) | ast::Item::Union(_) | ast::Item::Enum(_), _) => pass.fields(item),
        _ => {}
    }
    pass.found
}

/// rustc's `SymbolName`: the symbol an `extern` function links against, interned.
fn symbol(cx: &Cx<'_, '_>, id: FunctionId) -> Symbol {
    match AttrFlags::link_name(cx.db, AttrDefId::FunctionId(id)) {
        Some(name) => name,
        None => FunctionSignature::of(cx.db, id).name.symbol().clone(), // clone: an interned `Symbol`'s handle
    }
}

/// The span of the `#[link_name]` that names an `extern` function's symbol, if one does; read
/// only for a lint that fires.
fn link_name(cx: &Cx<'_, '_>, id: FunctionId, f: ast::Fn<'_>) -> Option<TextRange> {
    AttrFlags::link_name(cx.db, AttrDefId::FunctionId(id))?;
    walk::attr(cx, f, "link_name").map(|(_, range)| range)
}

/// rustc's `structurally_same_type`.
struct Same<'t, 'c, 'a, 'db> {
    types: &'t Types<'c, 'a, 'db>,
    seen: HashSet<(Ty<'db>, Ty<'db>)>,
}

impl<'db> Same<'_, '_, '_, 'db> {
    /// A transparent newtype's non-ZST field, through each, unless the newtype is non-null.
    fn non_transparent(&self, mut ty: Ty<'db>) -> Ty<'db> {
        loop {
            if let TyKind::Adt(def, args) = ty.kind() {
                let adt = def.def_id();
                if self.types.repr(adt).transparent() && !self.types.nonnull_optimization_guaranteed(adt) {
                    let first = variants(self.types.cx.db, adt).next();
                    if let Some(field) = first.and_then(|variant| self.types.transparent_field(adt, variant, args)) {
                        ty = field;
                        continue;
                    }
                }
            }
            return ty;
        }
    }

    fn sigs(&mut self, a: FnSig<'db>, b: FnSig<'db>) -> bool {
        (a.abi(), a.safety(), a.c_variadic()) == (b.abi(), b.safety(), b.c_variadic())
            && a.inputs().len() == b.inputs().len()
            && a.inputs().iter().zip(b.inputs()).all(|(&a, &b)| self.ty(a, b))
            && self.ty(a.output(), b.output())
    }

    fn fn_def_sig(&self, def: CallableIdWrapper, args: GenericArgs<'db>) -> FnSig<'db> {
        let interner = self.types.interner;
        let sig = self.types.cx.db.callable_item_signature(def.0).instantiate(interner, args).skip_norm_wip();
        interner.instantiate_bound_regions_with_erased(sig)
    }

    fn ty(&mut self, a: Ty<'db>, b: Ty<'db>) -> bool {
        ensure_sufficient_stack(|| self.ty_inner(a, b))
    }

    fn ty_inner(&mut self, a: Ty<'db>, b: Ty<'db>) -> bool {
        let a = self.non_transparent(a);
        let b = self.non_transparent(b);
        if !self.seen.insert((a, b)) || a == b {
            return true;
        }
        let primitive_or_pointer = |ty: Ty<'db>| {
            matches!(
                ty.kind(),
                TyKind::Bool
                    | TyKind::Char
                    | TyKind::Int(_)
                    | TyKind::Uint(_)
                    | TyKind::Float(_)
                    | TyKind::RawPtr(..)
                    | TyKind::Ref(..)
            )
        };
        match (a.kind(), b.kind()) {
            (TyKind::Adt(a_def, a_args), TyKind::Adt(b_def, b_args)) => {
                let (a_repr, b_repr) = (self.types.repr(a_def.def_id()), self.types.repr(b_def.def_id()));
                if !(a_repr.c() && b_repr.c()) {
                    return false;
                }
                if (a_repr.pack, a_repr.align, a_repr.simd()) != (b_repr.pack, b_repr.align, b_repr.simd()) {
                    return false;
                }
                let (db, interner) = (self.types.cx.db, self.types.interner);
                let fields = |def: AdtId, args| variants(db, def).flat_map(move |variant| field_tys(db, interner, variant, args));
                let (a_def, b_def) = (a_def.def_id(), b_def.def_id());
                fields(a_def, a_args).count() == fields(b_def, b_args).count()
                    && fields(a_def, a_args).zip(fields(b_def, b_args)).all(|(a, b)| self.ty(a, b))
            }
            (TyKind::Array(a_ty, a_len), TyKind::Array(b_ty, b_len)) => a_len == b_len && self.ty(a_ty, b_ty),
            (TyKind::Slice(a_ty), TyKind::Slice(b_ty)) => self.ty(a_ty, b_ty),
            (TyKind::RawPtr(a_ty, a_mut), TyKind::RawPtr(b_ty, b_mut)) => a_mut == b_mut && self.ty(a_ty, b_ty),
            (TyKind::Ref(_, a_ty, a_mut), TyKind::Ref(_, b_ty, b_mut)) => a_mut == b_mut && self.ty(a_ty, b_ty),
            (TyKind::FnDef(a_def, a_args), TyKind::FnDef(b_def, b_args)) => {
                let a_sig = self.fn_def_sig(a_def, a_args.skip_binder());
                let b_sig = self.fn_def_sig(b_def, b_args.skip_binder());
                self.sigs(a_sig, b_sig)
            }
            (TyKind::Adt(..) | TyKind::Pat(..), _) if primitive_or_pointer(b) => {
                self.types.repr_nullable_ptr(a).is_some_and(|it| it == b)
            }
            (_, TyKind::Adt(..) | TyKind::Pat(..)) if primitive_or_pointer(a) => {
                self.types.repr_nullable_ptr(b).is_some_and(|it| it == a)
            }
            _ => false,
        }
    }
}

/// `clashing_extern_declarations`: each function of an `extern` block, in the order of rustc's
/// `foreign_items`, against the first declared before it with the same symbol.
pub(super) fn lints<'db>(cx: &Cx<'_, 'db>, items: &[Item<'db>]) -> Vec<Found> {
    let db = cx.db;
    let mut decls = Vec::new_in(cx.frame());
    let mut orders = Orders::default();
    for item in items {
        let (ast::Item::Fn(f), Container::Extern(_)) = (item.node.value, item.container) else { continue };
        let Some(def) = cx.vsema.to_def2(item.node.with_value(f)) else { continue };
        let Ok(id) = FunctionId::try_from(def) else { continue };
        let lo = cx.span(item.node.map(|it| item_range_view(it.syntax()))).lo;
        decls.push((orders.key(cx, item.module, lo), item, f, def, id));
    }
    if decls.len() < 2 {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    decls.sort_by(|a, b| a.0.cmp(&b.0));
    let mut found = Vec::new(); // alloc: empty until a lint fires; holds only what fires
    let mut seen: HashMap<Symbol, usize> = HashMap::with_capacity_and_hasher(decls.len(), Default::default());
    for index in 0..decls.len() {
        let (_, item, f, def, id) = &decls[index];
        let first = match seen.entry(symbol(cx, *id)) {
            std::collections::hash_map::Entry::Occupied(it) => *it.get(),
            std::collections::hash_map::Entry::Vacant(it) => {
                it.insert(index);
                continue;
            }
        };
        let (_, first_item, first_f, first_def, first_id) = &decls[first];
        let types = Types::new(cx, GenericDefId::FunctionId(*id));
        let interner = types.interner;
        let sig_of = |id: FunctionId| db.callable_item_signature(CallableDefId::FunctionId(id)).skip_binder();
        let (expected, this_sig) = (sig_of(*first_id), sig_of(*id));
        let mut same = Same { types: &types, seen: HashSet::default() };
        let erased = (
            interner.instantiate_bound_regions_with_erased(expected),
            interner.instantiate_bound_regions_with_erased(this_sig),
        );
        if same.sigs(erased.0, erased.1) {
            continue;
        }
        // From here on the lint fires.
        // alloc: the firing lint's message
        let this = FunctionSignature::of(db, *id).name.as_str().to_owned();
        let orig = symbol(cx, *first_id);
        let orig = orig.as_str();
        let (orig_link, this_link) = (link_name(cx, *first_id, *first_f), link_name(cx, *id, *f));
        let relevant = |item: &Item<'_>, link: Option<TextRange>| {
            cx.span(item.node.map(|it| link.unwrap_or_else(|| item_range_view(it.syntax()))))
        };
        let previous = relevant(*first_item, orig_link);
        let mismatch = relevant(*item, this_link);
        let message = if orig == this {
            format!("`{this}` redeclared with a different signature") // alloc: the firing lint's message
        } else {
            format!("`{this}` redeclares `{orig}` with a different signature") // alloc: the firing lint's message
        };
        // clone: the firing lint's span, labelled too
        let mut diag = Diag::lint("clashing_extern_declarations", message, mismatch.clone());
        diag.span_label(previous, format!("`{orig}` previously declared here")); // alloc: its label
        diag.span_label(mismatch, "this signature doesn't match the previous declaration");
        let print = |def: &hir::Function, sig| cx.ty(&def.ret_type(db).derived_from_next_solver(Ty::new_fn_ptr(interner, sig)));
        diag.note(format!("expected `{}`\n   found `{}`", print(first_def, expected), print(def, this_sig))); // alloc: its note
        let first_name = FunctionSignature::of(db, *first_id).name.as_str().to_owned(); // alloc: its fact
        // alloc: its facts
        diag.fact(Fact::Signature(cx.key(ItemId::Function(*first_id))), format!("the signature of `{first_name}`"));
        diag.fact(Fact::Signature(cx.key(ItemId::Function(*id))), format!("the signature of `{this}`")); // alloc: as above
        found.push(Found::lint(diag, Phase::Late, item.ptr()));
    }
    found
}
