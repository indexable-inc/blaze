//! Match checking (rustc's `check_match`, ported in `hir-ty-matchck`) in `hir`'s terms: the lints
//! of a body's patterns ([`DefWithBody::match_lints`]), whether rustc builds the body's MIR
//! ([`DefWithBody::match_check_failed`]), the errors as [`AnyDiagnostic`]s, and
//! [`Type::is_inhabited_from`].

use base_db::Db;
use hir_def::{
    DefWithBodyId,
    expr_store::{Body, BodySourceMap, ExprOrPatPtr},
    hir::{ExprId, PatId},
    item_scope::ImportId,
    src::use_tree_sources_view,
};
use hir_expand::InFile;
use hir_ty::{
    inhabitedness::is_ty_uninhabited_from,
    next_solver::{DbInterner, EarlyBinder, StoredTy, TypingMode, infer::DbInternerInferExt as _},
};
use hir_ty_matchck::{self as matchck, LetAt, MatchError};
use stdx::never;
use syntax::{
    AstNode, AstPtr, SyntaxNodePtr, TextRange, ast,
    view::{self, AstNode as _, HasName as _, Typed},
};

pub use hir_ty_matchck::{BindingOrigin, SpecialTy};

use crate::{
    Adt, Const, DefWithBody, Enum, EnumVariant, Impl, Module, Type, TypeOwnerId,
    diagnostics::{
        AnyDiagnostic, ConstPatternRefused, EmptyRangePattern, MissingMatchArms, MixedDerefPatterns,
        NonExhaustiveLet, PatternComplexityLimit,
    },
};

/// A lint match checking gives a pattern.
#[derive(Debug, Clone)]
pub enum MatchLint<'db> {
    UnreachablePattern(UnreachablePattern<'db>),
    IrrefutableLet(IrrefutableLet),
    BindingWithVariantName(BindingWithVariantName),
    OverlappingRangeEndpoints(OverlappingRangeEndpoints),
    NonContiguousRangeEndpoints(NonContiguousRangeEndpoints),
}

/// `unreachable_patterns`.
#[derive(Debug, Clone)]
pub struct UnreachablePattern<'db> {
    pub pat: InFile<AstPtr<ast::Pat>>,
    /// The earlier patterns that make it unreachable; none when it matches no value because
    /// its type is uninhabited.
    pub covered_by: Vec<InFile<AstPtr<ast::Pat>>>,
    /// The one pattern covering it matches any value.
    pub catchall: bool,
    /// The pattern's type, for "matches no values because `T` is uninhabited".
    pub ty: Type<'db>,
    /// The match arm the pattern is the whole of, for "remove the match arm".
    pub arm: Option<InFile<AstPtr<ast::MatchArm>>>,
    pub typo: Option<PatternTypo>,
}

/// What rustc's `const_to_pat` refuses in a constant used as a pattern.
#[derive(Debug, Clone)]
pub enum ConstPatternError<'db> {
    /// "cannot use unions in constant patterns".
    Union,
    /// A function pointer, or a raw pointer into an allocation.
    Pointer,
    /// "{prefix} `{ty}` cannot be used in patterns", `ty` being the part of the value refused.
    Invalid(Type<'db>),
    /// The value holds this ADT, which doesn't derive `PartialEq` (`TypeNotStructural`).
    /// `inapplicable_structural`: it has a `StructuralPartialEq` impl whose bounds don't hold,
    /// so rustc says nothing of a manual `PartialEq` impl.
    NotStructural { ty: Type<'db>, inapplicable_structural: bool },
    /// `ty` doesn't implement `PartialEq` (`TypeNotPartialEq`), with what rustc's
    /// `extend_type_not_partial_eq` finds in it, as `hir_ty_matchck::ConstPatternError` lists
    /// them: in the order met, without repeats; rustc sorts the type notes by their text.
    NotPartialEq {
        ty: Type<'db>,
        /// A local ADT with a manual `PartialEq` impl: a span note.
        local_manual: Box<[ManualPartialEq]>,
        /// A local ADT with no `PartialEq` impl: a label at it.
        local_without: Box<[Adt]>,
        /// A foreign type with a manual `PartialEq` impl: a note.
        manual: Box<[Type<'db>]>,
        /// A foreign type with no `PartialEq` impl: a note.
        without: Box<[Type<'db>]>,
    },
    /// "cannot use NaN in patterns".
    NaN,
}

/// Where the note about a local manual `PartialEq` impl points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManualPartialEq {
    /// The impl written for the type.
    Impl(Impl),
    /// The ADT, whose `PartialEq` comes from a blanket or foreign impl.
    Adt(Adt),
}

/// What rustc's `find_fallback_pattern_typo` finds for a catch-all binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternTypo {
    /// A constant in scope the binding may have meant, by its name or a similar one.
    WantedConstant { is_typo: bool, const_name: String, const_path: String },
    /// The name of an import of a constant of the binding's name in another scope.
    AccessibleConstant(InFile<TextRange>),
    /// A constant of the binding's name not accessible from the pattern.
    InaccessibleConstant(Const),
    /// The name of a `let` binding of the binding's name.
    PatternLetBinding(InFile<TextRange>),
}

/// Where an irrefutable `let` is, as `irrefutable_let_patterns` tells them apart: rustc words
/// an `else if let` as it words an `if let`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LetSource {
    IfLet,
    IfLetGuard,
    WhileLet,
    LetElse,
}

impl From<matchck::LetSource> for LetSource {
    fn from(source: matchck::LetSource) -> Self {
        match source {
            matchck::LetSource::IfLet | matchck::LetSource::ElseIfLet => LetSource::IfLet,
            matchck::LetSource::IfLetGuard => LetSource::IfLetGuard,
            matchck::LetSource::WhileLet => LetSource::WhileLet,
            matchck::LetSource::LetElse => LetSource::LetElse,
        }
    }
}

/// `irrefutable_let_patterns`.
#[derive(Debug, Clone)]
pub struct IrrefutableLet {
    pub source: LetSource,
    /// The `let` expression, or the `let` statement of a `let`-`else`.
    pub at: InFile<SyntaxNodePtr>,
    /// A `let`-`else` of `Some(y)` with `y` an `Option` (`check_irrefutable_option_some`): the
    /// pattern and `y`, for "consider using `let Some(..) = y`".
    pub replacement: Option<(InFile<AstPtr<ast::Pat>>, InFile<AstPtr<ast::Expr>>)>,
}

/// `bindings_with_variant_name` (E0170).
#[derive(Debug, Clone)]
pub struct BindingWithVariantName {
    pub pat: InFile<AstPtr<ast::IdentPat>>,
    pub enum_: Enum,
    /// The pattern can be replaced by the variant's path.
    pub suggest: bool,
}

/// `overlapping_range_endpoints`.
#[derive(Debug, Clone)]
pub struct OverlappingRangeEndpoints {
    pub pat: InFile<AstPtr<ast::Pat>>,
    /// The value the ranges share, printed as a pattern.
    pub overlap: String,
    pub overlaps_with: Vec<InFile<AstPtr<ast::Pat>>>,
}

/// `non_contiguous_range_endpoints`.
#[derive(Debug, Clone)]
pub struct NonContiguousRangeEndpoints {
    pub pat: InFile<AstPtr<ast::Pat>>,
    /// The one value no range matches, printed as a pattern.
    pub gap: String,
    /// The range with an inclusive end that would close the gap.
    pub suggestion: String,
    pub gapped_with: Vec<InFile<AstPtr<ast::Pat>>>,
}

impl DefWithBody {
    /// The lints match checking gives the body's patterns (and those of its closures and inline
    /// constants), in the order rustc emits them; none for a body type checking found an error
    /// in.
    pub fn match_lints<'db>(self, db: &'db Db) -> Vec<MatchLint<'db>> {
        let Some(def) = self.id() else { return Vec::new() }; // alloc: empty.
        let check = matchck::match_check(db, def);
        if check.lints.is_empty() {
            return Vec::new(); // alloc: empty.
        }
        let (_, source_map) = Body::with_source_map(db, def);
        let syntax = Syntax { db, source_map };
        // alloc: once per body with a lint, one entry per lint.
        check.lints.iter().filter_map(|lint| syntax.lint(def, lint)).collect()
    }

    /// rustc's `check_match` returns `Err` for the body: match checking refused it, or type
    /// checking found an error in it. rustc then builds no MIR of the body but its error body,
    /// so the body gets no MIR lint.
    pub fn match_check_failed(self, db: &Db) -> bool {
        self.id().is_some_and(|def| {
            let check = matchck::match_check(db, def);
            check.failed || check.not_checked
        })
    }
}

impl<'db> Type<'db> {
    /// rustc's `Ty::is_inhabited_from`: a value of the type can exist as seen from `module`
    /// (an uninhabited field `module` can't see counts as inhabited).
    pub fn is_inhabited_from(&self, db: &'db Db, module: Module) -> bool {
        let env = self.param_env(db);
        let interner = DbInterner::new_with(db, env.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        let ty = self.ty.instantiate_identity().skip_norm_wip();
        !is_ty_uninhabited_from(&infcx, ty, module.id, env.param_env)
    }
}

/// The errors match checking finds in `def`, in the order rustc emits them. `source_map` is
/// asked for only when there is one to place.
pub(crate) fn match_check_diagnostics<'db, 'm>(
    db: &'db Db,
    def: DefWithBodyId,
    source_map: impl FnOnce() -> &'m BodySourceMap,
    type_owner: TypeOwnerId,
    acc: &mut Vec<AnyDiagnostic<'db>>,
) {
    let check = matchck::match_check(db, def);
    if check.errors.is_empty() {
        return;
    }
    let syntax = Syntax { db, source_map: source_map() };
    let interner = DbInterner::new_no_crate(db);
    let ty =
        |ty: &'db StoredTy| Type { owner: type_owner, ty: EarlyBinder::bind(interner, ty.as_ref()) };
    for error in &check.errors {
        match syntax.error(error, &ty) {
            Some(diagnostic) => acc.push(diagnostic),
            None => never!("match checking error in desugared syntax: {error:?}"),
        }
    }
}

/// The witnesses, variants and special types of an E0004 or E0005.
type UncoveredParts<'db> = (Vec<String>, Vec<EnumVariant>, Vec<(Type<'db>, SpecialTy)>);

fn uncovered<'db>(
    uncovered: &'db matchck::Uncovered,
    ty: &impl Fn(&'db StoredTy) -> Type<'db>,
) -> UncoveredParts<'db> {
    // alloc: once per error, the witnesses already printed.
    let witnesses = uncovered.witnesses.iter().map(|it| it.to_string()).collect();
    let variants = uncovered.variants.iter().map(|&id| EnumVariant { id }).collect(); // alloc: as above.
    // alloc: as above.
    let special_tys = uncovered.special_tys.iter().map(|(it, special)| (ty(it), *special)).collect();
    (witnesses, variants, special_tys)
}

/// From a body's ids to their syntax.
struct Syntax<'a, 'db> {
    db: &'db Db,
    source_map: &'a BodySourceMap,
}

impl<'db> Syntax<'_, 'db> {
    fn lint(&self, def: DefWithBodyId, lint: &'db matchck::MatchLint) -> Option<MatchLint<'db>> {
        Some(match lint {
            matchck::MatchLint::UnreachablePattern(it) => {
                let pat = self.pat(it.pat)?;
                MatchLint::UnreachablePattern(UnreachablePattern {
                    pat,
                    covered_by: self.pats(&it.covered_by),
                    catchall: it.catchall,
                    ty: Type::new_body(self.db, def.into(), it.ty.as_ref()),
                    arm: if it.whole_arm { self.parent::<ast::MatchArm>(pat) } else { None },
                    typo: it.typo.as_ref().and_then(|typo| self.typo(typo)),
                })
            }
            matchck::MatchLint::IrrefutableLet(it) => {
                let at = match it.at {
                    LetAt::Expr(expr) => {
                        self.source_map.expr_syntax(expr).ok()?.map(|it| it.syntax_node_ptr())
                    }
                    LetAt::Stmt(pat) => self
                        .parent::<ast::LetStmt>(self.pat(pat)?)?
                        .map(|it| it.syntax_node_ptr()),
                };
                let replacement =
                    it.replacement.and_then(|(pat, expr)| Some((self.pat(pat)?, self.expr(expr)?)));
                MatchLint::IrrefutableLet(IrrefutableLet { source: it.source.into(), at, replacement })
            }
            matchck::MatchLint::BindingWithVariantName(it) => {
                let ptr = self.source_map.pat_syntax(it.pat).ok()?;
                MatchLint::BindingWithVariantName(BindingWithVariantName {
                    pat: InFile::new(ptr.file_id, ptr.value.cast()?),
                    enum_: Enum { id: it.enum_ },
                    suggest: it.suggest,
                })
            }
            matchck::MatchLint::OverlappingRangeEndpoints(it) => {
                MatchLint::OverlappingRangeEndpoints(OverlappingRangeEndpoints {
                    pat: self.pat(it.pat)?,
                    overlap: it.overlap.to_string(), // alloc: once per lint emitted.
                    overlaps_with: self.pats(&it.overlaps_with),
                })
            }
            matchck::MatchLint::NonContiguousRangeEndpoints(it) => {
                MatchLint::NonContiguousRangeEndpoints(NonContiguousRangeEndpoints {
                    pat: self.pat(it.pat)?,
                    gap: it.gap.to_string(), // alloc: once per lint emitted.
                    suggestion: it.suggestion.to_string(), // alloc: as above.
                    gapped_with: self.pats(&it.gapped_with),
                })
            }
        })
    }

    fn error(
        &self,
        error: &'db MatchError,
        ty: &impl Fn(&'db StoredTy) -> Type<'db>,
    ) -> Option<AnyDiagnostic<'db>> {
        Some(match error {
            MatchError::MissingMatchArms {
                match_expr: _,
                scrutinee,
                scrutinee_ty,
                no_arms,
                uncovered: it,
                would_be_exhaustive_without_guards,
                ref_to_uninhabited,
                consts_like_bindings,
            } => {
                let (witnesses, variants, special_tys) = uncovered(it, ty);
                MissingMatchArms {
                    scrutinee_expr: self.expr(*scrutinee)?,
                    scrutinee_ty: ty(scrutinee_ty),
                    no_arms: *no_arms,
                    witnesses,
                    variants,
                    special_tys,
                    would_be_exhaustive_without_guards: *would_be_exhaustive_without_guards,
                    ref_to_uninhabited: *ref_to_uninhabited,
                    consts_like_bindings: consts_like_bindings
                        .iter()
                        .filter_map(|&(pat, id)| Some((self.pat(pat)?, Const { id })))
                        .collect(), // alloc: once per error.
                }
                .into()
            }
            MatchError::NonExhaustiveLet {
                pat,
                origin,
                has_initializer,
                pattern_ty,
                uncovered: it,
                privately_uninhabited,
                interpreted_as_const,
            } => {
                let (witnesses, variants, special_tys) = uncovered(it, ty);
                NonExhaustiveLet {
                    pat: self.pat_or_expr(*pat)?,
                    origin: *origin,
                    has_initializer: *has_initializer,
                    pattern_ty: ty(pattern_ty),
                    witnesses,
                    variants,
                    special_tys,
                    privately_uninhabited: *privately_uninhabited,
                    interpreted_as_const: interpreted_as_const.map(|id| Const { id }),
                }
                .into()
            }
            &MatchError::EmptyRange { pat, inclusive } => {
                EmptyRangePattern { pat: self.pat(pat)?, inclusive }.into()
            }
            &MatchError::ComplexityLimit { at } => PatternComplexityLimit {
                at: self.source_map.expr_or_pat_syntax(at).ok()?.map(|it| it.syntax_node_ptr()),
            }
            .into(),
            MatchError::ConstPattern { pat, error } => ConstPatternRefused {
                pat: self.pat(*pat)?,
                error: match error {
                    matchck::ConstPatternError::Union => ConstPatternError::Union,
                    matchck::ConstPatternError::Pointer => ConstPatternError::Pointer,
                    matchck::ConstPatternError::Invalid(it) => ConstPatternError::Invalid(ty(it)),
                    matchck::ConstPatternError::NotStructural { ty: it, inapplicable_structural } => {
                        ConstPatternError::NotStructural {
                            ty: ty(it),
                            inapplicable_structural: *inapplicable_structural,
                        }
                    }
                    matchck::ConstPatternError::NotPartialEq {
                        ty: it,
                        local_manual,
                        local_without,
                        manual,
                        without,
                    } => ConstPatternError::NotPartialEq {
                        ty: ty(it),
                        local_manual: local_manual
                            .iter()
                            .map(|&it| match it {
                                matchck::ManualPartialEq::Impl(id) => ManualPartialEq::Impl(id.into()),
                                matchck::ManualPartialEq::Adt(id) => ManualPartialEq::Adt(id.into()),
                            })
                            // alloc: the notes of an error that fired, once each.
                            .collect(),
                        // alloc: as above.
                        local_without: local_without.iter().map(|&id| Adt::from(id)).collect(),
                        manual: manual.iter().map(|it| ty(it)).collect(), // alloc: as above.
                        without: without.iter().map(|it| ty(it)).collect(), // alloc: as above.
                    },
                    matchck::ConstPatternError::NaN => ConstPatternError::NaN,
                },
            }
            .into(),
            &MatchError::MixedDerefPatterns { deref_pat, normal_pat } => MixedDerefPatterns {
                deref_pat: self.pat(deref_pat)?,
                normal_pat: self.pat(normal_pat)?,
            }
            .into(),
        })
    }

    fn pat(&self, pat: PatId) -> Option<InFile<AstPtr<ast::Pat>>> {
        let ptr = self.source_map.pat_syntax(pat).ok()?;
        Some(InFile::new(ptr.file_id, ptr.value.cast()?))
    }

    /// A pattern of a destructuring assignment is an expression.
    fn pat_or_expr(&self, pat: PatId) -> Option<InFile<ExprOrPatPtr>> {
        self.source_map.pat_syntax(pat).ok()
    }

    fn pats(&self, pats: &[PatId]) -> Vec<InFile<AstPtr<ast::Pat>>> {
        // alloc: once per lint emitted.
        pats.iter().filter_map(|&pat| self.pat(pat)).collect()
    }

    fn expr(&self, expr: ExprId) -> Option<InFile<AstPtr<ast::Expr>>> {
        let ptr = self.source_map.expr_syntax(expr).ok()?;
        Some(InFile::new(ptr.file_id, ptr.value.cast()?))
    }

    /// The node a pattern is the whole pattern of: a match arm, a `let` statement.
    fn parent<N: AstNode + Typed>(
        &self,
        pat: InFile<AstPtr<ast::Pat>>,
    ) -> Option<InFile<AstPtr<N>>> {
        let parent = pat.to_view(self.db).syntax().parent()?;
        let parent = <N::Of<'_> as view::AstNode<'_>>::cast(parent)?;
        Some(InFile::new(pat.file_id, AstPtr::of(parent)))
    }

    fn typo(&self, typo: &matchck::PatternTypo) -> Option<PatternTypo> {
        Some(match typo {
            matchck::PatternTypo::WantedConstant { is_typo, const_name, const_path } => {
                PatternTypo::WantedConstant {
                    is_typo: *is_typo,
                    const_name: const_name.to_string(), // alloc: once per lint emitted.
                    const_path: const_path.to_string(), // alloc: as above.
                }
            }
            &matchck::PatternTypo::AccessibleConstant(import) => {
                PatternTypo::AccessibleConstant(self.import_name(import)?)
            }
            &matchck::PatternTypo::InaccessibleConstant(id) => {
                PatternTypo::InaccessibleConstant(Const { id })
            }
            &matchck::PatternTypo::PatternLetBinding(pat) => {
                let pat = self.pat(pat)?;
                let view::Pat::IdentPat(ident) = pat.to_view(self.db) else { return None };
                PatternTypo::PatternLetBinding(InFile::new(
                    pat.file_id,
                    ident.name()?.syntax().text_range(),
                ))
            }
        })
    }

    /// The span rustc gives an import's ident: its rename, else its path's last segment.
    fn import_name(&self, import: ImportId) -> Option<InFile<TextRange>> {
        let source = use_tree_sources_view(self.db, import.use_);
        let tree = source.value.get(import.idx)?;
        let range = match tree.rename() {
            Some(rename) => rename.name()?.syntax().text_range(),
            None => tree.path()?.segment()?.syntax().text_range(),
        };
        Some(InFile::new(source.file_id, range))
    }
}
