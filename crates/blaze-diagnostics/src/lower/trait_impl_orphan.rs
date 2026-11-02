//! E0117 and E0210: an impl of a trait of another crate that the orphan rules refuse (hir's
//! `TraitImplOrphan`), told apart and labelled as rustc does by walking the impl's input types
//! as rustc's orphan checker does; the walk is also what `checks::impls` refuses with E0210
//! where hir's `check_orphan_rules` takes an uncovered type parameter for covered.
//!
//! Follows rustc's `compiler/rustc_next_trait_solver/src/coherence.rs`
//! (`orphan_check_trait_ref`, `OrphanChecker`), `compiler/rustc_hir_analysis/src/coherence/
//! orphan.rs` (`orphan_check_impl`, `orphan_check`, `emit_orphan_check_error`) and the
//! structs `OnlyCurrentTraits*` and `UncoveredTyParam` of `compiler/rustc_hir_analysis/src/
//! diagnostics.rs` (MIT or Apache-2.0).

use std::ops::ControlFlow;

use blaze_items::{Fact, ItemId};
use hir::InFile;
use hir_def::{
    AdtId,
    signatures::{StructFlags, StructSignature},
};
use syntax::view::{self as ast, AstNode as _, HasName as _};

use crate::{
    cx::Cx,
    lower::{Found, Phase, incoherent_impl::{ImplAt, unresolved}},
    model::{Applicability, Diag, Span},
};

/// rustc's `OrphanCheckMode`: whether a projection with a type parameter in it is taken for
/// an uncovered parameter (`Proper`), or passed over (`Compat`, which only decides whether the
/// error is the `uncovered_param_in_projection` lint's).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Proper,
    Compat,
}

/// rustc's `OrphanCheckErr`, with the impl's own parameters for its inference variables.
pub(crate) enum OrphanError<'db> {
    /// No input type is local: those that are not, each with whether it is (in) the self type.
    NonLocal(Vec<(hir::Type<'db>, bool)>),
    /// A type parameter not covered by a type appears before the first local type, if any.
    Uncovered { params: Vec<hir::TypeParam>, local: Option<hir::Type<'db>> },
}

/// rustc's orphan check of a trait impl: `Ok` when the trait is local or a local type comes
/// before any uncovered type parameter among the impl's input types.
pub(crate) fn orphan_check<'db>(
    cx: &Cx<'_, 'db>,
    impl_: hir::Impl,
    mode: Mode,
) -> Result<(), OrphanError<'db>> {
    let Some(trait_ref) = impl_.trait_ref(cx.db) else { return Ok(()) };
    let trait_ = trait_ref.trait_();
    if trait_.module(cx.db).krate(cx.db) == cx.krate {
        return Ok(());
    }
    // `Self`, then the trait's lifetimes (which are not types), then its types and consts.
    let len = 2 + hir::GenericDef::Trait(trait_).params(cx.db).len();
    let inputs: Vec<hir::Type<'db>> =
        // alloc: the error's parts, for an error hir found
        (0..len).filter_map(|index| trait_ref.get_type_argument(index)).collect();
    // alloc: the error's parts, for an error hir found
    let mut checker = Checker { cx, mode, in_self_ty: true, search_first_local: false, non_local: Vec::new() };
    let mut found = ControlFlow::Continue(());
    for ty in &inputs {
        found = checker.visit(ty);
        if found.is_break() {
            break;
        }
    }
    match found {
        ControlFlow::Continue(()) => Err(OrphanError::NonLocal(checker.non_local)),
        ControlFlow::Break(Exit::Local(_)) => Ok(()),
        ControlFlow::Break(Exit::Uncovered(uncovered)) => {
            checker.search_first_local = true;
            checker.in_self_ty = true;
            let mut local = None;
            for ty in &inputs {
                if let ControlFlow::Break(Exit::Local(ty)) = checker.visit(ty) {
                    local = Some(ty);
                    break;
                }
            }
            // alloc: the error's parts, for an error hir found
            let mut params = Vec::new();
            uncovered.walk(cx.db, |ty| {
                if let Some(param) = ty.as_type_param(cx.db)
                    && !params.contains(&param)
                {
                    params.push(param);
                }
            });
            Err(OrphanError::Uncovered { params, local })
        }
    }
}

enum Exit<'db> {
    Local(hir::Type<'db>),
    Uncovered(hir::Type<'db>),
}

struct Checker<'a, 'b, 'db> {
    cx: &'a Cx<'b, 'db>,
    mode: Mode,
    in_self_ty: bool,
    search_first_local: bool,
    non_local: Vec<(hir::Type<'db>, bool)>,
}

impl<'db> Checker<'_, '_, 'db> {
    fn is_local(&self, krate: hir::Crate) -> bool {
        krate == self.cx.krate
    }

    fn non_local(&mut self, ty: &hir::Type<'db>) -> ControlFlow<Exit<'db>> {
        // clone: on the error path, for an error hir found
        self.non_local.push((ty.clone(), self.in_self_ty));
        ControlFlow::Continue(())
    }

    fn uncovered(&mut self, ty: &hir::Type<'db>) -> ControlFlow<Exit<'db>> {
        if self.search_first_local {
            return ControlFlow::Continue(());
        }
        // clone: on the error path, for an error hir found
        ControlFlow::Break(Exit::Uncovered(ty.clone()))
    }

    /// rustc's `OrphanChecker::visit_ty`, the impl's parameters standing for its inference
    /// variables: a parameter is uncovered, a fundamental type (`&T`, `Box<T>`, `Pin<T>`) is
    /// looked into, any other type is local or not by where it is defined.
    fn visit(&mut self, ty: &hir::Type<'db>) -> ControlFlow<Exit<'db>> {
        let db = self.cx.db;
        let result = if ty.is_unknown() {
            // clone: on the error path, for an error hir found
            ControlFlow::Break(Exit::Local(ty.clone()))
        } else if ty.as_type_param(db).is_some() {
            self.uncovered(ty)
        } else if ty.as_associated_type_parent_trait(db).is_some() || ty.as_impl_traits(db).is_some() {
            let projection = ty.as_associated_type_parent_trait(db).is_some();
            if ty.generic_params(db).iter().any(|it| matches!(it, hir::GenericParam::TypeParam(_))) {
                match (projection, self.mode) {
                    (true, Mode::Compat) => ControlFlow::Continue(()),
                    _ => self.uncovered(ty),
                }
            } else {
                self.non_local(ty)
            }
        } else if let Some((inner, _)) = ty.as_reference() {
            self.visit(&inner)
        } else if let Some(adt) = ty.as_adt() {
            if self.is_local(adt.module(db).krate(db)) {
                // clone: on the error path, for an error hir found
                ControlFlow::Break(Exit::Local(ty.clone()))
            } else if is_fundamental(self.cx, adt) {
                // alloc: the error's parts, for an error hir found
                let args: Vec<_> = ty.type_arguments().collect();
                let mut result = ControlFlow::Continue(());
                for arg in &args {
                    result = self.visit(arg);
                    if result.is_break() {
                        break;
                    }
                }
                result
            } else {
                self.non_local(ty)
            }
        } else if let Some(trait_) = ty.as_dyn_trait() {
            if self.is_local(trait_.module(db).krate(db)) {
                // clone: on the error path, for an error hir found
                ControlFlow::Break(Exit::Local(ty.clone()))
            } else {
                self.non_local(ty)
            }
        } else {
            self.non_local(ty)
        };
        self.in_self_ty = false;
        result
    }
}

/// Whether `adt` is `#[fundamental]` (`Box`, `Pin`): its type argument decides for it.
fn is_fundamental(cx: &Cx<'_, '_>, adt: hir::Adt) -> bool {
    match AdtId::from(adt) {
        AdtId::StructId(id) => StructSignature::of(cx.db, id).flags.contains(StructFlags::FUNDAMENTAL),
        _ => false,
    }
}

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::TraitImplOrphan) -> Vec<Found> {
    let Some(at) = ImplAt::of_ptr(cx, d.file_id, d.impl_) else {
        return unresolved(cx, d.file_id, d.impl_, "an orphan impl error");
    };
    let found = report(cx, &at);
    if found.iter().any(|it| it.diag.is_error()) {
        return found;
    }
    // hir refuses what the walk takes: refused all the same, in blaze's words (no-fallback).
    let mut diag = Diag::error(
        Some("E0117"),
        "only traits defined in the current crate can be implemented for types defined outside of the crate",
        at.span(cx),
    );
    diag.note("blaze's orphan check refuses this impl, though rustc's finds a local type before any uncovered type parameter");
    diag.fact(at.coherence(cx), "the orphan check of the impl");
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Items)]
}

/// rustc's `orphan_check_impl`: the `Proper` check, and when it fails the `Compat` one, whose
/// error is reported; a failure only `Proper` finds is the `uncovered_param_in_projection`
/// lint's.
pub(crate) fn report<'db>(cx: &Cx<'_, 'db>, at: &ImplAt<'db>) -> Vec<Found> {
    // alloc: empty, allocates nothing
    let Err(proper) = orphan_check(cx, at.impl_, Mode::Proper) else { return Vec::new() };
    match orphan_check(cx, at.impl_, Mode::Compat) {
        Ok(()) => match proper {
            OrphanError::Uncovered { params, local } => uncovered(cx, at, &params, local.as_ref(), true),
            // alloc: empty, allocates nothing
            OrphanError::NonLocal(_) => Vec::new(),
        },
        // alloc: the error's parts, for an error hir found
        Err(OrphanError::NonLocal(tys)) => vec![Found::new(non_local(cx, at, &tys), Phase::Items)],
        Err(OrphanError::Uncovered { params, local }) => uncovered(cx, at, &params, local.as_ref(), false),
    }
}

/// E0117: `OnlyCurrentTraits`, with a label on each input type that is not local.
pub(crate) fn non_local<'db>(cx: &Cx<'_, 'db>, at: &ImplAt<'db>, tys: &[(hir::Type<'db>, bool)]) -> Diag {
    let db = cx.db;
    let span = at.span(cx);
    let self_ty = at.impl_.self_ty(db);
    let message = if self_ty.as_adt().is_some() {
        "only traits defined in the current crate can be implemented for types defined outside of the crate"
    } else if self_ty.as_builtin().is_some() && !self_ty.is_str() {
        "only traits defined in the current crate can be implemented for primitive types"
    } else {
        "only traits defined in the current crate can be implemented for arbitrary types"
    };
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(Some("E0117"), message, span.clone());
    let self_span = at.self_ty_span(cx);
    let trait_span = at.trait_span(cx);
    let self_has_param = self_ty.generic_params(db).iter().any(|it| {
        matches!(it, hir::GenericParam::TypeParam(_) | hir::GenericParam::ConstParam(_))
    });
    let mut wrapper = None;
    for (ty, is_self) in tys {
        // clone: on the error path, for an error hir found
        let at_span = if *is_self { self_span.clone() } else { trait_span.clone() };
        // The trait is always another crate's here, so a type not of the self type is foreign.
        let foreign = !is_self;
        let always_foreign = |name: &str| match foreign {
            // alloc: the error's text, for an error hir found
            true => "this is not defined in the current crate because this is a foreign trait".to_owned(),
            // alloc: the error's text, for an error hir found
            false => format!("this is not defined in the current crate because {name} are always foreign"),
        };
        let label = if ty.is_slice() {
            always_foreign("slices")
        } else if ty.is_array() {
            always_foreign("arrays")
        } else if ty.is_tuple() {
            always_foreign("tuples")
        } else if ty.as_impl_traits(db).is_some() {
            // alloc: the error's text, for an error hir found
            "type alias impl trait is treated as if it were foreign, because its hidden type could be from a foreign crate".to_owned()
        } else if let Some((pointee, mutability)) = ty.as_raw_ptr() {
            if !self_has_param {
                let mut_key = match mutability {
                    hir::Mutability::Mut => "mut ",
                    hir::Mutability::Shared => "",
                };
                // alloc: the error's text, for an error hir found
                wrapper = Some(format!("struct WrapperType(*{mut_key}{});\n\n", cx.ty(&pointee)));
            }
            // alloc: the error's text, for an error hir found
            format!("`{}` is not defined in the current crate because raw pointers are always foreign", cx.ty(ty))
        } else if let Some(adt) = ty.as_adt() {
            // alloc: the error's text, for an error hir found
            format!("`{}` is not defined in the current crate", cx.def_path(hir::ModuleDef::Adt(adt)))
        } else {
            // alloc: the error's text, for an error hir found
            format!("`{}` is not defined in the current crate", cx.ty(ty))
        };
        diag.span_label(at_span, label);
        if let Some(adt) = ty.as_adt() {
            // alloc: the error's text, for an error hir found
            diag.fact(Fact::Signature(cx.key(ItemId::Adt(adt.into()))), format!("where `{}` is defined", cx.ty(ty)));
        }
    }
    diag.note("impl doesn't have any local type before any uncovered type parameters");
    diag.note("for more information see https://doc.rust-lang.org/reference/items/implementations.html#orphan-rules");
    diag.note("define and implement a trait or new type instead");
    if let Some(code) = wrapper {
        diag.multipart_suggestion(
            "consider introducing a new wrapper type",
            // alloc: the error's text, for an error hir found
            vec![(self_span, "WrapperType".to_owned()), (span.shrink_to_lo(), code)],
            Applicability::MaybeIncorrect,
        );
    }
    diag.fact(at.coherence(cx), "the orphan check of the impl");
    if let Some(trait_) = at.impl_.trait_(db) {
        diag.fact(Fact::Signature(cx.key(ItemId::Trait(trait_.into()))), "where the trait is defined");
    }
    diag
}

/// E0210, one for each uncovered parameter (`UncoveredTyParam`); the lint
/// `uncovered_param_in_projection` instead when only a projection leaves it uncovered.
pub(crate) fn uncovered<'db>(
    cx: &Cx<'_, 'db>,
    at: &ImplAt<'db>,
    params: &[hir::TypeParam],
    local: Option<&hir::Type<'db>>,
    lint: bool,
) -> Vec<Found> {
    let note = "implementing a foreign trait is only possible if at least one of the types for which it is implemented is local";
    params
        .iter()
        .map(|&param| {
            let name = param.name(cx.db);
            let name = name.as_str();
            let span = param_span(cx, param).unwrap_or_else(|| at.span(cx));
            let message = match local {
                // alloc: the error's text, for an error hir found
                Some(local) => format!(
                    "type parameter `{name}` must be covered by another type when it appears before the first local type (`{}`)",
                    cx.ty(local)
                ),
                // alloc: the error's text, for an error hir found
                None => format!(
                    "type parameter `{name}` must be used as an argument to some local type (e.g., `MyStruct<{name}>`)"
                ),
            };
            let mut diag = match lint {
                // clone: on the error path, for an error hir found
                true => Diag::lint("uncovered_param_in_projection", message, span.clone()),
                // clone: on the error path, for an error hir found
                false => Diag::error(Some("E0210"), message, span.clone()),
            };
            diag.span_label(span, "uncovered type parameter");
            match local {
                Some(_) => {
                    // alloc: the error's text, for an error hir found
                    diag.note(format!("{note},\nand no uncovered type parameters appear before that first local type"));
                    diag.note("in this case, 'before' refers to the following order: `impl<..> ForeignTrait<T1, ..., Tn> for T0`,\nwhere `T0` is the first and `Tn` is the last");
                }
                None => {
                    diag.note(note);
                    diag.note("only traits defined in the current crate can be implemented for a type parameter");
                }
            }
            diag.fact(at.coherence(cx), "the orphan check of the impl");
            match lint {
                true => {
                    let node = at.node.map(|it| ast::ptr(it.syntax()));
                    Found::lint(diag, Phase::Items, node)
                }
                false => Found::new(diag, Phase::Items),
            }
        })
        // alloc: the error's parts, for an error hir found
        .collect()
}

/// The span of a type parameter's name where it is declared.
pub(crate) fn param_span(cx: &Cx<'_, '_>, param: hir::TypeParam) -> Option<Span> {
    let source = cx.vsema.source(param.merge())?;
    let name = match source.value {
        either::Either::Left(ast::TypeOrConstParam::Type(it)) => it.name()?,
        either::Either::Left(ast::TypeOrConstParam::Const(it)) => it.name()?,
        either::Either::Right(_) => return None,
    };
    Some(cx.span(InFile::new(source.file_id, name.syntax().text_range())))
}
