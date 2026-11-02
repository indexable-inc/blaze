//! Re-export diagnostics such that clients of `hir` don't have to depend on
//! low-level crates.
//!
//! This probably isn't the best way to do this -- ideally, diagnostics should
//! be expressed in terms of hir types themselves.
use std::mem::discriminant;

use base_db::Db;
use cfg::{CfgExpr, CfgOptions};
use either::Either;
use hir_def::{
    AdtId, AssocItemId, DefWithBodyId, EnumId, EnumVariantId, FunctionId, GenericDefId,
    GenericParamId, ImplId, ItemContainerId, Lookup, MacroId, ModuleDefId, ModuleId, SyntheticSyntax, TraitId,
    attrs::AttrFlags,
    expr_store::{
        Body, ExprOrPatPtr, ExpressionStore, ExpressionStoreDiagnostics, ExpressionStoreSourceMap,
        lower::{
            hir_assoc_type_binding_to_ast_view, hir_generic_arg_to_ast_view,
            hir_segment_to_ast_segment_view,
        },
    },
    hir::{ExprId, ExprOrPatId, PatId},
    nameres::{
        DefMap,
        assoc::{ImplItems, TraitItems},
        diagnostics::{DefDiagnosticKind, DefDiagnostics},
    },
    signatures::{
        ConstSignature, FunctionSignature, ImplFlags, ImplSignature, TraitFlags, TraitSignature,
        TypeAliasSignature,
    },
    src::HasChildSourceView as _,
    type_ref::TypeRefId,
    unstable_features::UnstableFeatures,
};
use hir_expand::{
    ExpandErrorKind, HirFileId, InFile, MacroCallId, MacroCallKind, MacroKind, RenderedExpandError,
    ValueResult, mod_path::ModPath, name::Name,
};
use hir_ty::{
    CastError, ElidedLifetimeSite, ExplicitDropMethodUseKind, InferBodyId, InferenceDiagnostic,
    InferenceResult, ParamEnvAndCrate, PathGenericsSource, PathLoweringDiagnostic,
    TyLoweringDiagnostic,
    check_orphan_rules,
    db::{AnonConstId, HirDatabase, signature_anon_consts_and_diagnostics},
    diagnostics::BodyValidationDiagnostic,
    display::{DisplayTarget, HirDisplay},
    method_resolution::TraitImpls,
    next_solver::{
        AnyImplId, DbInterner, EarlyBinder, StoredTy, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
    solver_errors::SolverDiagnosticKind,
    solver_errors::{self, StoredGoal},
    traits::{is_inherent_impl_coherent, structurally_normalize_ty},
};
use rustc_type_ir::inherent::IntoKind as _;
use span::Edition;
use stdx::{impl_from, never};
use syntax::{
    AstNode, AstPtr, SyntaxError, SyntaxNodePtr, TextRange, ast, match_ast,
    view::{self, AstNode as _, HasGenericArgs as _, HasName as _},
};
use triomphe::Arc;

use crate::{
    AnyFunctionId, AssocItem, Const, EnumVariant, Field, Function, GenericDef, GenericParam, Trait,
    Type, TypeAlias, TypeOwnerId, TypeParam, Variant,
    match_lints::{BindingOrigin, ConstPatternError, SpecialTy},
    struct_tail_raw,
};

pub use hir_def::{VariantId, expr_store::MissingBodyItemKind, hir::UnsafeBinderCastKind};
pub use hir_expand::BuiltinChild;
pub use hir_ty::{
    AssocTypeFailure, GenericArgsProhibitedReason, IncorrectGenericsLenKind, MissingLifetimeKind,
    PatternTypeUnsupported, ReturnKind, StructPathDef, StructPathFound,
    const_param_ty::ConstParamTyReason,
    consteval::CreateConstError,
    diagnostics::{CaseType, IdentType, IncorrectCase},
};
pub use hir_ty::solver_errors::{TypeAnnotationNeeded, UnreportedSolverError};

#[derive(Debug, Clone)]
pub enum SpanAst {
    Expr(ast::Expr),
    Pat(ast::Pat),
    Type(ast::Type),
}
const _: () = {
    use syntax::ast::*;
    impl_from!(Expr, Pat, Type for SpanAst);
};

impl From<Either<ast::Expr, ast::Pat>> for SpanAst {
    fn from(value: Either<ast::Expr, ast::Pat>) -> Self {
        match value {
            Either::Left(it) => it.into(),
            Either::Right(it) => it.into(),
        }
    }
}

impl ast::AstNode for SpanAst {
    fn can_cast(kind: syntax::SyntaxKind) -> bool {
        ast::Expr::can_cast(kind) || ast::Pat::can_cast(kind) || ast::Type::can_cast(kind)
    }

    fn cast(syntax: syntax::SyntaxNode) -> Option<Self> {
        let kind = syntax.kind();
        if ast::Expr::can_cast(kind) {
            ast::Expr::cast(syntax).map(SpanAst::Expr)
        } else if ast::Pat::can_cast(kind) {
            ast::Pat::cast(syntax).map(SpanAst::Pat)
        } else {
            ast::Type::cast(syntax).map(SpanAst::Type)
        }
    }

    fn syntax(&self) -> &syntax::SyntaxNode {
        match self {
            SpanAst::Expr(it) => it.syntax(),
            SpanAst::Pat(it) => it.syntax(),
            SpanAst::Type(it) => it.syntax(),
        }
    }
}

pub type SpanSyntax = InFile<AstPtr<SpanAst>>;

macro_rules! diagnostics {
    ($AnyDiagnostic:ident <$db:lifetime> -> $($diag:ident $(<$lt:lifetime>)?,)*) => {
        #[derive(Debug)]
        pub enum $AnyDiagnostic<$db> {$(
            $diag(Box<$diag $(<$lt>)?>),
        )*}

        $(
            impl<$db> From<$diag $(<$lt>)?> for $AnyDiagnostic<$db> {
                fn from(d: $diag $(<$lt>)?) -> $AnyDiagnostic<$db> {
                    // alloc: error path, one box per reported diagnostic
                    $AnyDiagnostic::$diag(Box::new(d))
                }
            }
        )*
    };
}

diagnostics![AnyDiagnostic<'db> ->
    AmbiguousAssociatedItem<'db>,
    ArrayPatternWithoutFixedLength,
    AwaitOutsideOfAsync,
    IntLiteralTooLarge,
    BorrowckError,
    BorrowckSkipped,
    BreakOutsideOfLoop,
    UnlabeledJumpInLabeledBlock,
    AsyncCoroutinesNotSupported,
    ConstCheckError,
    ConstEvalFailed,
    ConstImpure,
    CannotBeDereferenced<'db>,
    UnaryOperatorCannotBeApplied<'db>,
    BinaryOperatorCannotBeApplied<'db>,
    CannotImplicitlyDerefTraitObject<'db>,
    CannotIndexInto<'db>,
    CastToUnsized<'db>,
    EditionDependentMethodCall,
    ExpectedArrayOrSlicePat<'db>,
    ExpectedFunction<'db>,
    ExplicitDropMethodUse,
    FruInDestructuringAssignment,
    FullyQualifiedCallFrom2018<'db>,
    MissingBody,
    FunctionalRecordUpdateOnNonStruct,
    GenericDefaultRefersToSelf,
    AssocTypeNotResolved,
    AmbiguousAssocType,
    ImplTraitNotAllowed,
    AdditionalNonAutoTraits,
    ConflictingAssocTypeBindings,
    MissingSelfDefaultParams,
    ObjectWithoutTrait,
    PatternTypeNotLowered,
    InactiveCode,
    IncoherentImpl,
    IncorrectCase,
    IncorrectGenericsLen,
    IncorrectGenericsOrder,
    InferVarsNotAllowed,
    InvalidCast<'db>,
    InvalidDeriveTarget,
    InvalidVisibility,
    InvalidLhsOfAssignment,
    InvalidRangePatType<'db>,
    LateBoundLifetimeArgs,
    MacroDefError,
    MacroError,
    MacroExpansionParseError,
    MalformedDerive,
    MethodCallIllegalSizedBound,
    MismatchedArgCount,
    IncorrectArguments,
    MismatchedTupleStructPatArgCount,
    MissingFields,
    MissingMatchArms<'db>,
    MissingUnsafe,
    MutRefInImmRefPat,
    MutableRefBinding,
    NonExhaustiveLet<'db>,
    EmptyRangePattern,
    PatternComplexityLimit,
    MixedDerefPatterns,
    ConstPatternRefused<'db>,
    NonExhaustiveRecordExpr,
    NonExhaustiveRecordPat,
    NoSuchField,
    MismatchedArrayPatLen,
    DuplicateField,
    PatternArgInExternFn,
    MissingPrelude,
    PrivateAssocItem,
    PrivateField,
    RemoveTrailingReturn,
    RemoveUnnecessaryElse,
    UnusedMustUse<'db>,
    ReplaceFilterMapNextWithFindMap,
    TraitImplIncorrectSafety,
    TraitImplMissingAssocItems,
    TraitImplOrphan,
    TraitImplRedundantAssocItems,
    TraitImplUnstableDefaultBody,
    TraitItemImport,
    TypedHole<'db>,
    TypeMismatch<'db>,
    UndeclaredLabel,
    UnimplementedBuiltinMacro,
    UnreachableLabel,
    UnresolvedAssocItem<'db>,
    UnresolvedExternCrate,
    UnresolvedField<'db>,
    UnresolvedImport,
    UnresolvedMacroCall,
    UnresolvedMethodCall<'db>,
    UnresolvedModule,
    UnresolvedIdent,
    UnresolvedPath,
    GenericArgsProhibited,
    ParenthesizedGenericArgsWithoutFnTrait,
    BadRtn,
    MissingLifetime,
    MissingLifetimes,
    ElidedLifetimesInPath,
    TypeMustBeKnown<'db>,
    UnionExprMustHaveExactlyOneField,
    UnionPatMustHaveExactlyOneField,
    UnionPatHasRest,
    UnimplementedTrait<'db>,
    ClosureKindMismatch,
    TraitSolverError<'db>,
    YieldOutsideCoroutine,
    ReturnOutsideFunction,
    UnsafeBinderCastOfNonBinder<'db>,
    InvalidConstArg,
    NotAStructPath<'db>,
    AutoderefRecursionLimit<'db>,
    MethodOnUnknownRawPointee,
    TyvarBehindRawPointer,
    InheritedRefMutabilityMismatch,
    FnTraitArgsNotTuple,
    SelfCtorNotTupleOrUnit<'db>,
    ConstParamTyNotAllowed<'db>,
    ValTreeError<'db>,
    UnplacedDiagnostic,
];

#[derive(Debug)]
pub struct BreakOutsideOfLoop {
    pub expr: InFile<ExprOrPatPtr>,
    pub is_break: bool,
    pub bad_value_break: bool,
}

/// E0695: an unlabeled `break` (`is_break`) or `continue` inside a labeled block.
#[derive(Debug)]
pub struct UnlabeledJumpInLabeledBlock {
    pub expr: InFile<ExprOrPatPtr>,
    pub is_break: bool,
}

/// E0727: a `yield` in an `async` body.
#[derive(Debug)]
pub struct AsyncCoroutinesNotSupported {
    pub expr: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct TypedHole<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub expected: Type<'db>,
}

#[derive(Debug)]
pub struct UnresolvedModule {
    pub decl: InFile<AstPtr<ast::Module>>,
    pub candidates: Box<[String]>,
}

#[derive(Debug)]
pub struct UnresolvedExternCrate {
    pub decl: InFile<AstPtr<ast::ExternCrate>>,
}

#[derive(Debug)]
pub struct UnresolvedImport {
    pub decl: InFile<AstPtr<ast::UseTree>>,
}

/// An import of a trait's associated items that rustc reports (E0658 without
/// `import_trait_associated_functions`), or whose items blaze cannot list.
#[derive(Debug)]
pub struct TraitItemImport {
    pub decl: InFile<AstPtr<ast::UseTree>>,
    pub why: hir_def::nameres::diagnostics::TraitItemImport,
}

/// The crate's injected `extern crate std;` (`core` under `#![no_std]`) names no dependency, or
/// that crate has no `prelude::rust_20xx`: rustc's E0463 or E0433, with no span.
#[derive(Debug)]
pub struct MissingPrelude {
    pub krate: Name,
    pub error: hir_def::nameres::diagnostics::PreludeError,
}

/// A `pub(in path)` or `pub(super)` whose path names no module above the item, as rustc's
/// `try_resolve_visibility` reports it; the item is `pub` after it.
#[derive(Debug)]
pub struct InvalidVisibility {
    pub node: InFile<AstPtr<ast::Visibility>>,
    pub error: hir_def::visibility::VisibilityError,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UnresolvedMacroCall {
    pub range: InFile<TextRange>,
    pub path: ModPath,
    pub is_bang: bool,
}
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UnreachableLabel {
    pub node: InFile<AstPtr<ast::Lifetime>>,
    pub name: Name,
}

#[derive(Debug)]
pub struct AwaitOutsideOfAsync {
    pub node: InFile<AstPtr<ast::AwaitExpr>>,
    pub location: String,
}

/// An integer literal whose digits do not fit `u128`: rustc's `LitError::IntTooLarge`.
#[derive(Debug)]
pub struct IntLiteralTooLarge {
    pub node: InFile<AstPtr<ast::Literal>>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UndeclaredLabel {
    pub node: InFile<AstPtr<ast::Lifetime>>,
    pub name: Name,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct InactiveCode {
    pub node: InFile<SyntaxNodePtr>,
    pub cfg: CfgExpr,
    pub opts: CfgOptions,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct MacroError {
    pub range: InFile<TextRange>,
    pub message: String,
    pub error: bool,
    pub kind: &'static str,
    /// A builtin error's code: 802 for E0802.
    pub code: Option<u16>,
    /// A builtin error's primary spans after `range`.
    pub also_at: Box<[InFile<TextRange>]>,
    /// A builtin error's notes and helps, in rustc's order.
    pub children: Box<[(BuiltinChild, Box<str>)]>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct MacroExpansionParseError {
    pub range: InFile<TextRange>,
    pub errors: Arc<[SyntaxError]>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct MacroDefError {
    pub node: InFile<AstPtr<ast::Macro>>,
    pub message: String,
    pub name: Option<TextRange>,
}

#[derive(Debug)]
pub struct UnimplementedBuiltinMacro {
    pub node: InFile<SyntaxNodePtr>,
}

#[derive(Debug)]
pub struct InvalidDeriveTarget {
    pub range: InFile<TextRange>,
}

#[derive(Debug)]
pub struct MalformedDerive {
    pub range: InFile<TextRange>,
}

#[derive(Debug)]
pub struct NoSuchField {
    pub field: InFile<AstPtr<Either<ast::RecordExprField, ast::RecordPatField>>>,
    pub private: Option<Field>,
    pub variant: VariantId,
}

#[derive(Debug)]
pub struct DuplicateField {
    pub field: InFile<AstPtr<Either<ast::RecordExprField, ast::RecordPatField>>>,
    pub variant: Variant,
}

#[derive(Debug)]
pub struct PrivateAssocItem {
    pub expr_or_pat: InFile<ExprOrPatPtr>,
    pub item: AssocItem,
}

#[derive(Debug)]
pub struct MismatchedTupleStructPatArgCount {
    pub expr_or_pat: InFile<ExprOrPatPtr>,
    pub expected: usize,
    pub found: usize,
    /// The struct or variant the pattern names.
    pub variant: Variant,
    /// Whether the pattern's type did not match the scrutinee's; rustc suggests parentheses
    /// only when it did (`emit_err_pat_wrong_number_of_fields`).
    pub had_err: bool,
}

#[derive(Debug)]
pub struct MismatchedArrayPatLen {
    pub pat: InFile<ExprOrPatPtr>,
    pub expected: u64,
    pub found: u64,
    pub has_rest: bool,
}

#[derive(Debug)]
pub struct ArrayPatternWithoutFixedLength {
    pub pat: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct ExpectedArrayOrSlicePat<'db> {
    pub pat: InFile<ExprOrPatPtr>,
    pub found: Type<'db>,
}

#[derive(Debug)]
pub struct InvalidRangePatType<'db> {
    pub pat: InFile<ExprOrPatPtr>,
    /// Each end's type as inference had it when it checked that end (an undecided literal's
    /// as `{integer}`), and whether that end is not `char` or numeric.
    pub lhs: Option<(Type<'db>, bool)>,
    pub rhs: Option<(Type<'db>, bool)>,
}

#[derive(Debug)]
pub struct ExpectedFunction<'db> {
    pub call: InFile<ExprOrPatPtr>,
    pub found: Type<'db>,
}

#[derive(Debug)]
pub struct CannotBeDereferenced<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub found: Type<'db>,
    /// `found` as rustc prints it, at the error: an integer or float literal's type not
    /// decided yet is `{integer}` / `{float}` (after fallback `found` says `i32` / `f64`).
    /// `None` when it held none.
    pub at_error: Option<String>,
}

#[derive(Debug)]
pub struct UnaryOperatorCannotBeApplied<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub op: ast::UnaryOp,
    pub found: Type<'db>,
    /// `found` as rustc prints it, at the error: an integer or float literal's type not
    /// decided yet is `{integer}` / `{float}` (after fallback `found` says `i32` / `f64`).
    /// `None` when it held none.
    pub at_error: Option<String>,
    /// The leaf goal of each fulfillment error of `found: Trait` (rustc's `errors` of
    /// `check_user_unop`), empty when the trait is missing or the goal holds.
    pub unmet: Vec<SolverGoal<'db>>,
}

#[derive(Debug)]
pub struct BinaryOperatorCannotBeApplied<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub op: ast::BinaryOp,
    pub lhs: Type<'db>,
    pub rhs: Type<'db>,
    /// `lhs` and `rhs` as rustc prints them, at the error ("cannot add `{integer}` to
    /// `bool`"; after fallback `lhs` says `i32`). `None` when neither held such a type.
    pub at_error: Option<(String, String)>,
    /// The leaf goal of each fulfillment error of `lhs: Trait<rhs>` (rustc's `errors` of
    /// `check_overloaded_binop`, `X: PartialEq` for `[X; 3] == [X; 3]`), empty when the
    /// trait is missing or the goal holds.
    pub unmet: Vec<SolverGoal<'db>>,
    /// The type expected of the operation (`ObligationCauseCode::BinOp`'s `output_ty`).
    pub output: Option<Type<'db>>,
}

#[derive(Debug)]
pub struct MutRefInImmRefPat {
    pub pat: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct CannotImplicitlyDerefTraitObject<'db> {
    pub pat: InFile<ExprOrPatPtr>,
    pub found: Type<'db>,
}

#[derive(Debug)]
pub struct CannotIndexInto<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub found: Type<'db>,
    /// `found` as rustc prints it, at the error: an integer or float literal's type not
    /// decided yet is `{integer}` / `{float}` (after fallback `found` says `i32` / `f64`).
    /// `None` when it held none.
    pub at_error: Option<String>,
}

#[derive(Debug)]
pub struct ExplicitDropMethodUse {
    pub expr_or_path: Either<InFile<AstPtr<ast::MethodCallExpr>>, InFile<AstPtr<ast::Path>>>,
}

/// An error of rustc's borrow checker.
#[derive(Debug)]
pub struct BorrowckError {
    pub node: InFile<SyntaxNodePtr>,
    /// The span is the brace that closes `node`, where rustc has a scope end.
    pub at_end: bool,
    pub code: &'static str,
    pub message: String,
    /// rustc's `span_label`s, in the order rustc adds them; one at the primary span is its
    /// own. A part whose source is not in the file (what a macro made with no syntax of its
    /// own) is left out.
    pub labels: Vec<(InFile<TextRange>, String)>,
    /// rustc's notes and helps, in the order rustc adds them.
    pub children: Vec<BorrowckChild>,
    /// rustc's suggestions, in the order rustc adds them.
    pub suggestions: Vec<BorrowckSuggestion>,
}

pub use hir_ty_borrowck::{Applicability as BorrowckApplicability, Sublevel as BorrowckSublevel};

/// A note or help of a [`BorrowckError`].
#[derive(Debug)]
pub struct BorrowckChild {
    pub level: BorrowckSublevel,
    pub message: String,
    pub spans: Vec<InFile<TextRange>>,
    /// Labels of spans of the child's `MultiSpan`, which need not be among `spans`.
    pub labels: Vec<(InFile<TextRange>, String)>,
}

/// A suggested edit of a [`BorrowckError`]: each part replaces the source of its range.
#[derive(Debug)]
pub struct BorrowckSuggestion {
    pub message: String,
    pub parts: Vec<(InFile<TextRange>, String)>,
    pub applicability: BorrowckApplicability,
    /// rustc's `span_suggestion_verbose`: shown as a diff of its own, never inline.
    pub verbose: bool,
}

type BorrowckParts =
    (Vec<(InFile<TextRange>, String)>, Vec<BorrowckChild>, Vec<BorrowckSuggestion>);

/// The ranges of `diagnostic`'s labels, notes and suggestions, the nodes of its spans from
/// `node_of`. A label or note span with no node is left out, and so is a suggestion any part
/// of which has none.
fn borrowck_parts(
    db: &Db,
    diagnostic: &hir_ty_borrowck::BorrowckDiagnostic,
    node_of: &impl Fn(hir_ty_mir::mir::MirSpan) -> Option<InFile<SyntaxNodePtr>>,
) -> BorrowckParts {
    use hir_ty_mir::mir::MirSpan;
    use hir_ty_borrowck::PartSpan;
    let syntax = |node: InFile<SyntaxNodePtr>| {
        let root = node.file_id.root(db);
        node.map(|it| view::to_node(it, root))
    };
    let expr = |expr| node_of(MirSpan::ExprId(expr));
    let range = |part: PartSpan| -> Option<InFile<TextRange>> {
        Some(match part {
            PartSpan::At(span @ MirSpan::ExprEnd(_)) => {
                let node = syntax(node_of(span)?);
                let whole = node.value.text_range();
                let range = node.value.last_token().map_or(whole, |it| it.text_range());
                node.with_value(range)
            }
            PartSpan::At(span) => node_of(span)?.map(|it| it.text_range()),
            PartSpan::Before(span) => {
                node_of(span)?.map(|it| TextRange::empty(it.text_range().start()))
            }
            PartSpan::After(span) => {
                node_of(span)?.map(|it| TextRange::empty(it.text_range().end()))
            }
            PartSpan::Head(outer, inner) => {
                let (outer, inner) = (expr(outer)?, expr(inner)?);
                if outer.file_id != inner.file_id {
                    return None;
                }
                let start = outer.value.text_range().start();
                outer.with_value(TextRange::new(start, inner.value.text_range().start()))
            }
            PartSpan::Tail(outer, inner) => {
                let (outer, inner) = (expr(outer)?, expr(inner)?);
                if outer.file_id != inner.file_id {
                    return None;
                }
                let end = outer.value.text_range().end();
                outer.with_value(TextRange::new(inner.value.text_range().end(), end))
            }
            PartSpan::MethodName(call) => {
                let node = syntax(expr(call)?);
                let name = view::MethodCallExpr::cast(node.value)?.name_ref()?;
                InFile::new(node.file_id, name.syntax().text_range())
            }
            // rustc's `fn_decl_span`: a closure through its parameters and return type, a
            // block all of it.
            PartSpan::ClosureHead(closure) => {
                let node = syntax(expr(closure)?);
                let range = node.value.text_range();
                let end = view::ClosureExpr::cast(node.value).and_then(|it| {
                    let ret = it.ret_type().map(|it| it.syntax().text_range().end());
                    ret.or_else(|| Some(it.param_list()?.syntax().text_range().end()))
                });
                let end = end.map_or(range, |end| TextRange::new(range.start(), end));
                InFile::new(node.file_id, end)
            }
            // rustc's `def_span`: from the item's start, past its attributes, through its name
            // and generics.
            PartSpan::Def(adt) => {
                let (file_id, item) = match adt {
                    AdtId::StructId(it) => {
                        let source = hir_def::src::HasSource::source_view(it.lookup(db), db);
                        (source.file_id, view::Adt::Struct(source.value))
                    }
                    AdtId::EnumId(it) => {
                        let source = hir_def::src::HasSource::source_view(it.lookup(db), db);
                        (source.file_id, view::Adt::Enum(source.value))
                    }
                    AdtId::UnionId(it) => {
                        let source = hir_def::src::HasSource::source_view(it.lookup(db), db);
                        (source.file_id, view::Adt::Union(source.value))
                    }
                };
                let start = item
                    .syntax()
                    .children_with_tokens()
                    .find(|it| {
                        !matches!(
                            it.kind(),
                            syntax::SyntaxKind::ATTR
                                | syntax::SyntaxKind::COMMENT
                                | syntax::SyntaxKind::WHITESPACE
                        )
                    })?
                    .text_range()
                    .start();
                let end = match view::HasGenericParams::generic_param_list(&item) {
                    Some(generics) => generics.syntax().text_range().end(),
                    None => item.name()?.syntax().text_range().end(),
                };
                InFile::new(file_id, TextRange::new(start, end))
            }
        })
    };
    let labels = |labels: &[(PartSpan, String)]| -> Vec<(InFile<TextRange>, String)> {
        // alloc: error path, the parts of a reported borrowck error
        labels.iter().filter_map(|(at, text)| Some((range(*at)?, text.clone()))).collect()
    };
    let children = diagnostic
        .children
        .iter()
        .map(|child| BorrowckChild {
            level: child.level,
            // clone: error path, a reported borrowck error's note
            message: child.message.clone(),
            // alloc: error path, a reported borrowck error's note
            spans: child.spans.iter().filter_map(|&it| range(it)).collect(),
            labels: labels(&child.labels),
        })
        // alloc: error path, the notes of a reported borrowck error
        .collect();
    let suggestions = diagnostic
        .suggestions
        .iter()
        .filter_map(|suggestion| {
            let parts = suggestion
                .parts
                .iter()
                // clone: error path, a reported borrowck error's suggestion
                .map(|(at, text)| Some((range(*at)?, text.clone())))
                .collect::<Option<Vec<_>>>()?;
            Some(BorrowckSuggestion {
                // clone: error path, a reported borrowck error's suggestion
                message: suggestion.message.clone(),
                parts,
                applicability: suggestion.applicability,
                verbose: suggestion.verbose,
            })
        })
        // alloc: error path, the suggestions of a reported borrowck error
        .collect();
    (labels(&diagnostic.labels), children, suggestions)
}

/// An error of rustc's const checker: what a `const fn`, a `const` or a `static` may not do.
#[derive(Debug)]
pub struct ConstCheckError {
    pub node: InFile<SyntaxNodePtr>,
    pub code: &'static str,
    pub message: String,
    pub kind: hir_ty_mir::mir::const_check::ConstCheckKind,
    pub context: hir_ty_mir::mir::const_check::ConstContext,
}

/// A `const fn` whose result at run time depends on more than its arguments: "const fn reads
/// or writes mutable state: `CALLS` is `AtomicU64`, which is not `Freeze`".
#[derive(Debug)]
pub struct ConstImpure {
    pub node: InFile<SyntaxNodePtr>,
    pub message: String,
}

/// A body the borrow checker skipped, because MIR lowering does not support something in it:
/// always an error, as blaze has not checked what rustc would.
#[derive(Debug)]
pub struct BorrowckSkipped {
    pub node: InFile<SyntaxNodePtr>,
    pub reason: String,
}

/// The evaluation of a free `const` or a `static` failed. rustc evaluates each `static` and
/// each free `const` without generics of the crate it checks (`rustc_interface::passes::analysis`:
/// `eval_static_initializer`, `const_eval_poly`), whether or not anything uses it.
#[derive(Debug)]
pub struct ConstEvalFailed {
    /// Where in the item's value the evaluation failed (the call it was in), or the value.
    pub node: InFile<SyntaxNodePtr>,
    /// The item's name, `_` for an unnamed `const`.
    pub item: String,
    pub is_static: bool,
    pub failure: ConstEvalFailure,
}

#[derive(Debug)]
pub enum ConstEvalFailure {
    /// What the evaluated code did that a constant may not, in rustc's words: E0080.
    Failed(String),
    /// The value needs itself: E0391.
    Cycle,
    /// What blaze's evaluator cannot do: an error, as blaze has not checked what rustc would.
    Unsupported(String),
}

/// A diagnostic the front end found in code with no syntax of its own (what lowering
/// desugared), which blaze can't place where rustc does: blaze's error at the item the code is
/// in, naming the diagnostic, where it would otherwise be dropped.
#[derive(Debug)]
pub struct UnplacedDiagnostic {
    /// The anonymous constant's expression, or the item whose body the code is in.
    pub owner: InFile<SyntaxNodePtr>,
    pub what: Unplaced,
}

/// The diagnostic an [`UnplacedDiagnostic`] could not place.
#[derive(Debug)]
pub enum Unplaced {
    /// Type checking's, as text: its code and message, naming the item it is in and the
    /// types, trait and binding it is of.
    Inference(String),
    /// E0133, or a lint's, for an operation that needs `unsafe`.
    MissingUnsafe { reason: UnsafetyReason, lint: UnsafeLint },
    /// The borrow checker's, with its code (a lint's name for a lint).
    Borrowck { code: &'static str, message: String },
    /// Const checking's.
    ConstCheck { code: &'static str, message: String },
}

#[derive(Debug)]
pub struct FruInDestructuringAssignment {
    pub node: InFile<AstPtr<ast::Expr>>,
}

#[derive(Debug)]
pub struct MissingBody {
    pub node: InFile<SyntaxNodePtr>,
    pub kind: MissingBodyItemKind,
}

#[derive(Debug)]
pub struct FunctionalRecordUpdateOnNonStruct {
    pub base_expr: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct UnresolvedField<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub receiver: Type<'db>,
    pub name: Name,
    pub method_with_same_name_exists: bool,
}

#[derive(Debug)]
pub struct UnresolvedMethodCall<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub receiver: Type<'db>,
    pub name: Name,
    pub field_with_same_name: Option<Type<'db>>,
    pub assoc_func_with_same_name: Option<Function>,
}

#[derive(Debug)]
pub struct UnresolvedAssocItem<'db> {
    pub expr_or_pat: InFile<ExprOrPatPtr>,
    /// The type the item was looked up on.
    pub ty: Type<'db>,
    pub name: Name,
}

#[derive(Debug)]
pub struct UnresolvedIdent {
    pub node: InFile<(ExprOrPatPtr, Option<TextRange>)>,
}

/// A path of a body (an expression, a struct literal's, a pattern's) that resolves to nothing:
/// `segment` indexes the first of its segments that fails, the written segments after any
/// `crate`/`self`/`super`/`$crate`. A prefix failing is rustc's E0433; the last segment E0425
/// for a value, E0422 for a struct literal or record pattern, E0531 for a tuple struct
/// pattern.
#[derive(Debug)]
pub struct UnresolvedPath {
    pub node: InFile<ExprOrPatPtr>,
    pub segment: u32,
}

#[derive(Debug)]
pub struct PrivateField {
    pub expr: InFile<ExprOrPatPtr>,
    pub field: Field,
}

pub use hir_ty_unsafeck::{UnsafeLint, UnsafetyReason};

/// An operation that needs `unsafe` where it is not: `hir_ty_unsafeck::UnsafeOp` at its syntax.
#[derive(Debug)]
pub struct MissingUnsafe {
    pub node: InFile<ExprOrPatPtr>,
    pub lint: UnsafeLint,
    pub reason: UnsafetyReason,
    /// The first `unsafe_op_in_unsafe_fn` of its function or closure, where rustc notes that an
    /// unsafe function restricts its caller.
    pub first: bool,
    /// In the body of an `unsafe fn`, outside a block: a [`UnsafeLint::DeprecatedSafe2024`]
    /// there is given only where `unsafe_op_in_unsafe_fn` is not allowed.
    pub in_unsafe_fn: bool,
}

#[derive(Debug)]
pub struct MissingFields {
    pub file: HirFileId,
    pub field_list_parent: AstPtr<Either<ast::RecordExpr, ast::RecordPat>>,
    pub field_list_parent_path: Option<AstPtr<ast::Path>>,
    /// The struct or variant built or matched; `missed_fields` is empty for a struct expression
    /// whose `..` has no base and leaves out only fields with defaults.
    pub variant: Variant,
    pub missed_fields: Vec<(Name, Field)>,
}

#[derive(Debug)]
pub struct ReplaceFilterMapNextWithFindMap {
    pub file: HirFileId,
    /// This expression is the whole method chain up to and including `.filter_map(..).next()`.
    pub next_expr: AstPtr<ast::Expr>,
}

#[derive(Debug)]
pub struct MismatchedArgCount {
    pub call_expr: InFile<ExprOrPatPtr>,
    pub expected: usize,
    pub found: usize,
    /// True when the call is through a `Fn`/`FnMut`/`FnOnce` trait (E0057)
    /// rather than a regular function call (E0061).
    pub is_fn_trait_call: bool,
    pub args: CallArgs,
}

/// rustc's `report_arg_errors` of a call whose argument count fits but some argument does not
/// (E0308 "arguments to this function are incorrect"); each such argument also has its
/// `TypeMismatch`.
#[derive(Debug)]
pub struct IncorrectArguments {
    pub call_expr: InFile<ExprOrPatPtr>,
    pub args: CallArgs,
}

/// What rustc's `report_arg_errors` starts from, every type as rustc prints it at the error
/// (`{integer}` for an undecided literal, `{type error}` for any other variable).
#[derive(Debug)]
pub struct CallArgs {
    /// rustc's `callee_ty`: the callee's type after adjustments, the receiver's of a method call.
    pub callee: String,
    /// The method a method call resolved to.
    pub method: Option<Function>,
    pub is_method: bool,
    pub c_variadic: bool,
    /// The callee's inputs, without a method's receiver.
    pub formal_inputs: Vec<String>,
    /// `formal_inputs` as the call's expectation has them.
    pub expected_inputs: Vec<String>,
    /// The provided arguments' types after adjustments (of a C-variadic call, those of its
    /// fixed inputs only).
    pub provided: Vec<String>,
    compatible_ends: Box<[u32]>,
    compatible: Box<[u32]>,
}

impl CallArgs {
    /// `ArgMatrix::new`'s row of the `provided`th argument: the inputs it fits, in order.
    pub fn compatible_with(&self, provided: usize) -> &[u32] {
        let start = provided.checked_sub(1).map_or(0, |it| self.compatible_ends[it] as usize);
        &self.compatible[start..self.compatible_ends[provided] as usize]
    }

    fn new(db: &Db, target: DisplayTarget, args: &hir_ty::CallArguments) -> Self {
        // alloc: error path, the types of a reported call's arguments as rustc prints them
        let show = |ty: &StoredTy| ty.as_ref().display(db, target).to_string();
        // alloc: error path, as above
        let shows = |tys: &[StoredTy]| tys.iter().map(show).collect();
        CallArgs {
            callee: show(&args.callee),
            method: args.method.map(Into::into),
            is_method: args.is_method,
            c_variadic: args.c_variadic,
            formal_inputs: shows(&args.formal_inputs),
            expected_inputs: shows(&args.expected_inputs),
            provided: shows(&args.provided),
            // clone: error path, out of the inference result a reported call borrows
            compatible_ends: args.compatible_ends.clone(),
            // clone: error path, as above
            compatible: args.compatible.clone(),
        }
    }
}

/// E0004.
#[derive(Debug)]
pub struct MissingMatchArms<'db> {
    pub scrutinee_expr: InFile<AstPtr<ast::Expr>>,
    pub scrutinee_ty: Type<'db>,
    /// The match has no arms: rustc says "type `T` is non-empty" for a type that is not an
    /// enum with variants.
    pub no_arms: bool,
    /// Every pattern not covered, as rustc prints it (`print_witness_pat`).
    pub witnesses: Vec<String>,
    /// The variants of the scrutinee's enum the first witnesses are of, for "not covered"
    /// labels at them (`maybe_point_at_variant`).
    pub variants: Vec<EnumVariant>,
    /// The types in the first witness rustc adds a note about (`collect_special_tys`).
    pub special_tys: Vec<(Type<'db>, SpecialTy)>,
    pub would_be_exhaustive_without_guards: bool,
    /// The scrutinee is a reference to a type uninhabited from here: "references are always
    /// considered inhabited".
    pub ref_to_uninhabited: bool,
    /// The arms whose pattern is a constant that looks like a binding, in arm order.
    pub consts_like_bindings: Vec<(InFile<AstPtr<ast::Pat>>, Const)>,
}

/// E0005: a refutable pattern of a `let`, a parameter, a `for` loop or a destructuring
/// assignment (whose pattern is an expression).
#[derive(Debug)]
pub struct NonExhaustiveLet<'db> {
    pub pat: InFile<ExprOrPatPtr>,
    pub origin: BindingOrigin,
    /// The `let` has an initializer, for "consider using `if let`" or "`let else`".
    pub has_initializer: bool,
    pub pattern_ty: Type<'db>,
    /// As [`MissingMatchArms::witnesses`].
    pub witnesses: Vec<String>,
    pub variants: Vec<EnumVariant>,
    pub special_tys: Vec<(Type<'db>, SpecialTy)>,
    /// The first witness's variant is uninhabited only through fields not visible here.
    pub privately_uninhabited: bool,
    /// The pattern is a constant that looks like a binding.
    pub interpreted_as_const: Option<Const>,
}

/// E0030 (`lo..=hi` with `lo > hi`, `inclusive`) or E0579 (`lo..hi` with `lo >= hi`).
#[derive(Debug)]
pub struct EmptyRangePattern {
    pub pat: InFile<AstPtr<ast::Pat>>,
    pub inclusive: bool,
}

/// "reached pattern complexity limit", at a match's scrutinee or a binding's pattern.
#[derive(Debug)]
pub struct PatternComplexityLimit {
    pub at: InFile<SyntaxNodePtr>,
}

/// "mix of deref patterns and normal constructors".
#[derive(Debug)]
pub struct MixedDerefPatterns {
    pub deref_pat: InFile<AstPtr<ast::Pat>>,
    pub normal_pat: InFile<AstPtr<ast::Pat>>,
}

/// A constant used as a pattern that rustc's `const_to_pat` refuses, at the path that names it.
#[derive(Debug)]
pub struct ConstPatternRefused<'db> {
    pub pat: InFile<AstPtr<ast::Pat>>,
    pub error: ConstPatternError<'db>,
}

#[derive(Debug)]
pub struct NonExhaustiveRecordExpr {
    pub expr: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct NonExhaustiveRecordPat {
    pub pat: InFile<ExprOrPatPtr>,
    pub variant: Variant,
}

#[derive(Debug)]
pub struct TypeMismatch<'db> {
    pub expr_or_pat: InFile<ExprOrPatPtr>,
    pub expected: Type<'db>,
    pub actual: Type<'db>,
    /// `expected` and `actual` printed as inference had them at the error, as rustc prints
    /// them: an integer or float literal's type not decided yet is `{integer}` / `{float}`
    /// (after fallback `expected` and `actual` say `i32` / `f64`). `None` when neither held one.
    pub at_error: Option<(String, String)>,
    pub cause: ObligationCauseCode,
}

/// Why inference required a type or a bound (rustc's `ObligationCauseCode`, the kinds the
/// front end records), for the notes rustc adds to an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObligationCauseCode {
    /// Nothing more to say than the span.
    Misc,
    /// The `bound_index`th predicate of `item` (counting its parents' first), required by a use
    /// of `item` in an expression: rustc's "required by a bound in `item`".
    WhereClauseInExpr { item: GenericDef, bound_index: u32, arg_index: Option<u32> },
    /// A function argument's type must be `Sized`.
    SizedArgumentType,
    /// The declared return type of a body must be `Sized`.
    SizedReturnType,
    /// The tail of a block, or a `match`'s first arm (with no arm of a type other than `!`
    /// before it).
    BlockTailExpression,
    /// The value of a `return` or of a body's tail.
    ReturnValue,
    /// The `else` of an `if` must agree with its `then`.
    IfExpression,
    /// A `match` arm must agree with the `prior_arms` arms before it whose type is not `!`.
    MatchExpressionArm { prior_arms: u32 },
    /// A `let`'s type must be `Sized`.
    LetType,
    /// An `if` without an `else` is `()`, which must agree with what the `if` is expected to be.
    IfExpressionWithNoElse,
    /// A `return` without a value returns `()`, which must agree with the return type.
    ReturnNoExpression,
}

impl From<hir_ty::next_solver::infer::traits::ObligationCauseCode> for ObligationCauseCode {
    fn from(code: hir_ty::next_solver::infer::traits::ObligationCauseCode) -> Self {
        use hir_ty::next_solver::infer::traits::ObligationCauseCode as C;
        match code {
            C::Misc => Self::Misc,
            C::WhereClauseInExpr { item, bound_index, arg_index } => {
                Self::WhereClauseInExpr { item: item.into(), bound_index, arg_index }
            }
            C::SizedArgumentType => Self::SizedArgumentType,
            C::SizedReturnType => Self::SizedReturnType,
            C::BlockTailExpression => Self::BlockTailExpression,
            C::ReturnValue => Self::ReturnValue,
            C::IfExpression => Self::IfExpression,
            C::MatchExpressionArm { prior_arms } => Self::MatchExpressionArm { prior_arms },
            C::LetType => Self::LetType,
            C::IfExpressionWithNoElse => Self::IfExpressionWithNoElse,
            C::ReturnNoExpression => Self::ReturnNoExpression,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct IncoherentImpl {
    pub file_id: HirFileId,
    pub impl_: AstPtr<ast::Impl>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TraitImplOrphan {
    pub file_id: HirFileId,
    pub impl_: AstPtr<ast::Impl>,
}

// FIXME: Split this off into the corresponding 4 rustc errors
#[derive(Debug, PartialEq, Eq)]
pub struct TraitImplIncorrectSafety {
    pub file_id: HirFileId,
    pub impl_: AstPtr<ast::Impl>,
    pub should_be_safe: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TraitImplMissingAssocItems {
    pub file_id: HirFileId,
    pub impl_: AstPtr<ast::Impl>,
    pub missing: Vec<(Name, AssocItem)>,
}

/// rustc's `default_body_is_unstable` (E0658): a trait impl that leaves `item` to the trait's
/// default body, which `#[rustc_default_body_unstable]` gates behind a feature its crate does
/// not enable.
#[derive(Debug, PartialEq, Eq)]
pub struct TraitImplUnstableDefaultBody {
    pub file_id: HirFileId,
    pub impl_: AstPtr<ast::Impl>,
    pub item: Name,
    pub feature: intern::Symbol,
    pub reason: hir_def::attrs::stability::UnstableReason,
    pub issue: Option<std::num::NonZero<u32>>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TraitImplRedundantAssocItems {
    pub file_id: HirFileId,
    pub trait_: Trait,
    pub impl_: AstPtr<ast::Impl>,
    pub assoc_item: (Name, AssocItem),
}

#[derive(Debug)]
pub struct RemoveTrailingReturn {
    pub return_expr: InFile<AstPtr<ast::ReturnExpr>>,
}

#[derive(Debug)]
pub struct RemoveUnnecessaryElse {
    pub if_expr: InFile<AstPtr<ast::IfExpr>>,
}

#[derive(Debug)]
pub struct UnusedMustUse<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub message: Option<&'db str>,
}

#[derive(Debug)]
pub struct CastToUnsized<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub cast_ty: Type<'db>,
}

#[derive(Debug)]
pub struct InvalidCast<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub error: CastError,
    pub expr_ty: Type<'db>,
    pub cast_ty: Type<'db>,
    /// rustc's `PointerKind` of the expression's pointee, where the check of a pointer cast
    /// found it: `DifferingKinds`, `SizedUnsizedCast`, `NeedViaThinPtr`, and
    /// `UnknownCastPtrKind` of a cast to a wide pointer of unknown pointee.
    pub expr_pointer: Option<CastPointerKind>,
    /// Likewise of the pointee of the type cast to: also `IntToWideCast` and the
    /// `IllegalCast` of a function pointer.
    pub cast_pointer: Option<CastPointerKind>,
    /// Of `PtrPtrAddingAutoTraits`: the auto traits the target `dyn` adds, in its order.
    pub added_auto_traits: Vec<crate::Trait>,
}

/// The metadata of a pointer, as rustc's cast errors tell it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastPointerKind {
    Thin,
    /// A trait object, with its principal trait if it has one.
    VTable(Option<crate::Trait>),
    /// A slice or `str`.
    Length,
    /// A projection or opaque type's.
    OfAlias,
    /// A type parameter's.
    OfParam(crate::TypeParam),
}

#[derive(Debug)]
pub struct GenericArgsProhibited {
    /// The first of `all_args`.
    pub args: InFile<AstPtr<Either<ast::GenericArgList, ast::ParenthesizedArgList>>>,
    /// The arguments of every segment rustc's one `prohibit_generic_args` checks, in order:
    /// a path's leading segments, with the last when it is a type parameter, `Self`, a
    /// primitive type or a module. Return type notation is left out: segments of only that
    /// are `BadRtn`.
    pub all_args: Box<[InFile<AstPtr<Either<ast::GenericArgList, ast::ParenthesizedArgList>>>]>,
    /// The last segment's reason.
    pub reason: GenericArgsProhibitedReason,
}

#[derive(Debug)]
pub struct ParenthesizedGenericArgsWithoutFnTrait {
    pub args: InFile<AstPtr<ast::ParenthesizedArgList>>,
}

#[derive(Debug)]
pub struct BadRtn {
    pub rtn: InFile<AstPtr<ast::ReturnTypeSyntax>>,
}

#[derive(Debug)]
pub struct InferVarsNotAllowed {
    /// The first of `nodes`.
    pub node: InFile<SyntaxNodePtr>,
    /// Every placeholder of the error in source order: one, or all of a type its body infers
    /// (a return type, a const's or a static's), as rustc reports them together.
    pub nodes: Box<[InFile<SyntaxNodePtr>]>,
}

#[derive(Debug)]
pub struct IncorrectGenericsLen {
    /// Points at the name if there are no generics.
    pub generics_or_segment: InFile<AstPtr<Either<ast::GenericArgList, ast::NameRef>>>,
    pub kind: IncorrectGenericsLenKind,
    pub provided: u32,
    pub expected: u32,
    pub def: GenericDef,
}

#[derive(Debug)]
pub struct MissingLifetime {
    /// Points at the name if there are no generics.
    pub generics_or_segment: InFile<AstPtr<Either<ast::GenericArgList, ast::NameRef>>>,
    pub expected: u32,
    pub def: GenericDef,
    /// [`MissingLifetimeKind::Missing`] or [`MissingLifetimeKind::ElisionFailure`].
    pub kind: MissingLifetimeKind,
}

/// Lifetimes left out with none to elide to: one site, or for
/// [`MissingLifetimeKind::ElisionFailure`] every site of one return type, rustc's one error.
#[derive(Debug)]
pub struct MissingLifetimes {
    pub sites: Box<[MissingLifetimeSite]>,
    pub kind: MissingLifetimeKind,
}

#[derive(Debug)]
pub enum MissingLifetimeSite {
    /// `&T`.
    Reference(InFile<AstPtr<ast::RefType>>),
    /// `'_`.
    Placeholder(InFile<AstPtr<ast::Lifetime>>),
    /// A path to `def` without its lifetime arguments.
    Path {
        /// Points at the name if there are no generics.
        generics_or_segment: InFile<AstPtr<Either<ast::GenericArgList, ast::NameRef>>>,
        expected: u32,
        def: GenericDef,
    },
}

/// Lifetime arguments given to a function with late-bound lifetime parameters: E0794 when
/// `hard_error`, else the `late_bound_lifetime_arguments` lint.
#[derive(Debug)]
pub struct LateBoundLifetimeArgs {
    /// Points at the name if there are no generics.
    pub generics_or_segment: InFile<AstPtr<Either<ast::GenericArgList, ast::NameRef>>>,
    pub def: GenericDef,
    pub hard_error: bool,
}

/// `Self::Name` in `impl Trait for Enum` where `Enum` has a variant `Name`, resolved to
/// `Trait`'s associated type `item`: the `ambiguous_associated_items` lint.
#[derive(Debug)]
pub struct AmbiguousAssociatedItem<'db> {
    pub path: InFile<AstPtr<ast::Path>>,
    pub variant: Variant,
    pub item: TypeAlias,
    /// The impl's self type, the enum.
    pub self_ty: Type<'db>,
    /// The trait the item is of.
    pub bound: Trait,
}

#[derive(Debug)]
pub struct ElidedLifetimesInPath {
    /// Points at the name if there are no generics.
    pub generics_or_segment: InFile<AstPtr<Either<ast::GenericArgList, ast::NameRef>>>,
    pub expected: u32,
    pub def: GenericDef,
    pub hard_error: bool,
}

#[derive(Debug)]
pub struct TypeMustBeKnown<'db> {
    pub at_point: SpanSyntax,
    pub top_term: Option<Either<Type<'db>, String>>,
    pub kind: TypeAnnotationNeeded,
    /// The goal left ambiguous, for an error of a goal still pending when inference is done
    /// (rustc's `maybe_report_ambiguity`); `None` for a type writeback found undecided.
    pub ambiguity: Option<TypeAmbiguity<'db>>,
    /// Where the undecided variable (the first of `top_term` made for a parameter) was made and
    /// the type or const parameter it was made for, declared on its `parent` (rustc's
    /// `TypeVariableOrigin`: ", where the type for type parameter `T` is specified" and the
    /// turbofish suggestion). A type parameter only when the variable was not made in a macro
    /// expansion, as rustc's `extract_inference_diagnostics_data`.
    pub origin: Option<(SpanSyntax, GenericParam)>,
    /// `top_term` as rustc prints it in "consider giving `x` an explicit type" (its
    /// `ty_infer_name_resolver`). Each undecided variable made for a parameter is that
    /// parameter, and every other one is unknown. A trait's `Self`, an `impl Trait` argument's
    /// parameter, and a type parameter whose variable was made in a macro expansion are unknown.
    pub named_term: Option<Either<Type<'db>, String>>,
    /// Why the ambiguous goal was required, for rustc's closing `note_obligation_cause`
    /// ("required by a bound in `f`"); `Misc` for a type writeback found undecided.
    pub cause: ObligationCauseCode,
}

/// An ambiguous goal and the goals rustc merges into its error as notes ("the type must also
/// implement `Tr`", "cannot satisfy `..`"): those of the same inference variable at the same
/// place.
#[derive(Debug)]
pub struct TypeAmbiguity<'db> {
    pub primary: AmbiguousGoal<'db>,
    pub related: Vec<AmbiguousGoal<'db>>,
}

#[derive(Debug)]
pub struct AmbiguousGoal<'db> {
    pub goal: SolverGoal<'db>,
    /// The impls and where clauses that could meet a trait goal, when rustc lists them
    /// (`annotate_source_of_ambiguity`): two to nine of them and an argument of the trait known.
    pub candidates: Vec<AmbiguityCandidate<'db>>,
}

#[derive(Debug)]
pub enum AmbiguityCandidate<'db> {
    Impl(crate::Impl),
    /// A where clause of the body's owner.
    WhereClause(crate::TraitRef<'db>),
}

/// A goal of the trait solver as rustc's errors print it, its undecided types `{unknown}`.
/// A term that is a constant is its text.
#[derive(Debug)]
pub enum SolverGoal<'db> {
    /// `T: Trait` or `T: !Trait`.
    Trait(crate::TraitClause<'db>),
    /// `T: [const] Trait` (`maybe_const`) or `T: const Trait`.
    HostEffect { trait_ref: crate::TraitRef<'db>, maybe_const: bool },
    /// `alias == term`: `<T as Trait>::Assoc == U`.
    Projection { alias: Either<Type<'db>, String>, term: Either<Type<'db>, String> },
    /// What `alias` normalizes to: rustc's "cannot normalize `alias`".
    NormalizesTo { alias: Either<Type<'db>, String> },
    WellFormed(Either<Type<'db>, String>),
    /// `a <: b`.
    Subtype { a: Type<'db>, b: Type<'db> },
    /// `a` coerces to `b`.
    Coerce { a: Type<'db>, b: Type<'db> },
    ConstEvaluatable(String),
    ConstArgHasType { ct: String, ty: Type<'db> },
    ConstEquate { a: String, b: String },
    DynCompatible(Trait),
    /// An impl's `#[unstable_feature_bound(feature)]`.
    UnstableFeature(intern::Symbol),
    /// `a: b`: a type, or a region printed as rustc prints it, outlives the region `b`.
    Outlives { a: Either<Type<'db>, String>, b: String },
    Ambiguous,
}

/// A trait solver error other than an unmet trait bound or a closure of the wrong kind: rustc's
/// `report_fulfillment_error` for the other codes.
#[derive(Debug)]
pub struct TraitSolverError<'db> {
    pub span: SpanSyntax,
    pub cause: ObligationCauseCode,
    pub kind: TraitSolverErrorKind<'db>,
}

#[derive(Debug)]
pub enum TraitSolverErrorKind<'db> {
    /// A goal that does not hold that is not a trait's: a `[const] Trait` bound, a trait that
    /// is not dyn-compatible (E0038), a type that is not well-formed; `non_const_impl`, for a
    /// `[const] Trait` or `const Trait` goal of a `const` trait, the impl that meets the trait
    /// goal when it is not `const` (rustc's `report_host_effect_error`).
    GoalUnmet { goal: SolverGoal<'db>, non_const_impl: Option<crate::Impl> },
    /// E0275: evaluating the goal overflowed; `suggest_increasing_limit`: rustc's help to raise
    /// `#![recursion_limit]`.
    Overflow { goal: SolverGoal<'db>, suggest_increasing_limit: bool },
    /// E0271: "type mismatch resolving `goal`"; `normalized`, what the goal's alias normalizes
    /// to where that is not the goal's term (rustc's "expected `term`, found `normalized`"): a
    /// type, or a constant's text; `impl_`, the user-written impl selection picks for the
    /// projection's trait ref, whose associated item rustc labels "expected this to be `term`"
    /// when it is local and final.
    ProjectionMismatch {
        goal: SolverGoal<'db>,
        normalized: Option<Either<Type<'db>, String>>,
        impl_: Option<crate::Impl>,
    },
    /// E0308 of a subtyping or coercion goal.
    TypesDiffer { expected: Type<'db>, found: Type<'db> },
    /// "the constant `ct` is not of type `expected_ty`", which is `ct_ty`.
    ConstArgHasWrongType { ct: String, ct_ty: Type<'db>, expected_ty: Type<'db> },
    /// "higher-ranked lifetime bound could not be satisfied".
    HigherRankedOutlives,
    /// An error only rustc's old solver gives, which blaze does not describe.
    Unreported(UnreportedSolverError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenericArgKind {
    Lifetime,
    Type,
    Const,
}

impl GenericArgKind {
    fn from_id(id: GenericParamId) -> Self {
        match id {
            GenericParamId::TypeParamId(_) => GenericArgKind::Type,
            GenericParamId::ConstParamId(_) => GenericArgKind::Const,
            GenericParamId::LifetimeParamId(_) => GenericArgKind::Lifetime,
        }
    }
}

#[derive(Debug)]
pub struct IncorrectGenericsOrder {
    pub provided_arg: InFile<AstPtr<ast::GenericArg>>,
    pub expected_kind: GenericArgKind,
    /// The parameter the argument met, of `expected_kind`.
    pub param: GenericParam,
}

#[derive(Debug)]
pub struct GenericDefaultRefersToSelf {
    /// The `Self` segment.
    pub segment: InFile<AstPtr<ast::PathSegment>>,
}

/// The associated type `segment` names that `qself` does not give for `failure`: E0220 not
/// found, E0221 ambiguous, E0391 a cycle. `qself` is the type parameter (`Self` of a trait
/// included) whose bounds were looked in, or the trait of `Trait::Name`.
#[derive(Debug)]
pub struct AssocTypeNotResolved {
    pub segment: InFile<AstPtr<ast::PathSegment>>,
    pub qself: Either<TypeParam, Trait>,
    pub failure: AssocTypeFailure,
}

/// An associated type `segment` of a type no bound gives it of: E0223.
#[derive(Debug)]
pub struct AmbiguousAssocType {
    pub segment: InFile<AstPtr<ast::PathSegment>>,
}

/// `impl Trait` where it is not allowed: E0562.
#[derive(Debug)]
pub struct ImplTraitNotAllowed {
    pub ty: InFile<AstPtr<ast::Type>>,
}

/// A trait object with more than one non-auto trait: E0225, at the `first` bound and the
/// `additional` ones.
#[derive(Debug)]
pub struct AdditionalNonAutoTraits {
    pub first: InFile<AstPtr<ast::Type>>,
    pub additional: Box<[InFile<AstPtr<ast::Type>>]>,
}

/// A second value of the associated item `assoc` in a trait object, at `bound`: E0719.
#[derive(Debug)]
pub struct ConflictingAssocTypeBindings {
    pub bound: InFile<AstPtr<ast::Type>>,
    pub assoc: AssocItem,
}

/// A trait object of `trait_` at `bound` that leaves out `params`, whose defaults name
/// `Self`: E0393.
#[derive(Debug)]
pub struct MissingSelfDefaultParams {
    pub bound: InFile<AstPtr<ast::Type>>,
    pub trait_: Trait,
    pub params: Box<[TypeParam]>,
}

/// A trait object whose bounds are only relaxed (`dyn ?Sized`), at the first: E0224.
#[derive(Debug)]
pub struct ObjectWithoutTrait {
    pub bound: InFile<AstPtr<ast::Type>>,
}

/// The pattern of the pattern type `ty`, which is not lowered for `reason`.
#[derive(Debug)]
pub struct PatternTypeNotLowered {
    pub ty: InFile<AstPtr<ast::Type>>,
    pub reason: PatternTypeUnsupported,
}

#[derive(Debug)]
pub struct UnionExprMustHaveExactlyOneField {
    pub expr: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct UnionPatMustHaveExactlyOneField {
    pub pat: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct UnionPatHasRest {
    pub pat: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct InvalidLhsOfAssignment {
    pub lhs: InFile<AstPtr<Either<ast::Expr, ast::Pat>>>,
    /// The `=` or compound assignment, which may come from another expansion than `lhs`.
    pub assign: InFile<ExprOrPatPtr>,
    /// `None` for `=` (E0070), the operator of a compound assignment (E0067).
    pub op: Option<ast::ArithOp>,
}

#[derive(Debug)]
pub struct MethodCallIllegalSizedBound {
    pub call_expr: InFile<ExprOrPatPtr>,
    pub method: crate::Function,
    /// Whose own bounds or where clause (the method's, or its trait's or impl's) hold the
    /// `Self: Sized` bound; `None` where it is only implied (rustc gives it no label).
    pub bound: Option<crate::GenericDef>,
    /// rustc's `needs_mut`: the receiver of the other mutability picks another method.
    pub needs_mut: bool,
    /// The traits, in scope or not, a probe of every crate's traits picks the method from.
    pub candidates: Vec<crate::Trait>,
}

/// rustc's `lint_edition_dependent_dot_call`: a method call that a later edition's prelude or
/// `IntoIterator` impl resolves to another method.
#[derive(Debug)]
pub struct EditionDependentMethodCall {
    pub call_expr: InFile<AstPtr<ast::MethodCallExpr>>,
    pub receiver: InFile<ExprOrPatPtr>,
    pub lint: crate::EditionLint,
    /// The picked method, which the suggestion calls by its trait's or type's path.
    pub method: Function,
    /// An inherent or trait object pick: the suggestion only adjusts the receiver.
    pub inherent: bool,
    pub autoderefs: u32,
    pub autoref: Option<crate::ReceiverAdjustment>,
}

/// rustc's `lint_fully_qualified_call_from_2018`: a type-relative path to `try_from`,
/// `try_into` or `from_iter` that the 2021 prelude makes ambiguous.
#[derive(Debug)]
pub struct FullyQualifiedCallFrom2018<'db> {
    pub expr_or_pat: InFile<ExprOrPatPtr>,
    /// The picked item, whose trait the suggestion names.
    pub item: AssocItem,
    pub self_ty: Type<'db>,
}

#[derive(Debug)]
pub struct PatternArgInExternFn {
    pub node: InFile<AstPtr<ast::Pat>>,
}

#[derive(Debug)]
pub struct UnimplementedTrait<'db> {
    pub span: SpanSyntax,
    pub cause: ObligationCauseCode,
    pub trait_predicate: crate::TraitClause<'db>,
    pub parent_trait_predicates: Vec<crate::TraitClause<'db>>,
    /// For each of `parent_trait_predicates`, the impl that would prove it, whose where clause
    /// required the predicate nearer the error: rustc's "required for `T` to implement `Trait`"
    /// and "unsatisfied trait bound introduced here". `None` where it came from no impl.
    pub parent_impls: Vec<Option<crate::Impl>>,
    /// For each of `parent_impls`, the impl's bound the step went through (rustc's
    /// `impl_def_predicate_index`), where "unsatisfied trait bound introduced here" goes.
    pub parent_impl_bounds: Vec<Option<BoundSource>>,
}

/// Where a predicate of an item comes from, its written bound found in syntax: for rustc's
/// "required by this bound in `f`", "unsatisfied trait bound introduced here" and "the implicit
/// `Sized` requirement on this type parameter".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundSource {
    /// A bound written on a parameter, in a where clause, or an `impl Trait` argument's.
    Written(InFile<AstPtr<ast::TypeBound>>),
    /// A type parameter's implicit `Sized`.
    ImplicitSized(crate::TypeParam),
    /// A trait's own `Self: Trait`, which no one writes.
    ImpliedTrait(Trait),
    /// A const parameter's type.
    ConstParamType(crate::ConstParam),
}

impl GenericDef {
    /// The source of the item's `bound_index`th predicate, counted as
    /// [`ObligationCauseCode::WhereClauseInExpr`] counts them (parents first); `None` for one
    /// with no syntax.
    pub fn bound_source(self, db: &Db, bound_index: u32) -> Option<BoundSource> {
        bound_source(db, GenericDefId::try_from(self).ok()?, bound_index)
    }
}

pub(crate) fn bound_source(db: &Db, def: GenericDefId, bound_index: u32) -> Option<BoundSource> {
    use hir_def::hir::{
        generics::{GenericParams, WherePredicate},
        type_ref::TypeBound,
    };
    use hir_ty::PredicateSource;
    let sources = blaze_mem::frame::task(blaze_mem::Tag::Diagnostics, |_| {
        hir_ty::predicate_sources(db, def)
    });
    Some(match *sources.get(bound_index as usize)? {
        PredicateSource::ImpliedTrait(id) => BoundSource::ImpliedTrait(Trait { id }),
        PredicateSource::ImplicitSized(id) => BoundSource::ImplicitSized(crate::TypeParam { id }),
        PredicateSource::ConstParamType(id) => BoundSource::ConstParamType(crate::ConstParam { id }),
        PredicateSource::Written { def, index } => {
            let (params, _, source_map) = GenericParams::with_source_map(db, def);
            let bound = match *params.where_predicates().get(index as usize)? {
                WherePredicate::TypeBound {
                    bound: TypeBound::Path(path, _) | TypeBound::ForLifetime(_, path),
                    ..
                } => {
                    let ptr = source_map.type_syntax(path.type_ref()).ok()?;
                    let root = ptr.file_id.root(db);
                    ptr.map(|it| {
                        it.to_view(root).syntax().ancestors().find_map(view::TypeBound::cast)
                    })
                }
                WherePredicate::TypeBound { bound: TypeBound::Lifetime(lifetime), .. }
                | WherePredicate::Lifetime { bound: lifetime, .. } => {
                    let ptr = source_map.lifetime_syntax(lifetime).ok()?;
                    let root = ptr.file_id.root(db);
                    ptr.map(|it| {
                        it.to_view(root).syntax().ancestors().find_map(view::TypeBound::cast)
                    })
                }
                WherePredicate::TypeBound { bound: TypeBound::Use(_) | TypeBound::Error, .. } => {
                    return None;
                }
            };
            let file_id = bound.file_id;
            BoundSource::Written(InFile::new(file_id, AstPtr::of(bound.value?)))
        }
    })
}

/// Where a dyn-compatibility violation is written: rustc's `DynCompatibilityViolation::spans`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DynCompatibilitySite {
    Syntax(InFile<SyntaxNodePtr>),
    /// The `index`th item bound of an associated type (its own, then those the trait's where
    /// clauses put on it). hir can't point at one yet: the bounds are lowered with no record of
    /// which written bound each came from.
    AssocTypeBound { alias: TypeAlias, index: u32 },
}

impl Trait {
    /// rustc's `dyn_compatibility_violations` of the trait itself, each with its sites in rustc's
    /// order. A violation with none is one rustc gives no span: a `Sized` only elaborated, and
    /// [`crate::DynCompatibilityViolation::HasNonCompatibleSuperTrait`], whose trait has its
    /// own. The two `SelfReferential` violations stay apart as rustc's: predicates first, then
    /// the associated types' bounds.
    pub fn dyn_compatibility_violation_sites(
        self,
        db: &Db,
    ) -> Vec<(crate::DynCompatibilityViolation, Vec<DynCompatibilitySite>)> {
        use hir_ty::dyn_compatibility::{ViolationSite, dyn_compatibility_violation_sites};
        let site = |site: &ViolationSite| -> Option<DynCompatibilitySite> {
            let syntax = match *site {
                ViolationSite::Predicate { def, index } => match bound_source(db, def, index)? {
                    BoundSource::Written(bound) => bound.map(|it| it.syntax_node_ptr()),
                    BoundSource::ImplicitSized(_)
                    | BoundSource::ImpliedTrait(_)
                    | BoundSource::ConstParamType(_) => return None,
                },
                ViolationSite::AssocTypeBound { alias, index } => {
                    return Some(DynCompatibilitySite::AssocTypeBound {
                        alias: TypeAlias { id: alias },
                        index,
                    });
                }
                ViolationSite::Param { function, index } => param_syntax(db, function, index)?,
                ViolationSite::Output(function) => output_syntax(db, function)?,
                ViolationSite::Opaque { function, index } => {
                    opaque_syntax(db, function, index.into_raw().into_u32())?
                }
                ViolationSite::Name(item) => assoc_item_name(db, item)?,
            };
            Some(DynCompatibilitySite::Syntax(syntax))
        };
        blaze_mem::frame::task(blaze_mem::Tag::Diagnostics, |_| {
            dyn_compatibility_violation_sites(db, self.id)
        })
        .into_iter()
        .map(|(violation, sites)| {
            let sites = sites.iter().filter_map(site).collect(); // alloc: error path
            (violation, sites)
        })
        .collect() // alloc: error path, a dyn-incompatible trait reported
    }
}

/// The name of an associated item.
fn assoc_item_name(db: &Db, item: AssocItemId) -> Option<InFile<SyntaxNodePtr>> {
    fn name<'db, N: view::HasName<'db>>(source: InFile<N>) -> Option<InFile<SyntaxNodePtr>> {
        let name = source.value.name()?;
        Some(source.with_value(view::ptr(name.syntax())))
    }
    use hir_def::src::HasSource;
    match item {
        AssocItemId::FunctionId(id) => name(id.lookup(db).source_view(db)),
        AssocItemId::ConstId(id) => name(id.lookup(db).source_view(db)),
        AssocItemId::TypeAliasId(id) => name(id.lookup(db).source_view(db)),
    }
}

/// The type of `function`'s `index`th parameter, `self` counted (rustc's `decl.inputs[index]`);
/// for a `self` with no type written, the parameter.
fn param_syntax(db: &Db, function: FunctionId, index: u32) -> Option<InFile<SyntaxNodePtr>> {
    let (signature, source_map) = FunctionSignature::with_source_map(db, function);
    let typed = signature.params.get(index as usize).and_then(|&ty| source_map.type_syntax(ty).ok());
    if let Some(typed) = typed {
        return Some(typed.map(|it| it.syntax_node_ptr()));
    }
    // `&'a mut self` has no type written: rustc points at the parameter.
    if index != 0 || !signature.has_self_param() {
        return None;
    }
    let source = hir_def::src::HasSource::source_view(function.lookup(db), db);
    let param = source.value.param_list()?.self_param()?;
    Some(source.with_value(view::ptr(param.syntax())))
}

/// `function`'s written return type.
fn output_syntax(db: &Db, function: FunctionId) -> Option<InFile<SyntaxNodePtr>> {
    let (signature, source_map) = FunctionSignature::with_source_map(db, function);
    let ty = source_map.type_syntax(signature.ret_type?).ok()?;
    Some(ty.map(|it| it.syntax_node_ptr()))
}

/// The `index`th `impl Trait` of `function`'s return type, counted in the order lowering
/// allocates them (each before the ones in its bounds), as `ImplTraitId::ReturnTypeImplTrait`.
fn opaque_syntax(db: &Db, function: FunctionId, index: u32) -> Option<InFile<SyntaxNodePtr>> {
    use hir_def::{
        expr_store::StoreVisitor,
        hir::type_ref::{TypeRef, TypeRefId},
    };
    struct Find<'a> {
        store: &'a ExpressionStore,
        found: Vec<TypeRefId>,
    }
    impl StoreVisitor for Find<'_> {
        fn on_type(&mut self, ty: TypeRefId) {
            if let TypeRef::ImplTrait(_) = self.store.type_ref(ty) {
                self.found.push(ty);
            }
            let store = self.store;
            store.visit_type_ref_children(ty, self);
        }
    }
    let (signature, source_map) = FunctionSignature::with_source_map(db, function);
    // alloc: error path, the site of a reported error
    let mut find = Find { store: &signature.store, found: Vec::new() };
    if let Some(ret_type) = signature.ret_type {
        find.on_type(ret_type);
    }
    let store = &signature.store;
    let it = *find.found.get(index as usize)?;
    let ty = source_map.type_syntax(it).ok().or_else(|| {
        // An `async fn`'s future has no syntax: its output, the type the function says it
        // returns, has.
        let TypeRef::ImplTrait(bounds) = store.type_ref(it) else { return None };
        let [hir_def::hir::type_ref::TypeBound::Path(path, _)] = &bounds[..] else {
            return None;
        };
        let args = store.path(*path).generic_args()?.last()??;
        let output = args.bindings.first()?.type_ref?;
        source_map.type_syntax(output).ok()
    })?;
    Some(ty.map(|it| it.syntax_node_ptr()))
}

#[derive(Debug)]
pub struct ClosureKindMismatch {
    pub span: SpanSyntax,
    pub cause: ObligationCauseCode,
    pub expected: crate::FnTrait,
    pub found: crate::FnTrait,
    /// The closure, which rustc points at; `None` when it has no syntax.
    pub closure: Option<InFile<AstPtr<ast::ClosureExpr>>>,
}

#[derive(Debug)]
pub struct MutableRefBinding {
    pub pat: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct YieldOutsideCoroutine {
    pub expr: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct ReturnOutsideFunction {
    pub expr: InFile<ExprOrPatPtr>,
    pub kind: ReturnKind,
}

/// A const argument in a body that is not a constant, for `reason`: at its expression, or at
/// the type ref of a path written where a type could stand.
#[derive(Debug)]
pub struct InvalidConstArg {
    pub site: SpanSyntax,
    pub reason: CreateConstError,
}

/// A struct literal's or pattern's path that resolves to something it can't be (see
/// `StructPathFound` for rustc's error of each case). `ty` is the type found, exactly for
/// `StructPathFound::Ty`.
#[derive(Debug)]
pub struct NotAStructPath<'db> {
    pub node: InFile<ExprOrPatPtr>,
    pub found: StructPathFound,
    pub ty: Option<Type<'db>>,
}

/// E0055 "reached the recursion limit while auto-dereferencing `{ty}`", with rustc's help to
/// add `#![recursion_limit = "{suggested_limit}"]`.
#[derive(Debug)]
pub struct AutoderefRecursionLimit<'db> {
    pub site: SpanSyntax,
    pub ty: Type<'db>,
    pub suggested_limit: u32,
}

/// E0282 "type annotations needed" of a method call on a raw pointer to an unknown pointee,
/// labelled "cannot call a method on a raw pointer with an unknown pointee type".
#[derive(Debug)]
pub struct MethodOnUnknownRawPointee {
    pub site: SpanSyntax,
}

/// `tyvar_behind_raw_pointer` "type annotations needed" of a method call on a raw pointer to an
/// unknown pointee in edition 2015, a hard error from 2018.
#[derive(Debug)]
pub struct TyvarBehindRawPointer {
    pub site: SpanSyntax,
}

/// E0308 "mismatched types" at a `&mut` pattern's prefix matching an inherited shared
/// reference: noted "cannot match inherited `&` with `&mut` pattern", suggested "replace this
/// `&mut` pattern with `&`".
#[derive(Debug)]
pub struct InheritedRefMutabilityMismatch {
    pub pat: InFile<ExprOrPatPtr>,
}

/// E0059 "cannot use call notation; the first type parameter for the function trait is
/// neither a tuple nor unit", at the call.
#[derive(Debug)]
pub struct FnTraitArgsNotTuple {
    pub call: InFile<ExprOrPatPtr>,
}

/// "the `Self` constructor can only be used with tuple or unit structs", at a `Self`
/// expression whose impl's `self_ty` has no constructor.
#[derive(Debug)]
pub struct SelfCtorNotTupleOrUnit<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub self_ty: Type<'db>,
}

/// rustc's two errors of `check_expr_unsafe_binder_cast`: for `Wrap`, "`wrap_binder!()` can
/// only wrap into unsafe binder, not {found}" (note "unsafe binders are the only valid output
/// of wrap"), at the written type or else the cast; for `Unwrap`, "expected unsafe binder,
/// found {found} as input of `unwrap_binder!()`" (note "only an unsafe binder type can be
/// unwrapped"). `expr` is the cast.
#[derive(Debug)]
pub struct UnsafeBinderCastOfNonBinder<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub kind: UnsafeBinderCastKind,
    pub found: Type<'db>,
}

/// rustc's `check_param_wf` of a const parameter's type: E0741 with `adt_const_params`, its
/// featureless errors without; `reason` picks the text.
#[derive(Debug)]
pub struct ConstParamTyNotAllowed<'db> {
    pub param: InFile<AstPtr<ast::ConstParam>>,
    pub ty: Type<'db>,
    pub reason: ConstParamTyReason,
}

/// A const argument whose value has no value tree (rustc's `eval_to_valtree`), at the
/// argument. `konst` is `None` for an anonymous constant, which rustc names
/// `{owner}::{constant#n}`.
#[derive(Debug)]
pub struct ValTreeError<'db> {
    pub at: InFile<SyntaxNodePtr>,
    pub konst: Option<crate::DefWithBody>,
    pub error: ValTreeErrorKind<'db>,
}

#[derive(Debug)]
pub enum ValTreeErrorKind<'db> {
    /// "maximum number of nodes exceeded in constant {path}".
    NodesOverflow,
    /// A value of this type, met inside the constant, has no value tree.
    NonSupportedType(Type<'db>),
    /// The evaluated bytes are not a value of `ty`: `what`. No rustc counterpart.
    Unreadable { ty: Type<'db>, what: &'static str },
}

pub(crate) struct DiagnosticsCollector<'a, 'db, F: ?Sized> {
    db: &'db Db,
    krate: base_db::Crate,
    edition: Edition,
    style_lints: bool,
    acc: &'a mut Vec<AnyDiagnostic<'db>>,
    /// The bodies whose diagnostics are asked for ([`crate::check::every_body`] for all).
    bodies: &'a F,
    /// The bodies a plan checks as tasks of their own, which the walk of a module leaves
    /// ([`crate::check::bodies`]).
    deferred: Option<crate::check::bodies::Defer<'a>>,
}

/// What a [`MacroError`] keeps of a builtin error besides its text: its code, its other primary
/// spans and its notes and helps. Nothing for any other error.
fn builtin_parts(
    db: &Db,
    err: &ExpandErrorKind,
) -> (Option<u16>, Box<[InFile<TextRange>]>, Box<[(BuiltinChild, Box<str>)]>) {
    let ExpandErrorKind::Builtin(builtin) = err else {
        return (None, Box::default(), Box::default());
    };
    let also_at = builtin
        .also_at
        .iter()
        .map(|&span| {
            let at = hir_expand::resolve_span(db, span);
            InFile::new(HirFileId::from(at.file_id), at.range)
        })
        // alloc: an error's few spans, on the error path
        .collect();
    // clone: an error's few lines, on the error path
    (builtin.code, also_at, builtin.children.clone())
}

fn precise_macro_call_location(
    ast: &MacroCallKind,
    db: &Db,
    krate: base_db::Crate,
) -> InFile<TextRange> {
    // FIXME: maybe we actually want slightly different ranges for the different macro diagnostics
    // - e.g. the full attribute for macro errors, but only the name for name resolution
    match ast {
        MacroCallKind::FnLike { ast_id, .. } => {
            let node = ast_id.to_view(db);
            let range = node
                .path()
                .and_then(|it| it.segment())
                .and_then(|it| it.name_ref())
                .map(|it| it.syntax().text_range());
            let range = range.unwrap_or_else(|| node.syntax().text_range());
            ast_id.with_value(range)
        }
        MacroCallKind::Derive { ast_id, derive_attr_index, derive_index, .. } => {
            let range = derive_attr_index.find_derive_range(db, krate, *ast_id, *derive_index);
            ast_id.with_value(range)
        }
        MacroCallKind::Attr { ast_id, censored_attr_ids: attr_ids, .. } => {
            let attr_range = attr_ids
                .invoc_attr()
                .find_attr_range_view(db, krate, *ast_id)
                .1
                .syntax()
                .text_range();
            ast_id.with_value(attr_range)
        }
    }
}

impl<'a, 'db, F: Fn(crate::DefWithBody) -> bool + ?Sized> DiagnosticsCollector<'a, 'db, F> {
    pub(crate) fn collect(
        db: &'db Db,
        module: ModuleId,
        acc: &'a mut Vec<AnyDiagnostic<'db>>,
        style_lints: bool,
        bodies: &'a F,
    ) {
        Self::collect_with(db, module, acc, style_lints, bodies, None);
    }

    /// [`DiagnosticsCollector::collect`] of a module whose bodies listed in `deferred` are
    /// checked as tasks of their own: each is left where the walk meets it, its place marked.
    pub(crate) fn collect_deferred(
        db: &'db Db,
        module: ModuleId,
        acc: &'a mut Vec<AnyDiagnostic<'db>>,
        style_lints: bool,
        bodies: &'a F,
        deferred: crate::check::bodies::Defer<'a>,
    ) {
        Self::collect_with(db, module, acc, style_lints, bodies, Some(deferred));
    }

    fn collect_with(
        db: &'db Db,
        module: ModuleId,
        acc: &'a mut Vec<AnyDiagnostic<'db>>,
        style_lints: bool,
        bodies: &'a F,
        deferred: Option<crate::check::bodies::Defer<'a>>,
    ) {
        let krate = module.krate(db);
        let edition = krate.data(db).edition;
        DiagnosticsCollector { db, krate, edition, style_lints, acc, bodies, deferred }
            .collect_module(module);
    }

    /// The diagnostics of body `def`, whose types are written in `type_owner`'s generics, with
    /// those of the items inside it that `bodies` accepts: what the walk of its module gives
    /// where it meets the body.
    pub(crate) fn collect_body(
        db: &'db Db,
        def: DefWithBodyId,
        type_owner: TypeOwnerId,
        acc: &'a mut Vec<AnyDiagnostic<'db>>,
        style_lints: bool,
        bodies: &'a F,
    ) {
        let krate = hir_def::HasModule::krate(&def, db);
        let edition = krate.data(db).edition;
        DiagnosticsCollector { db, krate, edition, style_lints, acc, bodies, deferred: None }
            .check_body(def, type_owner);
    }

    fn emit_def_diagnostic(&mut self, diag: &DefDiagnosticKind) {
        match diag {
            DefDiagnosticKind::UnresolvedModule { ast: declaration, candidates } => {
                let decl = declaration.to_ptr(self.db);
                self.acc.push(
                    UnresolvedModule {
                        decl: InFile::new(declaration.file_id, decl),
                        // clone: error path, a reported diagnostic
                        candidates: candidates.clone(),
                    }
                    .into(),
                )
            }
            DefDiagnosticKind::UnresolvedExternCrate { ast } => {
                let item = ast.to_ptr(self.db);
                self.acc
                    .push(UnresolvedExternCrate { decl: InFile::new(ast.file_id, item) }.into());
            }

            DefDiagnosticKind::MacroError { ast, path: _, err } => {
                let item = ast.to_ptr(self.db);
                let RenderedExpandError { message, error, kind } = err.render_to_string(self.db);
                let (code, also_at, children) = builtin_parts(self.db, err);
                self.acc.push(
                    MacroError {
                        range: InFile::new(ast.file_id, item.text_range()),
                        message,
                        error,
                        kind,
                        code,
                        also_at,
                        children,
                    }
                    .into(),
                )
            }
            DefDiagnosticKind::UnresolvedImport { id, index } => {
                let file_id = id.file_id;

                let use_tree = hir_def::src::use_tree_to_view(self.db, *id, *index);
                self.acc.push(
                    UnresolvedImport { decl: InFile::new(file_id, AstPtr::of(use_tree)) }.into(),
                );
            }
            DefDiagnosticKind::TraitItemImport { id, index, why } => {
                let use_tree = hir_def::src::use_tree_to_view(self.db, *id, *index);
                let decl = InFile::new(id.file_id, AstPtr::of(use_tree));
                self.acc.push(TraitItemImport { decl, why: *why }.into());
            }
            DefDiagnosticKind::Prelude { krate, error } => {
                self.acc.push(MissingPrelude { krate: *krate, error: *error }.into());
            }

            DefDiagnosticKind::UnconfiguredCode { ast_id, cfg } => {
                let ast_id_map = ast_id.file_id.ast_id_map(self.db);
                let ptr = ast_id_map.get_erased(ast_id.value);
                self.acc.push(
                    InactiveCode {
                        node: InFile::new(ast_id.file_id, ptr),
                        // clone: error path, a reported diagnostic
                        cfg: cfg.clone(),
                        // clone: error path, a reported diagnostic
                        opts: self.krate.cfg_options(self.db).clone(),
                    }
                    .into(),
                );
            }
            DefDiagnosticKind::UnresolvedMacroCall { ast, path } => {
                let location = precise_macro_call_location(ast, self.db, self.krate);
                self.acc.push(
                    UnresolvedMacroCall {
                        range: location,
                        // clone: error path, a reported diagnostic
                        path: path.clone(),
                        is_bang: matches!(ast, MacroCallKind::FnLike { .. }),
                    }
                    .into(),
                );
            }
            DefDiagnosticKind::UnimplementedBuiltinMacro { ast } => {
                let node = ast.to_view(self.db);
                // Must have a name, otherwise we wouldn't emit it.
                let name = node.name().expect("unimplemented builtin macro with no name");
                self.acc.push(
                    UnimplementedBuiltinMacro { node: ast.with_value(view::ptr(name.syntax())) }
                        .into(),
                );
            }
            DefDiagnosticKind::InvalidDeriveTarget { ast, id } => {
                let (_, attr) = id.find_attr_range_view(self.db, self.krate, *ast);
                let derive = attr
                    .path()
                    .map(|path| path.syntax().text_range())
                    .unwrap_or_else(|| attr.syntax().text_range());
                self.acc.push(InvalidDeriveTarget { range: ast.with_value(derive) }.into());
            }
            DefDiagnosticKind::MalformedDerive { ast, id } => {
                let derive =
                    id.find_attr_range_view(self.db, self.krate, *ast).1.syntax().text_range();
                self.acc.push(MalformedDerive { range: ast.with_value(derive) }.into());
            }
            DefDiagnosticKind::MacroDefError { ast, message } => {
                let node = ast.to_view(self.db);
                self.acc.push(
                    MacroDefError {
                        node: InFile::new(ast.file_id, AstPtr::of(node)),
                        name: node.name().map(|it| it.syntax().text_range()),
                        // clone: error path, a reported diagnostic
                        message: message.clone(),
                    }
                    .into(),
                );
            }
        }
    }

    fn emit_def_diagnostics(&mut self, diagnostics: &DefDiagnostics) {
        diagnostics.iter().for_each(|diag| self.emit_def_diagnostic(&diag.kind));
    }

    fn emit_case_diagnostics(&mut self, def: ModuleDefId) {
        self.acc
            .extend(hir_ty::diagnostics::incorrect_case(self.db, def).into_iter().map(Into::into));
    }

    fn collect_macro_call(&mut self, macro_call_id: MacroCallId) {
        let Some(e) = macro_call_id.parse_macro_expansion_error(self.db) else {
            return;
        };
        let ValueResult { value: parse_errors, err } = e;
        if let Some(err) = err {
            let loc = macro_call_id.loc(self.db);
            let file_id = loc.kind.file_id();
            let mut range = precise_macro_call_location(&loc.kind, self.db, loc.krate);
            let RenderedExpandError { message, error, kind } = err.render_to_string(self.db);
            if Some(err.span().anchor.file_id)
                == file_id.file_id().map(|it| it.span_file_id(self.db))
            {
                range.value = err.span().range
                    + file_id
                        .ast_id_map(self.db)
                        .get_erased(err.span().anchor.ast_id)
                        .text_range()
                        .start();
            }
            let (code, also_at, children) = builtin_parts(self.db, err.kind());
            self.acc.push(
                MacroError { range, message, error, kind, code, also_at, children }.into(),
            );
        }

        if !parse_errors.is_empty() {
            let loc = macro_call_id.loc(self.db);
            let range = precise_macro_call_location(&loc.kind, self.db, loc.krate);
            // clone: error path, a reported diagnostic
            self.acc.push(MacroExpansionParseError { range, errors: parse_errors.clone() }.into())
        }
    }

    fn collect_assoc_items(&mut self, defs: &[(Name, AssocItemId)], def_map: &DefMap) {
        for &(_, def) in defs {
            self.collect_module_def(def.into(), def_map);
        }
    }

    fn collect_trait(&mut self, def: TraitId, def_map: &DefMap) {
        let items = TraitItems::query_with_diagnostics(self.db, def);

        self.collect_generic_def(def.into());
        self.collect_wfcheck(def.into());
        self.emit_def_diagnostics(&items.1);
        items.0.macro_calls().for_each(|(_, call)| self.collect_macro_call(call));
        self.collect_assoc_items(&items.0.items, def_map);
    }

    fn collect_impl(
        &mut self,
        def: ImplId,
        infcx: &InferCtxt<'db>,
        def_map: &'db DefMap,
        impl_assoc_items_scratch: &mut Vec<(Name, AssocItemId)>,
    ) {
        let impl_signature = ImplSignature::of(self.db, def);
        let impl_items = ImplItems::of(self.db, def);

        self.collect_generic_def(def.into());
        self.emit_def_diagnostics(&impl_items.1);
        impl_items.0.macro_calls().for_each(|(_, call)| self.collect_macro_call(call));
        self.collect_assoc_items(&impl_items.0.items, def_map);
        for &(_, item) in &impl_items.0.items {
            use hir_def::src::HasSource as _;
            use syntax::view::HasVisibility as _;
            let db = self.db;
            match item {
                AssocItemId::FunctionId(it) => {
                    self.collect_visibility(it, it.lookup(db).source_view(db).map(|it| it.visibility()))
                }
                AssocItemId::ConstId(it) => {
                    self.collect_visibility(it, it.lookup(db).source_view(db).map(|it| it.visibility()))
                }
                AssocItemId::TypeAliasId(it) => {
                    self.collect_visibility(it, it.lookup(db).source_view(db).map(|it| it.visibility()))
                }
            }
        }

        let loc = def.lookup(self.db);

        let file_id = loc.id.file_id;
        if file_id.macro_file().is_some_and(|it| it.kind(self.db) == MacroKind::DeriveBuiltIn) {
            // these expansion come from us, diagnosing them is a waste of resources
            // FIXME: Once we diagnose the inputs to builtin derives, we should at least extract those diagnostics somehow
            return;
        }
        self.collect_wfcheck(def.into());

        let ast_id_map = file_id.ast_id_map(self.db);

        let trait_impl = impl_signature.target_trait.is_some();
        if !trait_impl && !is_inherent_impl_coherent(self.db, def_map, def) {
            self.acc.push(IncoherentImpl { impl_: ast_id_map.get(loc.id.value), file_id }.into())
        }

        if trait_impl && !check_orphan_rules(self.db, def) {
            self.acc.push(TraitImplOrphan { impl_: ast_id_map.get(loc.id.value), file_id }.into())
        }

        let trait_ = trait_impl
            .then(|| self.db.impl_trait(def))
            .flatten()
            .map(|trait_ref| trait_ref.instantiate_identity().skip_norm_wip().def_id.0);
        let mut trait_is_unsafe = trait_.is_some_and(|trait_| {
            TraitSignature::of(self.db, trait_).flags.contains(TraitFlags::UNSAFE)
        });
        let impl_is_negative = impl_signature.is_negative();
        let impl_is_unsafe = impl_signature.flags.contains(ImplFlags::UNSAFE);

        let trait_is_unresolved = trait_.is_none() && trait_impl;
        if trait_is_unresolved {
            // Ignore trait safety errors when the trait is unresolved, as otherwise we'll treat it as safe,
            // which may not be correct.
            trait_is_unsafe = impl_is_unsafe;
        }

        // rustc's `unsafe_attr`: a `#[may_dangle]` parameter of the impl, whatever its trait.
        let maybe_dangle = trait_.is_some() && {
            let (lifetimes_attrs, type_and_consts_attrs) =
                AttrFlags::query_generic_params(self.db, def.into());
            lifetimes_attrs.values().any(|it| it.contains(AttrFlags::MAY_DANGLE))
                || type_and_consts_attrs.values().any(|it| it.contains(AttrFlags::MAY_DANGLE))
        };

        match (impl_is_unsafe, trait_is_unsafe, impl_is_negative, maybe_dangle) {
                // unsafe negative impl
                (true, _, true, _) |
                // unsafe impl for safe trait
                (true, false, _, false) => self.acc.push(TraitImplIncorrectSafety { impl_: ast_id_map.get(loc.id.value), file_id, should_be_safe: true }.into()),
                // safe impl for unsafe trait
                (false, true, false, _) |
                // safe positive impl with a `#[may_dangle]` parameter
                (false, false, false, true) => self.acc.push(TraitImplIncorrectSafety { impl_: ast_id_map.get(loc.id.value), file_id, should_be_safe: false }.into()),
                _ => (),
            };

        // Negative impls can't have items, don't emit missing items diagnostic for them
        if let (false, Some(trait_)) = (impl_is_negative, trait_) {
            let trait_items = &trait_.trait_items(self.db).items;
            let required_items = trait_items.iter().filter(|&(_, assoc)| match *assoc {
                AssocItemId::FunctionId(it) => !FunctionSignature::of(self.db, it).has_body(),
                AssocItemId::ConstId(id) => !ConstSignature::of(self.db, id).has_body(),
                AssocItemId::TypeAliasId(it) => TypeAliasSignature::of(self.db, it).ty.is_none(),
            });
            impl_assoc_items_scratch.extend(impl_items.0.items.iter().cloned());

            let redundant = impl_assoc_items_scratch
                .iter()
                .filter(|(name, id)| {
                    !trait_items.iter().any(|(impl_name, impl_item)| {
                        discriminant(impl_item) == discriminant(id) && impl_name == name
                    })
                })
                .map(|(name, item)| (*name, AssocItem::from(*item)));
            for (name, assoc_item) in redundant {
                self.acc.push(
                    TraitImplRedundantAssocItems {
                        trait_: trait_.into(),
                        file_id,
                        impl_: ast_id_map.get(loc.id.value),
                        assoc_item: (name, assoc_item),
                    }
                    .into(),
                )
            }

            let implemented_here = |name: &Name, id: &AssocItemId| {
                impl_assoc_items_scratch.iter().any(|(impl_name, impl_item)| {
                    discriminant(impl_item) == discriminant(id) && impl_name == name
                })
            };
            let mut missing: Vec<_> = required_items
                .filter(|(name, id)| !implemented_here(name, id))
                .map(|(name, item)| (*name, AssocItem::from(*item)))
                // alloc: empty, so no allocation, unless items are missing: then reported
                .collect();

            // rustc's `eval_default_body_stability` of each item the impl leaves to the trait: a
            // default body `#[rustc_default_body_unstable]` gates is refused to another crate
            // without its feature; `Drop::drop`'s, without `pin_ergonomics`, as a missing item.
            if hir_def::HasModule::krate(&trait_, self.db) != self.krate {
                use hir_def::attrs::stability::{StabilityLevel, default_body_stability};
                let features = UnstableFeatures::query(self.db, self.krate);
                let is_drop =
                    hir_def::lang_item::lang_items(self.db, self.krate).Drop == Some(trait_);
                for &(name, id) in trait_items.iter() {
                    if implemented_here(&name, &id) {
                        continue;
                    }
                    let def = match id {
                        AssocItemId::FunctionId(it) => hir_def::AttrDefId::from(it),
                        AssocItemId::ConstId(it) => it.into(),
                        AssocItemId::TypeAliasId(it) => it.into(),
                    };
                    let Some(stability) = default_body_stability(self.db, def) else {
                        continue;
                    };
                    let StabilityLevel::Unstable { reason, issue, .. } = &stability.level else {
                        continue;
                    };
                    if features.is_enabled(&stability.feature) {
                        continue;
                    }
                    if is_drop
                        && name == intern::sym::drop
                        && !features.is_enabled(&intern::Symbol::intern("pin_ergonomics"))
                    {
                        if !missing.iter().any(|(it, _)| *it == name) {
                            missing.push((name, AssocItem::from(id)));
                        }
                        continue;
                    }
                    self.acc.push(
                        TraitImplUnstableDefaultBody {
                            file_id,
                            impl_: ast_id_map.get(loc.id.value),
                            item: name,
                            feature: stability.feature.clone(),
                            reason: reason.clone(),
                            issue: *issue,
                        }
                        .into(),
                    );
                }
            }

            if !missing.is_empty() {
                let env = ParamEnvAndCrate {
                    param_env: self.db.trait_environment(def.into()),
                    krate: self.krate,
                };
                let self_ty = self.db.impl_self_ty(def).instantiate_identity().skip_norm_wip();
                let self_ty = structurally_normalize_ty(infcx, self_ty, env.param_env);
                let tail_ty = struct_tail_raw(self.db, infcx.interner, self_ty, |ty| {
                    structurally_normalize_ty(infcx, ty, env.param_env)
                });
                let self_ty_is_guaranteed_unsized =
                    matches!(tail_ty.kind(), TyKind::Dynamic(..) | TyKind::Slice(..) | TyKind::Str);
                if self_ty_is_guaranteed_unsized {
                    missing.retain(|(_, assoc_item)| {
                            let assoc_item = match *assoc_item {
                                AssocItem::Function(it) => match it.id {
                                    AnyFunctionId::FunctionId(id) => id.into(),
                                    AnyFunctionId::BuiltinDeriveImplMethod { .. } => {
                                        never!("should not have an `AnyFunctionId::BuiltinDeriveImplMethod` here");
                                        return false;
                                    },
                                },
                                AssocItem::Const(it) => it.id.into(),
                                AssocItem::TypeAlias(it) => it.id.into(),
                            };
                            !hir_ty::dyn_compatibility::generics_require_sized_self(self.db, assoc_item)
                        });
                }
            }

            // HACK: When specialization is enabled in the current crate, and there exists
            // *any* blanket impl that provides a default implementation for the missing item,
            // suppress the missing associated item diagnostic.
            // This can lead to false negatives when the impl in question does not actually
            // specialize that blanket impl, but determining the exact specialization
            // relationship here would be significantly more expensive.
            if !missing.is_empty() {
                let features = UnstableFeatures::query(self.db, self.krate);
                if features.specialization || features.min_specialization {
                    missing.retain(|(assoc_name, assoc_item)| {
                        let AssocItem::Function(_) = assoc_item else {
                            return true;
                        };

                        for &impl_ in
                            TraitImpls::for_crate(self.db, self.krate).blanket_impls(trait_)
                        {
                            if impl_ == def {
                                continue;
                            }

                            for (name, item) in &impl_.impl_items(self.db).items {
                                let AssocItemId::FunctionId(fn_) = item else {
                                    continue;
                                };
                                if name != assoc_name {
                                    continue;
                                }

                                if FunctionSignature::of(self.db, *fn_).is_default() {
                                    return false;
                                }
                            }
                        }

                        true
                    });
                }
            }

            if !missing.is_empty() {
                self.acc.push(
                    TraitImplMissingAssocItems {
                        impl_: ast_id_map.get(loc.id.value),
                        file_id,
                        missing,
                    }
                    .into(),
                )
            }
            impl_assoc_items_scratch.clear();
        }
    }

    fn collect_module(&mut self, def: ModuleId) {
        let _p = tracing::info_span!("diagnostics", name = ?def.name(self.db)).entered();

        let def_map = def.def_map(self.db);
        let scope = def_map.module(self.db, def).scope(self.db);

        for diag in def_map.diagnostics_of(self.db, def) {
            self.emit_def_diagnostic(&diag.kind);
        }

        if !def.is_block_module(self.db) {
            // These are reported by the body of block modules
            scope.all_macro_calls().for_each(|call| self.collect_macro_call(call));
        }

        scope
            .declarations()
            .chain(scope.unnamed_consts().map(ModuleDefId::ConstId))
            .for_each(|def| self.collect_module_def(def, def_map));
        self.collect_visibilities(def, def_map, scope);

        scope.legacy_macros().flat_map(|(_, it)| it).for_each(|&def| {
            self.emit_case_diagnostics(def.into());
            self.collect_macro_def(def);
        });

        if scope.impls().len() == 0 {
            return;
        }
        let interner = DbInterner::new_with(self.db, self.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        // alloc: one scratch a module, reused by each of its impls
        let mut impl_assoc_items_scratch = Vec::new();
        scope.impls().for_each(|def| {
            impl_assoc_items_scratch.clear();
            self.collect_impl(def, &infcx, def_map, &mut impl_assoc_items_scratch)
        });
    }

    /// rustc's errors for the visibilities written on the items `module` declares, its imports
    /// and extern crates, resolved in `module` as the def collectors resolve them.
    fn collect_visibilities(
        &mut self,
        module: ModuleId,
        def_map: &DefMap,
        scope: &hir_def::item_scope::ItemScope,
    ) {
        use hir_def::src::HasSource as _;
        use syntax::view::HasVisibility as _;
        let db = self.db;
        for def in scope.declarations().chain(scope.unnamed_consts().map(ModuleDefId::ConstId)) {
            let vis = match def {
                ModuleDefId::FunctionId(it) => it.lookup(db).source_view(db).map(|it| it.visibility()),
                ModuleDefId::ConstId(it) => it.lookup(db).source_view(db).map(|it| it.visibility()),
                ModuleDefId::StaticId(it) => it.lookup(db).source_view(db).map(|it| it.visibility()),
                ModuleDefId::TypeAliasId(it) => {
                    it.lookup(db).source_view(db).map(|it| it.visibility())
                }
                ModuleDefId::TraitId(it) => it.lookup(db).source_view(db).map(|it| it.visibility()),
                ModuleDefId::AdtId(AdtId::StructId(it)) => {
                    it.lookup(db).source_view(db).map(|it| it.visibility())
                }
                ModuleDefId::AdtId(AdtId::UnionId(it)) => {
                    it.lookup(db).source_view(db).map(|it| it.visibility())
                }
                ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                    it.lookup(db).source_view(db).map(|it| it.visibility())
                }
                ModuleDefId::MacroId(MacroId::Macro2Id(it)) => {
                    it.lookup(db).source_view(db).map(|it| it.visibility())
                }
                ModuleDefId::ModuleId(it) => {
                    match def_map.module(db, it).declaration_source_view(db) {
                        Some(source) => source.map(|it| it.visibility()),
                        None => continue,
                    }
                }
                ModuleDefId::MacroId(MacroId::MacroRulesId(_) | MacroId::ProcMacroId(_))
                | ModuleDefId::EnumVariantId(_)
                | ModuleDefId::BuiltinType(_) => continue,
            };
            self.collect_visibility(module, vis);
        }
        for it in scope.use_decls() {
            self.collect_visibility(module, it.lookup(db).source_view(db).map(|it| it.visibility()));
        }
        for it in scope.extern_crate_decls() {
            self.collect_visibility(module, it.lookup(db).source_view(db).map(|it| it.visibility()));
        }
    }

    /// rustc's error for `vis`, written on an item whose visibility `scope` resolves.
    fn collect_visibility(
        &mut self,
        scope: impl hir_def::resolver::HasResolver,
        vis: InFile<Option<view::Visibility<'_>>>,
    ) {
        let (file_id, Some(node)) = (vis.file_id, vis.value) else { return };
        let Some(error) = hir_def::visibility::visibility_error_from_ast_view(self.db, scope, vis)
        else {
            return;
        };
        self.acc.push(InvalidVisibility { node: InFile::new(file_id, AstPtr::of(node)), error }.into());
    }

    fn collect_macro_def(&mut self, def: MacroId) {
        let id = def.definition(self.db);
        if let hir_expand::MacroDefKind::Declarative(ast, _) = id.kind
            && let expander = ast.decl_macro_expander(self.db, id.krate)
            && let Some(e) = expander.mac.err()
        {
            self.emit_def_diagnostic(&DefDiagnosticKind::MacroDefError {
                ast,
                // alloc: error path, a reported diagnostic's message
                message: e.to_string(),
            });
        }
    }

    fn collect_anon_const(&mut self, source_map: &ExpressionStoreSourceMap, def: AnonConstId<'db>) {
        // An inline `const { .. }` was inferred with its body, whose errors are reported there.
        if !def.is_inline_const(self.db) {
            self.emit_inference_errors(
                def.into(),
                source_map,
                TypeOwnerId::from_anon_const(def, self.db),
            );
        }
        self.collect_anon_const_borrowck(source_map, def);
        self.collect_anon_const_const_check(source_map, def);
        self.collect_anon_const_valtree(source_map, def);
    }

    /// rustc's const checks of `def` as a `const` of its own (an array length, a repeat count,
    /// a const argument or default, a `const { .. }` block), whatever body it is written in.
    fn collect_anon_const_const_check(
        &mut self,
        source_map: &ExpressionStoreSourceMap,
        def: AnonConstId<'db>,
    ) {
        let db = self.db;
        for diagnostic in &hir_ty_mir::mir::const_check::anon_const_check(db, def).diagnostics {
            let Some(node) = anon_const_node(db, source_map, def, diagnostic.span) else {
                self.acc.push(
                    UnplacedDiagnostic {
                        owner: unplaced_owner(db, def.into(), source_map),
                        what: Unplaced::ConstCheck {
                            code: diagnostic.code,
                            // clone: error path, a reported const check error
                            message: diagnostic.message.clone(),
                        },
                    }
                    .into(),
                );
                continue;
            };
            self.acc.push(
                ConstCheckError {
                    node,
                    code: diagnostic.code,
                    // clone: error path, a reported const check error
                    message: diagnostic.message.clone(),
                    // clone: error path, a reported const check error
                    kind: diagnostic.kind.clone(),
                    context: diagnostic.context,
                }
                .into(),
            );
        }
    }

    fn collect_anon_const_valtree(
        &mut self,
        source_map: &ExpressionStoreSourceMap,
        def: AnonConstId<'db>,
    ) {
        use hir_ty::const_valtree::{ValTreeError as Error, anon_const_valtree_error};
        let Some(error) = anon_const_valtree_error(self.db, def) else { return };
        let loc = def.loc(self.db);
        let Ok(at) = source_map.expr_syntax(loc.expr) else {
            never!("{def:?}: an anonymous constant with no syntax has no value tree");
            return;
        };
        let owner = loc.owner.generic_def(self.db);
        let error = match error {
            Error::NodesOverflow => ValTreeErrorKind::NodesOverflow,
            Error::NonSupportedType(ty) => {
                ValTreeErrorKind::NonSupportedType(Type::new(self.db, owner, ty.as_ref()))
            }
            Error::Unreadable { ty, what } => ValTreeErrorKind::Unreadable {
                ty: Type::new(self.db, owner, ty.as_ref()),
                what,
            },
        };
        self.acc.push(
            ValTreeError { at: at.map(|it| it.syntax_node_ptr()), konst: None, error }.into(),
        );
    }

    fn collect_anon_const_borrowck(
        &mut self,
        source_map: &ExpressionStoreSourceMap,
        def: AnonConstId<'db>,
    ) {
        use hir_ty_mir::mir::MirSpan;
        let db = self.db;
        let result = hir_ty_borrowck::borrowck_anon_const(db, def);
        let node_of = |span: MirSpan| anon_const_node(db, source_map, def, span);
        for diagnostic in &result.diagnostics {
            let Some(node) = node_of(diagnostic.span) else {
                self.acc.push(
                    UnplacedDiagnostic {
                        owner: unplaced_owner(self.db, def.into(), source_map),
                        what: Unplaced::Borrowck {
                            code: diagnostic.code,
                            // clone: error path, a reported borrowck error
                            message: diagnostic.message.clone(),
                        },
                    }
                    .into(),
                );
                continue;
            };
            let (labels, children, suggestions) = borrowck_parts(self.db, diagnostic, &node_of);
            self.acc.push(
                BorrowckError {
                    node,
                    at_end: matches!(diagnostic.span, MirSpan::ExprEnd(_)),
                    code: diagnostic.code,
                    // clone: error path, a reported borrowck error
                    message: diagnostic.message.clone(),
                    labels,
                    children,
                    suggestions,
                }
                .into(),
            );
        }
    }

    fn collect_enum(&mut self, def: EnumId) {
        self.collect_only_generic_def(def);
        self.collect_wfcheck(def.into());

        let variants = def.enum_variants_with_diagnostics(self.db);
        variants.0.variants.values().for_each(|&(def, _)| self.collect_enum_variant(def));

        let file = def.lookup(self.db).id.file_id;
        let ast_id_map = file.ast_id_map(self.db);
        for diag in &variants.1 {
            self.acc.push(
                InactiveCode {
                    node: InFile::new(file, ast_id_map.get(diag.ast_id).syntax_node_ptr()),
                    // clone: error path, a reported diagnostic
                    cfg: diag.cfg.clone(),
                    // clone: error path, a reported diagnostic
                    opts: diag.opts.clone(),
                }
                .into(),
            );
        }
    }

    fn collect_enum_variant(&mut self, def: EnumVariantId) {
        self.collect_def_with_body(
            def.into(),
            TypeOwnerId::GenericDefId(def.loc(self.db).parent.into()),
        );
        self.collect_variant(def.into());
    }

    fn collect_anon_consts_and_ty_diagnostics(
        &mut self,
        source_map: &ExpressionStoreSourceMap,
        anon_consts: &[AnonConstId<'db>],
        diagnostics: &[TyLoweringDiagnostic],
    ) {
        anon_consts.iter().for_each(|&anon_const| self.collect_anon_const(source_map, anon_const));

        diagnostics
            .iter()
            .filter_map(|diag| AnyDiagnostic::ty_diagnostic(diag, source_map, self.db))
            .for_each(|diag| self.acc.push(diag));
    }

    /// The checks of `def`'s signature. Like [`Self::check_body`], they read the signature and
    /// what was found in it without positions, and ask for its source map, which an edit
    /// before the item in its file makes again, only when there is a diagnostic to place.
    fn collect_generic_def(&mut self, def: GenericDefId) {
        let db = self.db;
        let store = ExpressionStore::of(db, def.into());
        self.collect_blocks(store);
        let lowered = signature_anon_consts_and_diagnostics(db, def);
        let reports = signature_store_has_diagnostics(db, def)
            || store_macro_failed(db, store)
            || lowered.iter().any(|&(anon_consts, diagnostics)| {
                !diagnostics.is_empty()
                    || anon_consts.iter().any(|&it| self.anon_const_reports(it))
            });
        if reports {
            let (_, source_map) = ExpressionStore::with_source_map(db, def.into());
            self.collect_store_diagnostics(source_map);
            for (anon_consts, diagnostics) in lowered {
                self.collect_anon_consts_and_ty_diagnostics(source_map, anon_consts, diagnostics);
            }
        }
        self.collect_const_param_tys(def);
    }

    /// Whether [`Self::collect_anon_const`] finds anything in `def`, from the results it reads.
    fn anon_const_reports(&self, def: AnonConstId<'db>) -> bool {
        let db = self.db;
        !InferenceResult::of(db, def).diagnostics().is_empty()
            || !hir_ty::coroutine_obligations::coroutine_obligation_errors(db, def.into()).is_empty()
            || !hir_ty_borrowck::borrowck_anon_const(db, def).diagnostics.is_empty()
            || !hir_ty_mir::mir::const_check::anon_const_check(db, def).diagnostics.is_empty()
            || hir_ty::const_valtree::anon_const_valtree_error(db, def).is_some()
    }

    fn collect_const_param_tys(&mut self, def: GenericDefId) {
        let errors = hir_ty::const_param_ty::const_param_ty_errors(self.db, def);
        if errors.is_empty() {
            return;
        }
        for error in errors {
            let source = def.child_source_view(self.db, error.param.local_id());
            let Some(Either::Left(view::TypeOrConstParam::Const(param))) = source.value else {
                never!("{:?}: a const parameter with no `ast::ConstParam`", error.param);
                continue;
            };
            self.acc.push(
                ConstParamTyNotAllowed {
                    param: InFile::new(source.file_id, AstPtr::of(param)),
                    ty: Type::new(self.db, def, error.ty.as_ref()),
                    reason: error.reason,
                }
                .into(),
            );
        }
    }

    fn collect_def_with_body(&mut self, def: DefWithBodyId, type_owner: TypeOwnerId) {
        if !(self.bodies)(def.into()) {
            return;
        }
        let at = self.acc.len();
        if let Some(deferred) = &mut self.deferred
            && deferred.take(def, at)
        {
            return;
        }
        self.check_body(def, type_owner);
    }

    /// The checks of body `def`, which `bodies` asks for.
    /// Every check reads the body and its results without positions, and asks for the source
    /// map ([`Placing`]) only to place a diagnostic it found.
    fn check_body(&mut self, def: DefWithBodyId, type_owner: TypeOwnerId) {
        let db = self.db;
        let body = Body::of(db, def);
        let placing = Placing::new(db, def);

        self.collect_blocks(body);
        if !Body::unmoved_source_map(db, def).diagnostics().is_empty()
            || store_macro_failed(db, body)
        {
            self.collect_store_diagnostics(placing.get());
        }
        let infer = InferenceResult::of(db, def);
        let stalled = hir_ty::coroutine_obligations::coroutine_obligation_errors(db, def.into());
        if !infer.diagnostics().is_empty() || !stalled.is_empty() {
            self.emit_inference_errors(def.into(), placing.get(), type_owner);
        }

        // FIXME: Body validation should be defined for any `InferBodyId`.
        for op in hir_ty_unsafeck::unsafety_check(self.db, def).ops() {
            let source_map = placing.get();
            match source_map.expr_or_pat_syntax(op.node()) {
                Ok(node) => self.acc.push(
                    MissingUnsafe {
                        node,
                        lint: op.lint,
                        reason: op.reason,
                        first: op.first,
                        in_unsafe_fn: op.in_unsafe_fn,
                    }
                    .into(),
                ),
                Err(SyntheticSyntax) => self.acc.push(
                    UnplacedDiagnostic {
                        owner: unplaced_owner(self.db, def.into(), source_map),
                        what: Unplaced::MissingUnsafe { reason: op.reason, lint: op.lint },
                    }
                    .into(),
                ),
            }
        }

        for diagnostic in BodyValidationDiagnostic::collect(self.db, def, self.style_lints) {
            self.acc.extend(AnyDiagnostic::body_validation_diagnostic(
                self.db,
                diagnostic,
                placing.get(),
            ));
        }
        crate::match_lints::match_check_diagnostics(
            self.db,
            def,
            || placing.get(),
            type_owner,
            self.acc,
        );
        self.collect_borrowck(def, body, &placing);
        // A repeat count or const argument in the body is a constant of its own, inferred and
        // checked apart from it, as rustc does. The list can name one twice (merging a nested
        // body's appends its own), and each is reported once.
        // Their inference errors are the body's, reported with it above.
        let defined = infer.defined_anon_consts();
        for (at, &konst) in defined.iter().enumerate() {
            if !defined[..at].contains(&konst) && self.anon_const_reports(konst) {
                let source_map = placing.get();
                self.collect_anon_const_borrowck(source_map, konst);
                self.collect_anon_const_const_check(source_map, konst);
                self.collect_anon_const_valtree(source_map, konst);
            }
        }
        self.collect_const_eval(def, body, infer, &placing);
    }

    /// [`ConstEvalFailed`] of `def` when it is a free `const` or a `static` whose body has no
    /// error of its own: rustc evaluates no body an earlier error taints.
    fn collect_const_eval(
        &mut self,
        def: DefWithBodyId,
        body: &Body,
        infer: &InferenceResult<'db>,
        placing: &Placing<'db>,
    ) {
        use hir_def::{
            hir::generics::GenericParams,
            signatures::{StaticFlags, StaticSignature},
        };
        use hir_ty::next_solver::GenericArgs;
        let db = self.db;
        let (name, is_static) = match def {
            DefWithBodyId::ConstId(id) => {
                let signature = ConstSignature::of(db, id);
                if !signature.has_body()
                    || !matches!(id.lookup(db).container, ItemContainerId::ModuleId(_))
                    || !GenericParams::of(db, id.into()).is_empty()
                {
                    return;
                }
                (signature.name.as_ref(), false)
            }
            DefWithBodyId::StaticId(id) => {
                let signature = StaticSignature::of(db, id);
                if !signature.flags.contains(StaticFlags::HAS_BODY) {
                    return;
                }
                (Some(&signature.name), true)
            }
            DefWithBodyId::FunctionId(_) | DefWithBodyId::VariantId(_) => return,
        };
        if infer.has_type_mismatches() || infer.is_erroneous() || !infer.diagnostics().is_empty() {
            return;
        }
        let borrowck = hir_ty_borrowck::borrowck(db, def);
        if borrowck.skipped.is_some()
            || borrowck.not_checked.is_some_and(|it| it.refuses())
            || borrowck.diagnostics.iter().any(|it| it.is_error())
            || !hir_ty_mir::mir::const_check::const_check(db, def).diagnostics.is_empty()
        {
            return;
        }
        let evaluated = match def {
            DefWithBodyId::ConstId(id) => db.const_eval(id, GenericArgs::empty(), None).map(drop),
            DefWithBodyId::StaticId(id) => db.const_eval_static(id).map(drop),
            DefWithBodyId::FunctionId(_) | DefWithBodyId::VariantId(_) => return,
        };
        let Err(error) = evaluated else { return };
        let display_target = DisplayTarget::from_crate_and_edition(db, self.krate, self.edition);
        let own = InferBodyId::DefWithBodyId(def);
        let Some((failure, span)) = const_eval_failure(db, own, &error, display_target) else {
            return;
        };
        let source_map = placing.get();
        let at = |span: hir_ty_mir::mir::MirSpan| {
            use hir_ty_mir::mir::MirSpan;
            match span {
                MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => source_map.expr_syntax(expr).ok(),
                MirSpan::PatId(pat) => source_map.pat_syntax(pat).ok(),
                _ => None,
            }
        };
        let node = span
            .and_then(at)
            .or_else(|| source_map.expr_syntax(body.root_expr()).ok())
            .map(|it| it.map(|it| it.syntax_node_ptr()));
        let Some(node) = node else {
            never!("{def:?}: a `const` or `static` whose value has no syntax");
            return;
        };
        // alloc: error path, the item's name in a reported diagnostic
        let item = name.map_or_else(|| "_".to_owned(), |it| it.as_str().to_owned());
        self.acc.push(ConstEvalFailed { node, item, is_static, failure }.into());
    }

    fn collect_borrowck(&mut self, def: DefWithBodyId, body: &Body, placing: &Placing<'db>) {
        use hir_ty_mir::mir::MirSpan;
        let result = hir_ty_borrowck::borrowck(self.db, def);
        // An `async fn`'s body is the future it makes, which rustc points at as its block.
        let block = || -> Option<InFile<SyntaxNodePtr>> {
            let DefWithBodyId::FunctionId(function) = def else { return None };
            let source = hir_def::src::HasSource::source_view(function.lookup(self.db), self.db);
            let block = source.value.body()?;
            Some(InFile::new(source.file_id, view::ptr(block.syntax())))
        };
        let node_of = |span: MirSpan| -> Option<InFile<SyntaxNodePtr>> {
            let source_map = placing.get();
            let source = match span {
                MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => {
                    match source_map.expr_syntax(expr) {
                        Ok(it) => it,
                        // A borrow `format_args!` lowers to is at what it borrows, and a
                        // variable its template names at the template.
                        Err(_) => match body.expr(expr) {
                            hir_def::hir::Expr::Ref { expr: inner, .. } => {
                                match source_map.expr_syntax(inner) {
                                    Ok(it) => it,
                                    Err(_) => source_map
                                        .format_args_implicit_capture(inner)?
                                        .map(|(template, _)| template.wrap_left()),
                                }
                            }
                            _ if expr == body.root_expr() => return block(),
                            // What else a macro lowers to, as the call of `Arguments::new` of
                            // `format_args!`, is at the nearest expression the source has,
                            // where rustc points at the macro call.
                            _ => {
                                let mut child = expr;
                                loop {
                                    let (parent, _) = body.exprs().find(|&(it, _)| {
                                        let mut found = false;
                                        body.walk_child_exprs(it, |it| found |= it == child);
                                        found
                                    })?;
                                    if let Ok(it) = source_map.expr_syntax(parent) {
                                        break it;
                                    }
                                    child = parent;
                                }
                            }
                        },
                    }
                }
                MirSpan::PatId(pat) => source_map.pat_syntax(pat).ok()?,
                // `self` is a binding no pattern binds.
                MirSpan::BindingId(binding)
                    if body
                        .self_param
                        .is_some_and(|it| it.formal == binding || it.user_written == binding) =>
                {
                    return source_map
                        .self_param_syntax()
                        .map(|it| it.map(|it| it.syntax_node_ptr()));
                }
                MirSpan::BindingId(binding) => {
                    let pat = *source_map.patterns_for_binding(binding).first()?;
                    source_map.pat_syntax(pat).ok()?
                }
                MirSpan::SelfParam => {
                    return source_map
                        .self_param_syntax()
                        .map(|it| it.map(|it| it.syntax_node_ptr()));
                }
                MirSpan::Unknown => match source_map.expr_syntax(body.root_expr()) {
                    Ok(it) => it,
                    Err(_) => return block(),
                },
                // Only an optimized body inlines; the borrow checker reads built ones.
                MirSpan::Inlined(_) => return None,
            };
            Some(source.map(|it| it.syntax_node_ptr()))
        };
        // rustc's span of a function's signature starts at `fn`, the line of its name.
        let signature = || -> Option<InFile<SyntaxNodePtr>> {
            let DefWithBodyId::FunctionId(function) = def else { return None };
            let source = hir_def::src::HasSource::source_view(function.lookup(self.db), self.db);
            let name = source.value.name()?;
            Some(InFile::new(source.file_id, view::ptr(name.syntax())))
        };
        for diagnostic in &result.diagnostics {
            let node = match diagnostic.at_signature {
                true => signature().or_else(|| node_of(diagnostic.span)),
                false => node_of(diagnostic.span),
            };
            let Some(node) = node else {
                self.acc.push(
                    UnplacedDiagnostic {
                        owner: unplaced_owner(self.db, def.into(), placing.get()),
                        what: Unplaced::Borrowck {
                            code: diagnostic.code,
                            // clone: error path, a reported borrowck error
                            message: diagnostic.message.clone(),
                        },
                    }
                    .into(),
                );
                continue;
            };
            // A variable lowering declares `mut` itself, as an `async fn`'s argument, points
            // at what the user wrote, which has no `mut` to remove.
            if diagnostic.code == "unused_mut" {
                let pat = view::to_node(node.value, node.file_id.root(self.db));
                let written = match view::SelfParam::can_cast(pat.kind()) {
                    true => view::SelfParam::cast(pat).is_some_and(|it| it.mut_token().is_some()),
                    false => view::IdentPat::cast(pat).is_some_and(|it| it.mut_token().is_some()),
                };
                if !written {
                    continue;
                }
            }
            let (labels, children, suggestions) = borrowck_parts(self.db, diagnostic, &node_of);
            self.acc.push(
                BorrowckError {
                    node,
                    at_end: matches!(diagnostic.span, MirSpan::ExprEnd(_)),
                    code: diagnostic.code,
                    // clone: error path, a reported borrowck error
                    message: diagnostic.message.clone(),
                    labels,
                    children,
                    suggestions,
                }
                .into(),
            );
        }
        let const_check = &hir_ty_mir::mir::const_check::const_check(self.db, def).diagnostics;
        for diagnostic in const_check {
            let Some(node) = node_of(diagnostic.span) else {
                self.acc.push(
                    UnplacedDiagnostic {
                        owner: unplaced_owner(self.db, def.into(), placing.get()),
                        what: Unplaced::ConstCheck {
                            code: diagnostic.code,
                            // clone: error path, a reported const check error
                            message: diagnostic.message.clone(),
                        },
                    }
                    .into(),
                );
                continue;
            };
            self.acc.push(
                ConstCheckError {
                    node,
                    code: diagnostic.code,
                    // clone: error path, a reported const check error
                    message: diagnostic.message.clone(),
                    // clone: error path, a reported const check error
                    kind: diagnostic.kind.clone(),
                    context: diagnostic.context,
                }
                .into(),
            );
        }
        // `blaze::const_impure` is fn-cache's lint: no other crate's `const fn` is walked for it.
        if let DefWithBodyId::FunctionId(func) = def
            && hir_ty::fn_cache::uses_fn_cache(self.db, hir_def::HasModule::krate(&def, self.db))
            && const_check.is_empty()
            && hir_ty_mir::mir::const_check::const_context(self.db, def)
                == Some(hir_ty_mir::mir::const_check::ConstContext::ConstFn)
            && let Some(impure) = hir_ty_mir::mir::purity::const_purity(self.db, func)
            && impure.told
            && let Some(node) = node_of(impure.span)
        {
            // alloc: error path, a reported lint's message
            let message = format!("const fn {}", impure.said());
            self.acc.push(ConstImpure { node, message }.into());
        }
        if let Some(reason) = &result.skipped {
            use hir_def::src::HasSource as _;
            let node = node_of(MirSpan::Unknown).unwrap_or_else(|| {
                let db = self.db;
                match def {
                    DefWithBodyId::FunctionId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                    DefWithBodyId::StaticId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                    DefWithBodyId::ConstId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                    DefWithBodyId::VariantId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                }
            });
            // clone: error path, a reported diagnostic
            self.acc.push(BorrowckSkipped { node, reason: reason.clone() }.into());
        }
    }

    fn emit_inference_errors(
        &mut self,
        def: InferBodyId<'db>,
        source_map: &ExpressionStoreSourceMap,
        type_owner: TypeOwnerId,
    ) {
        let infer = InferenceResult::of(self.db, def);
        // rustc's `check_coroutine_obligations`, after the body's own errors.
        let stalled = hir_ty::coroutine_obligations::coroutine_obligation_errors(self.db, def);

        for diag in infer.diagnostics().iter().chain(stalled) {
            let placed = AnyDiagnostic::inference_diagnostic(
                self.db,
                self.krate,
                self.edition,
                diag,
                source_map,
                type_owner,
            );
            let found = match placed {
                Some(it) => it,
                None => {
                    let target = DisplayTarget::from_crate_and_edition(self.db, self.krate, self.edition);
                    UnplacedDiagnostic {
                        owner: unplaced_owner(self.db, def, source_map),
                        what: Unplaced::Inference(unplaced_inference_text(self.db, target, def, diag)),
                    }
                    .into()
                }
            };
            self.acc.push(found);
        }
    }

    /// The checks of `def`'s fields, read without positions as [`Self::collect_generic_def`]
    /// reads a signature: their source map only to place a diagnostic.
    fn collect_variant(&mut self, def: VariantId) {
        use hir_def::signatures::VariantFields;
        let db = self.db;
        let store = &VariantFields::of(db, def).store;
        self.collect_blocks(store);
        let lowering = db.field_types_with_diagnostics(def);
        let store_reports = match VariantFields::relative(db, def) {
            Some((_, map)) => !map.diagnostics().is_empty(),
            None => !VariantFields::with_source_map(db, def).1.diagnostics().is_empty(),
        };
        let reports = store_reports
            || store_macro_failed(db, store)
            || !lowering.diagnostics().is_empty()
            || lowering.defined_anon_consts().iter().any(|&it| self.anon_const_reports(it));
        if reports {
            let (_, source_map) = def.fields_with_source_map(db);
            self.collect_store_diagnostics(source_map);
            self.collect_anon_consts_and_ty_diagnostics(
                source_map,
                lowering.defined_anon_consts(),
                lowering.diagnostics(),
            );
        }
    }

    fn collect_generic_def_with_body(
        &mut self,
        def: impl Into<GenericDefId> + Into<DefWithBodyId> + Copy,
    ) {
        let generic_def: GenericDefId = def.into();
        self.collect_generic_def(generic_def);

        let def_with_body: DefWithBodyId = def.into();
        self.collect_def_with_body(def_with_body, generic_def.into());
        if let GenericDefId::FunctionId(_) = generic_def {
            self.collect_wfcheck(generic_def);
        }
    }

    fn collect_generic_variant(&mut self, def: impl Into<GenericDefId> + Into<VariantId> + Copy) {
        let generic_def: GenericDefId = def.into();
        self.collect_generic_def(generic_def);
        self.collect_variant(def.into());
        self.collect_wfcheck(generic_def);

        use syntax::view::HasVisibility as _;
        let variant: VariantId = def.into();
        let module = hir_def::HasModule::module(&variant, self.db);
        let fields = hir_def::src::enabled_fields_view(self.db, variant);
        for field in fields.value {
            let vis = field.either(|it| it.visibility(), |it| it.visibility());
            self.collect_visibility(module, InFile::new(fields.file_id, vis));
        }
    }

    /// rustc's well-formedness errors of an ADT or an impl, reported as the borrow checker's.
    fn collect_wfcheck(&mut self, def: GenericDefId) {
        use hir_ty_wfcheck::Site;
        let db = self.db;
        for diagnostic in &hir_ty_wfcheck::wfcheck(db, def).diagnostics {
            let mut node = None;
            let source = match diagnostic.site {
                Site::Field(variant, field) => {
                    let (fields, source_map) = variant.fields_with_source_map(db);
                    source_map.type_syntax(fields.fields()[field].type_ref).ok()
                }
                Site::AssocType(alias) => {
                    let (signature, source_map) = TypeAliasSignature::with_source_map(db, alias);
                    signature.ty.and_then(|ty| source_map.type_syntax(ty).ok())
                }
                Site::ImplTrait(impl_) => {
                    let (signature, source_map) = ImplSignature::with_source_map(db, impl_);
                    signature
                        .target_trait
                        .as_ref()
                        .and_then(|it| source_map.type_syntax(it.path.type_ref()).ok())
                }
                Site::Bound(owner, index) => {
                    use hir_def::hir::{
                        generics::{GenericParams, WherePredicate},
                        type_ref::TypeBound,
                    };
                    let (_, source_map) = ExpressionStore::with_source_map(db, owner.into());
                    match GenericParams::of(db, owner).where_predicates().get(index as usize) {
                        Some(WherePredicate::TypeBound { target, bound, .. }) => match bound {
                            TypeBound::Path(path, _) | TypeBound::ForLifetime(_, path) => {
                                source_map.type_syntax(path.type_ref()).ok()
                            }
                            _ => source_map.type_syntax(*target).ok(),
                        },
                        _ => None,
                    }
                }
                Site::Param(function, index) => {
                    node = param_syntax(db, function, index);
                    None
                }
                Site::Output(function) => {
                    node = output_syntax(db, function);
                    None
                }
                Site::SelfTy(impl_) => {
                    let (signature, source_map) = ImplSignature::with_source_map(db, impl_);
                    source_map.type_syntax(signature.self_ty).ok()
                }
                Site::Opaque(function, index) => {
                    node = opaque_syntax(db, function, index);
                    None
                }
            };
            let Some(node) = source.map(|it| it.map(|it| it.syntax_node_ptr())).or(node) else {
                continue;
            };
            self.acc.push(
                BorrowckError {
                    node,
                    at_end: false,
                    code: diagnostic.code,
                    // clone: error path, a reported wfcheck error
                    message: diagnostic.message.clone(),
                    // alloc: empty
                    labels: Vec::new(),
                    // alloc: empty
                    children: Vec::new(),
                    // alloc: empty
                    suggestions: Vec::new(),
                }
                .into(),
            );
        }
    }

    fn collect_only_generic_def(&mut self, def: impl Into<GenericDefId>) {
        self.collect_generic_def(def.into());
    }

    fn collect_module_def(&mut self, def: ModuleDefId, def_map: &DefMap) {
        self.emit_case_diagnostics(def);

        match def {
            ModuleDefId::ModuleId(def) => {
                // Only add diagnostics from inline modules
                if def_map.module(self.db, def).origin.is_inline() {
                    self.collect_module(def);
                }
            }
            ModuleDefId::TraitId(def) => self.collect_trait(def, def_map),
            ModuleDefId::MacroId(def) => self.collect_macro_def(def),
            ModuleDefId::FunctionId(def) => self.collect_generic_def_with_body(def),
            ModuleDefId::ConstId(def) => self.collect_generic_def_with_body(def),
            ModuleDefId::StaticId(def) => self.collect_generic_def_with_body(def),
            ModuleDefId::EnumVariantId(def) => self.collect_enum_variant(def),
            ModuleDefId::AdtId(AdtId::StructId(def)) => self.collect_generic_variant(def),
            ModuleDefId::AdtId(AdtId::UnionId(def)) => self.collect_generic_variant(def),
            ModuleDefId::AdtId(AdtId::EnumId(def)) => self.collect_enum(def),
            ModuleDefId::TypeAliasId(def) => self.collect_only_generic_def(def),
            ModuleDefId::BuiltinType(_) => {}
        }
    }

    /// The items of `store`'s block modules.
    fn collect_blocks(&mut self, store: &ExpressionStore) {
        for (_, def_map) in store.blocks(self.db) {
            self.collect_module(def_map.root_module_id());
        }
    }

    /// What lowering the store of `source_map` found, placed.
    fn collect_store_diagnostics(&mut self, source_map: &ExpressionStoreSourceMap) {
        for diag in source_map.diagnostics() {
            self.acc.push(match diag {
                ExpressionStoreDiagnostics::InactiveCode { node, cfg } => InactiveCode {
                    node: *node,
                    // clone: error path, a reported diagnostic
                    cfg: cfg.clone(),
                    // clone: error path, a reported diagnostic
                    opts: self.krate.cfg_options(self.db).clone(),
                }
                .into(),
                ExpressionStoreDiagnostics::UnresolvedMacroCall { node, path } => {
                    UnresolvedMacroCall {
                        range: node.map(|ptr| ptr.text_range()),
                        // clone: error path, a reported diagnostic
                        path: path.clone(),
                        is_bang: true,
                    }
                    .into()
                }
                ExpressionStoreDiagnostics::AwaitOutsideOfAsync { node, location } => {
                    // clone: error path, a reported diagnostic
                    AwaitOutsideOfAsync { node: *node, location: location.clone() }.into()
                }
                ExpressionStoreDiagnostics::UnreachableLabel { node, name } => {
                    UnreachableLabel { node: *node, name: *name }.into()
                }
                ExpressionStoreDiagnostics::UndeclaredLabel { node, name } => {
                    UndeclaredLabel { node: *node, name: *name }.into()
                }
                ExpressionStoreDiagnostics::PatternArgInExternFn { node } => {
                    PatternArgInExternFn { node: *node }.into()
                }
                ExpressionStoreDiagnostics::FruInDestructuringAssignment { node } => {
                    FruInDestructuringAssignment { node: *node }.into()
                }
                ExpressionStoreDiagnostics::MissingBody { node, kind } => {
                    MissingBody { node: *node, kind: *kind }.into()
                }
                ExpressionStoreDiagnostics::IntLiteralTooLarge { node } => {
                    IntLiteralTooLarge { node: *node }.into()
                }
            });
        }

        source_map.macro_calls().for_each(|(_ast_id, call_id)| self.collect_macro_call(call_id));
    }
}

impl<'db> AnyDiagnostic<'db> {
    fn body_validation_diagnostic(
        db: &'db Db,
        diagnostic: BodyValidationDiagnostic<'db>,
        source_map: &hir_def::expr_store::BodySourceMap,
    ) -> Option<AnyDiagnostic<'db>> {
        match diagnostic {
            BodyValidationDiagnostic::RecordMissingFields { record, variant, missed_fields } => {
                let variant_data = variant.fields(db);
                let missed_fields = missed_fields
                    .into_iter()
                    .map(|idx| {
                        (variant_data.fields()[idx].name, Field { parent: variant.into(), id: idx })
                    })
                    // alloc: error path, a reported diagnostic
                    .collect();

                let record = match record {
                    Either::Left(record_expr) => source_map.expr_syntax(record_expr).ok()?,
                    Either::Right(record_pat) => source_map.pat_syntax(record_pat).ok()?,
                };
                let file = record.file_id;
                match record.to_view(db) {
                    Either::Left(view::Expr::RecordExpr(record_expr))
                        if record_expr.record_expr_field_list().is_some() =>
                    {
                        let field_list_parent_path = record_expr.path().map(AstPtr::of);
                        return Some(
                            MissingFields {
                                file,
                                field_list_parent: AstPtr::of(Either::Left(record_expr)),
                                field_list_parent_path,
                                variant: variant.into(),
                                missed_fields,
                            }
                            .into(),
                        );
                    }
                    Either::Right(view::Pat::RecordPat(record_pat))
                        if record_pat.record_pat_field_list().is_some() =>
                    {
                        let field_list_parent_path = record_pat.path().map(AstPtr::of);
                        return Some(
                            MissingFields {
                                file,
                                field_list_parent: AstPtr::of(Either::Right(record_pat)),
                                field_list_parent_path,
                                variant: variant.into(),
                                missed_fields,
                            }
                            .into(),
                        );
                    }
                    _ => {}
                }
            }
            BodyValidationDiagnostic::ReplaceFilterMapNextWithFindMap { method_call_expr } => {
                if let Ok(next_source_ptr) = source_map.expr_syntax(method_call_expr) {
                    return Some(
                        ReplaceFilterMapNextWithFindMap {
                            file: next_source_ptr.file_id,
                            next_expr: next_source_ptr.value.cast()?,
                        }
                        .into(),
                    );
                }
            }
            BodyValidationDiagnostic::RemoveTrailingReturn { return_expr } => {
                if let Ok(source_ptr) = source_map.expr_syntax(return_expr)
                    // Filters out desugared return expressions (e.g. desugared try operators).
                    && let Some(ptr) = source_ptr.value.cast::<ast::ReturnExpr>()
                {
                    return Some(
                        RemoveTrailingReturn { return_expr: InFile::new(source_ptr.file_id, ptr) }
                            .into(),
                    );
                }
            }
            BodyValidationDiagnostic::RemoveUnnecessaryElse { if_expr } => {
                if let Ok(source_ptr) = source_map.expr_syntax(if_expr)
                    && let Some(ptr) = source_ptr.value.cast::<ast::IfExpr>()
                {
                    return Some(
                        RemoveUnnecessaryElse { if_expr: InFile::new(source_ptr.file_id, ptr) }
                            .into(),
                    );
                }
            }
            BodyValidationDiagnostic::UnusedMustUse { expr, message } => {
                if let Ok(source_ptr) = source_map.expr_syntax(expr) {
                    return Some(UnusedMustUse { expr: source_ptr, message }.into());
                }
            }
        }
        None
    }

    fn inference_diagnostic(
        db: &'db Db,
        krate: base_db::Crate,
        edition: Edition,
        d: &'db InferenceDiagnostic,
        source_map: &ExpressionStoreSourceMap,
        type_owner: TypeOwnerId,
    ) -> Option<AnyDiagnostic<'db>> {
        let expr_syntax = |expr| Self::expr_syntax(expr, source_map);
        let pat_syntax = |pat| Self::pat_syntax(pat, source_map);
        let expr_or_pat_syntax = |id| match id {
            ExprOrPatId::ExprId(expr) => expr_syntax(expr),
            ExprOrPatId::PatId(pat) => pat_syntax(pat),
        };
        let interner = DbInterner::new_no_crate(db);
        let new_ty = |ty| Type { owner: type_owner, ty: EarlyBinder::bind(interner, ty) };
        let span_syntax = |span| Self::span_syntax(span, source_map);
        Some(match d {
            &InferenceDiagnostic::NoSuchField { field: expr, private, variant } => {
                let expr_or_pat = match expr.unpack() {
                    ExprOrPatId::ExprId(expr) => {
                        source_map.field_syntax(expr).map(AstPtr::wrap_left)
                    }
                    ExprOrPatId::PatId(pat) => source_map.pat_field_syntax(pat),
                };
                let private = private.map(|id| Field { id, parent: variant.into() });
                NoSuchField { field: expr_or_pat, private, variant }.into()
            }
            &InferenceDiagnostic::MismatchedArrayPatLen { pat, expected, found, has_rest } => {
                let pat = pat_syntax(pat)?.map(Into::into);
                MismatchedArrayPatLen { pat, expected, found, has_rest }.into()
            }
            &InferenceDiagnostic::ArrayPatternWithoutFixedLength { pat } => {
                let pat = pat_syntax(pat)?.map(Into::into);
                ArrayPatternWithoutFixedLength { pat }.into()
            }
            InferenceDiagnostic::ExpectedArrayOrSlicePat { pat, found } => {
                let pat = pat_syntax(*pat)?.map(Into::into);
                ExpectedArrayOrSlicePat {
                    pat,
                    found: Type {
                        owner: type_owner,
                        ty: EarlyBinder::bind(interner, found.as_ref()),
                    },
                }
                .into()
            }
            InferenceDiagnostic::InvalidRangePatType { pat, lhs, rhs } => {
                let pat = pat_syntax(*pat)?.map(Into::into);
                let end = |side: &Option<(StoredTy, bool)>| {
                    side.as_ref().map(|(ty, fail)| (new_ty(ty.as_ref()), *fail))
                };
                InvalidRangePatType { pat, lhs: end(lhs), rhs: end(rhs) }.into()
            }
            &InferenceDiagnostic::DuplicateField { field: expr, variant } => {
                let expr_or_pat = match expr.unpack() {
                    ExprOrPatId::ExprId(expr) => {
                        source_map.field_syntax(expr).map(AstPtr::wrap_left)
                    }
                    ExprOrPatId::PatId(pat) => source_map.pat_field_syntax(pat),
                };
                DuplicateField { field: expr_or_pat, variant: variant.into() }.into()
            }
            InferenceDiagnostic::MismatchedArgCount {
                call_expr,
                expected,
                found,
                is_fn_trait_call,
                args,
            } => {
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                MismatchedArgCount {
                    call_expr: expr_syntax(*call_expr)?,
                    expected: *expected,
                    found: *found,
                    is_fn_trait_call: *is_fn_trait_call,
                    args: CallArgs::new(db, target, args),
                }
                .into()
            }
            InferenceDiagnostic::IncorrectArguments { call_expr, args } => {
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                let args = CallArgs::new(db, target, args);
                IncorrectArguments { call_expr: expr_syntax(*call_expr)?, args }.into()
            }
            &InferenceDiagnostic::PrivateField { expr, field } => {
                let expr = expr_syntax(expr)?;
                let field = field.into();
                PrivateField { expr, field }.into()
            }
            &InferenceDiagnostic::PrivateAssocItem { id, item } => {
                let expr_or_pat = expr_or_pat_syntax(id.unpack())?;
                let item = item.into();
                PrivateAssocItem { expr_or_pat, item }.into()
            }
            InferenceDiagnostic::ExpectedFunction { call_expr, found } => {
                let call_expr = expr_syntax(*call_expr)?;
                ExpectedFunction { call: call_expr, found: new_ty(found.as_ref()) }.into()
            }
            InferenceDiagnostic::UnresolvedField {
                expr,
                receiver,
                name,
                method_with_same_name_exists,
            } => {
                let expr = expr_syntax(*expr)?;
                UnresolvedField {
                    expr,
                    name: *name,
                    receiver: new_ty(receiver.as_ref()),
                    method_with_same_name_exists: *method_with_same_name_exists,
                }
                .into()
            }
            InferenceDiagnostic::UnresolvedMethodCall {
                expr,
                receiver,
                name,
                field_with_same_name,
                assoc_func_with_same_name,
            } => {
                let expr = expr_syntax(*expr)?;
                UnresolvedMethodCall {
                    expr,
                    name: *name,
                    receiver: new_ty(receiver.as_ref()),
                    field_with_same_name: field_with_same_name
                        .as_ref()
                        .map(|ty| new_ty(ty.as_ref())),
                    assoc_func_with_same_name: assoc_func_with_same_name.map(Into::into),
                }
                .into()
            }
            InferenceDiagnostic::UnresolvedAssocItem { id, ty, name } => {
                let expr_or_pat = expr_or_pat_syntax(id.unpack())?;
                UnresolvedAssocItem { expr_or_pat, ty: new_ty(ty.as_ref()), name: *name }.into()
            }
            &InferenceDiagnostic::UnresolvedIdent { id } => {
                let node = match id.unpack() {
                    ExprOrPatId::ExprId(id) => match source_map.expr_syntax(id) {
                        Ok(syntax) => syntax.map(|it| (it, None)),
                        Err(SyntheticSyntax) => source_map
                            .format_args_implicit_capture(id)?
                            .map(|(node, range)| (node.wrap_left(), Some(range))),
                    },
                    ExprOrPatId::PatId(id) => pat_syntax(id)?.map(|it| (it, None)),
                };
                UnresolvedIdent { node }.into()
            }
            &InferenceDiagnostic::UnresolvedPath { id, segment } => {
                UnresolvedPath { node: expr_or_pat_syntax(id.unpack())?, segment }.into()
            }
            &InferenceDiagnostic::BreakOutsideOfLoop { expr, is_break, bad_value_break } => {
                let expr = expr_syntax(expr)?;
                BreakOutsideOfLoop { expr, is_break, bad_value_break }.into()
            }
            &InferenceDiagnostic::UnlabeledJumpInLabeledBlock { expr, is_break } => {
                UnlabeledJumpInLabeledBlock { expr: expr_syntax(expr)?, is_break }.into()
            }
            &InferenceDiagnostic::AsyncCoroutinesNotSupported { expr } => {
                AsyncCoroutinesNotSupported { expr: expr_syntax(expr)? }.into()
            }
            &InferenceDiagnostic::NonExhaustiveRecordExpr { expr } => {
                NonExhaustiveRecordExpr { expr: expr_syntax(expr)? }.into()
            }
            &InferenceDiagnostic::NonExhaustiveRecordPat { pat, variant } => {
                let pat = pat_syntax(pat)?.map(Into::into);
                NonExhaustiveRecordPat { pat, variant: variant.into() }.into()
            }
            &InferenceDiagnostic::UnionPatMustHaveExactlyOneField { pat } => {
                let pat = pat_syntax(pat)?.map(Into::into);
                UnionPatMustHaveExactlyOneField { pat }.into()
            }
            &InferenceDiagnostic::UnionPatHasRest { pat } => {
                let pat = pat_syntax(pat)?.map(Into::into);
                UnionPatHasRest { pat }.into()
            }
            &InferenceDiagnostic::FunctionalRecordUpdateOnNonStruct { base_expr } => {
                FunctionalRecordUpdateOnNonStruct { base_expr: expr_syntax(base_expr)? }.into()
            }
            InferenceDiagnostic::TypedHole { expr, expected } => {
                let expr = expr_syntax(*expr)?;
                TypedHole { expr, expected: new_ty(expected.as_ref()) }.into()
            }
            &InferenceDiagnostic::MismatchedTupleStructPatArgCount {
                pat,
                expected,
                found,
                variant,
                had_err,
            } => {
                let InFile { file_id, value } = pat_syntax(pat)?;
                // cast from Either<Pat, SelfParam> -> Either<_, Pat>
                let ptr = AstPtr::try_from_raw(value.syntax_node_ptr())?;
                let expr_or_pat = InFile { file_id, value: ptr };
                MismatchedTupleStructPatArgCount {
                    expr_or_pat,
                    expected,
                    found,
                    variant: variant.into(),
                    had_err,
                }
                .into()
            }
            InferenceDiagnostic::CastToUnsized { expr, cast_ty } => {
                let expr = expr_syntax(*expr)?;
                CastToUnsized { expr, cast_ty: new_ty(cast_ty.as_ref()) }.into()
            }
            InferenceDiagnostic::InvalidCast { expr, error, expr_ty, cast_ty, pointers } => {
                let expr = expr_syntax(*expr)?;
                let expr_ty = new_ty(expr_ty.as_ref());
                let cast_ty = new_ty(cast_ty.as_ref());
                let kind = |kind: Option<hir_ty::CastPointerKind>| {
                    kind.map(|kind| match kind {
                        hir_ty::CastPointerKind::Thin => CastPointerKind::Thin,
                        hir_ty::CastPointerKind::VTable(principal) => {
                            CastPointerKind::VTable(principal.map(|id| crate::Trait { id }))
                        }
                        hir_ty::CastPointerKind::Length => CastPointerKind::Length,
                        hir_ty::CastPointerKind::OfAlias => CastPointerKind::OfAlias,
                        hir_ty::CastPointerKind::OfParam(id) => {
                            CastPointerKind::OfParam(crate::TypeParam { id })
                        }
                    })
                };
                let (expr_pointer, cast_pointer, added_auto_traits) = match pointers {
                    Some(it) => (
                        kind(it.expr),
                        kind(it.cast),
                        // alloc: once per refused pointer cast
                        it.added_auto_traits.iter().map(|&id| crate::Trait { id }).collect(),
                    ),
                    // alloc: empty
                    None => (None, None, Vec::new()),
                };
                InvalidCast {
                    expr,
                    error: *error,
                    expr_ty,
                    cast_ty,
                    expr_pointer,
                    cast_pointer,
                    added_auto_traits,
                }
                .into()
            }
            InferenceDiagnostic::CannotBeDereferenced { expr, found, at_error } => {
                let expr = expr_syntax(*expr)?;
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                // alloc: error path, a reported diagnostic's type as rustc prints it
                let at_error = at_error.as_ref().map(|it| it.as_ref().display(db, target).to_string());
                CannotBeDereferenced { expr, found: new_ty(found.as_ref()), at_error }.into()
            }
            InferenceDiagnostic::UnaryOperatorCannotBeApplied { expr, op, found, at_error, unmet } => {
                let expr = expr_syntax(*expr)?;
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                // alloc: error path, a reported diagnostic's type as rustc prints it
                let at_error = at_error.as_ref().map(|it| it.as_ref().display(db, target).to_string());
                let unmet =
                    // alloc: error path, a reported diagnostic's unmet goals
                    unmet.iter().map(|it| Self::solver_goal(db, it, type_owner, target)).collect();
                let found = new_ty(found.as_ref());
                UnaryOperatorCannotBeApplied { expr, op: *op, found, at_error, unmet }.into()
            }
            InferenceDiagnostic::BinaryOperatorCannotBeApplied {
                expr,
                op,
                lhs,
                rhs,
                at_error,
                unmet,
                output,
            } => {
                let expr = expr_syntax(*expr)?;
                let (lhs, rhs) = (new_ty(lhs.as_ref()), new_ty(rhs.as_ref()));
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                let at_error = at_error.as_deref().map(|(lhs, rhs)| {
                    (
                        // alloc: error path, a reported diagnostic's types as rustc prints them
                        lhs.as_ref().display(db, target).to_string(),
                        // alloc: error path, as above
                        rhs.as_ref().display(db, target).to_string(),
                    )
                });
                let unmet =
                    // alloc: error path, a reported diagnostic's unmet goals
                    unmet.iter().map(|it| Self::solver_goal(db, it, type_owner, target)).collect();
                let output = output.as_ref().map(|it| new_ty(it.as_ref()));
                BinaryOperatorCannotBeApplied { expr, op: *op, lhs, rhs, at_error, unmet, output }
                    .into()
            }
            InferenceDiagnostic::MutRefInImmRefPat { pat } => {
                let pat = pat_syntax(*pat)?.map(Into::into);
                MutRefInImmRefPat { pat }.into()
            }
            InferenceDiagnostic::CannotImplicitlyDerefTraitObject { pat, found } => {
                let pat = pat_syntax(*pat)?.map(Into::into);
                CannotImplicitlyDerefTraitObject { pat, found: new_ty(found.as_ref()) }.into()
            }
            InferenceDiagnostic::CannotIndexInto { expr, found, at_error } => {
                let expr = expr_syntax(*expr)?;
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                // alloc: error path, a reported diagnostic's type as rustc prints it
                let at_error = at_error.as_ref().map(|it| it.as_ref().display(db, target).to_string());
                CannotIndexInto { expr, found: new_ty(found.as_ref()), at_error }.into()
            }
            InferenceDiagnostic::TyDiagnostic { diag } => {
                Self::ty_diagnostic(diag, source_map, db)?
            }
            InferenceDiagnostic::PathDiagnostic { node, diag } => {
                let source = expr_or_pat_syntax(node.unpack())?;
                let syntax = source.to_view(db);
                let path = match_ast! {
                    match (syntax.syntax()) {
                        view::RecordExpr(it) => it.path()?,
                        view::RecordPat(it) => it.path()?,
                        view::TupleStructPat(it) => it.path()?,
                        view::PathExpr(it) => it.path()?,
                        view::PathPat(it) => it.path()?,
                        _ => return None,
                    }
                };
                Self::path_diagnostic(diag, source.with_value(path), db)?
            }
            &InferenceDiagnostic::MethodCallIncorrectGenericsLen {
                expr,
                provided_count,
                expected_count,
                kind,
                def,
            } => {
                let syntax = expr_syntax(expr)?;
                let file_id = syntax.file_id;
                let syntax =
                    syntax.with_value(syntax.value.cast::<ast::MethodCallExpr>()?).to_view(db);
                let generics_or_name = syntax
                    .generic_arg_list()
                    .map(Either::Left)
                    .or_else(|| syntax.name_ref().map(Either::Right))?;
                let generics_or_name = InFile::new(file_id, AstPtr::of(generics_or_name));
                IncorrectGenericsLen {
                    generics_or_segment: generics_or_name,
                    kind,
                    provided: provided_count,
                    expected: expected_count,
                    def: def.into(),
                }
                .into()
            }
            &InferenceDiagnostic::MethodCallIncorrectGenericsOrder {
                expr,
                param_id,
                arg_idx,
                has_self_arg,
            } => {
                let syntax = expr_syntax(expr)?;
                let file_id = syntax.file_id;
                let syntax =
                    syntax.with_value(syntax.value.cast::<ast::MethodCallExpr>()?).to_view(db);
                let generic_args = syntax.generic_arg_list()?;
                let provided_arg = hir_generic_arg_to_ast_view(generic_args, arg_idx, has_self_arg)?;
                let provided_arg = InFile::new(file_id, AstPtr::of(provided_arg));
                let expected_kind = GenericArgKind::from_id(param_id);
                IncorrectGenericsOrder { provided_arg, expected_kind, param: param_id.into() }
                    .into()
            }
            &InferenceDiagnostic::MethodCallLateBoundLifetimeArgs { expr, def } => {
                let syntax = expr_syntax(expr)?;
                let file_id = syntax.file_id;
                let call = syntax.value.cast::<ast::MethodCallExpr>()?;
                let call = call.try_to_view(file_id.root(db))?;
                let generics_or_segment = match view::HasGenericArgs::generic_arg_list(&call) {
                    Some(list) => AstPtr::<ast::GenericArgList>::of(list).wrap_left(),
                    None => AstPtr::<ast::NameRef>::of(call.name_ref()?).wrap_right(),
                };
                let generics_or_segment = InFile::new(file_id, generics_or_segment);
                LateBoundLifetimeArgs { generics_or_segment, def: def.into(), hard_error: false }
                    .into()
            }
            &InferenceDiagnostic::InvalidLhsOfAssignment { lhs, assign, op } => {
                let lhs = expr_syntax(lhs)?;
                let assign = expr_syntax(assign)?;
                InvalidLhsOfAssignment { lhs, assign, op }.into()
            }
            &InferenceDiagnostic::MethodCallIllegalSizedBound {
                call_expr,
                method,
                bound,
                needs_mut,
                ref candidates,
            } => MethodCallIllegalSizedBound {
                call_expr: expr_syntax(call_expr)?,
                method: method.into(),
                bound: bound.map(Into::into),
                needs_mut,
                // alloc: the error path's candidate traits.
                candidates: candidates.iter().map(|&id| crate::Trait { id }).collect(),
            }
            .into(),
            &InferenceDiagnostic::EditionDependentMethodCall {
                call_expr,
                receiver,
                lint,
                method,
                inherent,
                autoderefs,
                autoref,
            } => {
                let call_expr = expr_syntax(call_expr)?;
                let call_expr =
                    call_expr.with_value(call_expr.value.cast::<ast::MethodCallExpr>()?);
                EditionDependentMethodCall {
                    call_expr,
                    receiver: expr_syntax(receiver)?,
                    lint,
                    method: method.into(),
                    inherent,
                    autoderefs,
                    autoref,
                }
                .into()
            }
            InferenceDiagnostic::FullyQualifiedCallFrom2018 { node, item, self_ty } => {
                FullyQualifiedCallFrom2018 {
                    expr_or_pat: expr_or_pat_syntax(node.unpack())?,
                    item: (*item).into(),
                    self_ty: new_ty(self_ty.as_ref()),
                }
                .into()
            }
            &InferenceDiagnostic::TypeMustBeKnown {
                at_point,
                ref top_term,
                kind,
                ref ambiguity,
                origin,
                ref named,
                cause,
            } => {
                let at_point = span_syntax(at_point)?;
                let origin = origin.and_then(|(span, param)| {
                    let span = span_syntax(span)?;
                    let in_expansion = span.file_id.is_macro();
                    match param {
                        hir_def::GenericParamId::TypeParamId(_) if in_expansion => None,
                        _ => Some((span, GenericParam::from(param))),
                    }
                });
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                let ambiguity = ambiguity.as_deref().map(|ambiguity| {
                    let note = |note: &'db solver_errors::AmbiguityNote| {
                        Self::ambiguous_goal(db, note, type_owner, target)
                    };
                    TypeAmbiguity {
                        primary: note(&ambiguity.primary),
                        // alloc: error path, a reported diagnostic's notes
                        related: ambiguity.related.iter().map(note).collect(),
                    }
                });
                let term = |term: hir_ty::next_solver::GenericArg<'db>| match term.kind() {
                    rustc_type_ir::GenericArgKind::Type(ty) => Either::Left(new_ty(ty)),
                    // FIXME: Printing the const to string is definitely not the correct thing to do here.
                    rustc_type_ir::GenericArgKind::Const(konst) => Either::Right(
                        konst
                            .display(db, target)
                            // alloc: error path, a reported diagnostic's const as rustc prints it
                            .to_string(),
                    ),
                    rustc_type_ir::GenericArgKind::Lifetime(_) => {
                        unreachable!("we currently don't emit TypeMustBeKnown for lifetimes")
                    }
                };
                let top_term = top_term.as_ref().map(|top_term| term(top_term.as_ref()));
                // rustc's `ty_infer_name_resolver` names a const parameter wherever its variable
                // was made, a type parameter only outside a macro expansion.
                let named_term = named.as_deref().map(|named| {
                    term(named.named(interner, |span, param| match param {
                        hir_def::GenericParamId::TypeParamId(_) => {
                            span_syntax(span).is_none_or(|span| !span.file_id.is_macro())
                        }
                        _ => true,
                    }))
                });
                TypeMustBeKnown {
                    at_point,
                    top_term,
                    kind,
                    ambiguity,
                    origin,
                    named_term,
                    cause: cause.into(),
                }
                .into()
            }
            &InferenceDiagnostic::UnionExprMustHaveExactlyOneField { expr } => {
                let expr = expr_syntax(expr)?;
                UnionExprMustHaveExactlyOneField { expr }.into()
            }
            InferenceDiagnostic::TypeMismatch { node, expected, found, at_error, cause } => {
                let expr_or_pat = expr_or_pat_syntax(node.unpack())?;
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                let at_error = at_error.as_deref().map(|(expected, found)| {
                    (
                        // alloc: error path, a reported diagnostic's types as rustc prints them
                        expected.as_ref().display(db, target).to_string(),
                        // alloc: error path, as above
                        found.as_ref().display(db, target).to_string(),
                    )
                });
                TypeMismatch {
                    at_error,
                    cause: (*cause).into(),
                    expr_or_pat,
                    expected: Type {
                        owner: type_owner,
                        ty: EarlyBinder::bind(interner, expected.as_ref()),
                    },
                    actual: Type {
                        owner: type_owner,
                        ty: EarlyBinder::bind(interner, found.as_ref()),
                    },
                }
                .into()
            }
            InferenceDiagnostic::SolverDiagnostic(d) => {
                let span = span_syntax(d.span)?;
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                Self::solver_diagnostic(db, &d.kind, span, d.cause.into(), type_owner, target)?
            }
            InferenceDiagnostic::ExplicitDropMethodUse { kind } => {
                let expr_or_path = match kind {
                    ExplicitDropMethodUseKind::MethodCall(expr) => {
                        let expr = expr_syntax(*expr)?;
                        let expr = expr.with_value(expr.value.cast::<ast::MethodCallExpr>()?);
                        Either::Left(expr)
                    }
                    ExplicitDropMethodUseKind::Path(path_expr_id) => {
                        let syntax = expr_or_pat_syntax(path_expr_id.unpack())?;
                        let file_id = syntax.file_id;
                        let syntax =
                            syntax.with_value(syntax.value.cast::<ast::PathExpr>()?).to_view(db);
                        let path = syntax.path()?;
                        let path = InFile::new(file_id, AstPtr::of(path));
                        Either::Right(path)
                    }
                };
                ExplicitDropMethodUse { expr_or_path }.into()
            }
            InferenceDiagnostic::MutableRefBinding { pat } => {
                let pat = pat_syntax(*pat)?.map(Into::into);
                MutableRefBinding { pat }.into()
            }
            &InferenceDiagnostic::YieldOutsideCoroutine { expr } => {
                YieldOutsideCoroutine { expr: expr_syntax(expr)? }.into()
            }
            &InferenceDiagnostic::ReturnOutsideFunction { expr, kind } => {
                ReturnOutsideFunction { expr: expr_syntax(expr)?, kind }.into()
            }
            InferenceDiagnostic::InvalidConstArg { site, reason } => {
                InvalidConstArg { site: span_syntax(*site)?, reason: reason.clone() }.into()
            }
            InferenceDiagnostic::NotAStructPath { node, found, ty } => NotAStructPath {
                node: expr_or_pat_syntax(node.unpack())?,
                found: *found,
                ty: ty.as_ref().map(|it| new_ty(it.as_ref())),
            }
            .into(),
            InferenceDiagnostic::AutoderefRecursionLimit { span, ty, suggested_limit } => {
                AutoderefRecursionLimit {
                    site: span_syntax(*span)?,
                    ty: new_ty(ty.as_ref()),
                    suggested_limit: *suggested_limit,
                }
                .into()
            }
            InferenceDiagnostic::MethodOnUnknownRawPointee { span } => {
                MethodOnUnknownRawPointee { site: span_syntax(*span)? }.into()
            }
            InferenceDiagnostic::TyvarBehindRawPointer { span } => {
                TyvarBehindRawPointer { site: span_syntax(*span)? }.into()
            }
            &InferenceDiagnostic::InheritedRefMutabilityMismatch { pat } => {
                InheritedRefMutabilityMismatch { pat: pat_syntax(pat)? }.into()
            }
            &InferenceDiagnostic::FnTraitArgsNotTuple { call_expr } => {
                FnTraitArgsNotTuple { call: expr_syntax(call_expr)? }.into()
            }
            InferenceDiagnostic::SelfCtorNotTupleOrUnit { expr, self_ty } => {
                SelfCtorNotTupleOrUnit {
                    expr: expr_syntax(*expr)?,
                    self_ty: new_ty(self_ty.as_ref()),
                }
                .into()
            }
            InferenceDiagnostic::UnsafeBinderCastOfNonBinder { expr, kind, found } => {
                UnsafeBinderCastOfNonBinder {
                    expr: expr_syntax(*expr)?,
                    kind: *kind,
                    found: new_ty(found.as_ref()),
                }
                .into()
            }
            &InferenceDiagnostic::RecordMissingFields { record, variant, ref missed_fields } => {
                let record = expr_or_pat_syntax(record)?;
                let file = record.file_id;
                let variant_data = variant.fields(db);
                let missed_fields = missed_fields
                    .iter()
                    .map(|&idx| {
                        (variant_data.fields()[idx].name, Field { parent: variant.into(), id: idx })
                    })
                    // alloc: error path, a reported diagnostic
                    .collect();
                match record.to_view(db) {
                    Either::Left(view::Expr::RecordExpr(record_expr))
                        if record_expr.record_expr_field_list().is_some() =>
                    {
                        let field_list_parent_path = record_expr.path().map(AstPtr::of);
                        return Some(
                            MissingFields {
                                file,
                                field_list_parent: AstPtr::of(Either::Left(record_expr)),
                                field_list_parent_path,
                                variant: variant.into(),
                                missed_fields,
                            }
                            .into(),
                        );
                    }
                    Either::Right(view::Pat::RecordPat(record_pat))
                        if record_pat.record_pat_field_list().is_some() =>
                    {
                        let field_list_parent_path = record_pat.path().map(AstPtr::of);
                        MissingFields {
                            file,
                            field_list_parent: AstPtr::of(Either::Right(record_pat)),
                            field_list_parent_path,
                            variant: variant.into(),
                            missed_fields,
                        }
                        .into()
                    }
                    _ => return None,
                }
            }
        })
    }

    fn solver_diagnostic(
        db: &'db Db,
        d: &'db SolverDiagnosticKind,
        span: SpanSyntax,
        cause: ObligationCauseCode,
        type_owner: TypeOwnerId,
        target: DisplayTarget,
    ) -> Option<AnyDiagnostic<'db>> {
        let interner = DbInterner::new_no_crate(db);
        let ty = |ty: &'db hir_ty::next_solver::StoredTy| Type {
            owner: type_owner,
            ty: EarlyBinder::bind(interner, ty.as_ref()),
        };
        let goal = |goal| Self::solver_goal(db, goal, type_owner, target);
        let error = |kind| -> AnyDiagnostic<'db> { TraitSolverError { span, cause, kind }.into() };
        Some(match d {
            &SolverDiagnosticKind::GoalUnmet { goal: ref unmet, non_const_impl } => {
                error(TraitSolverErrorKind::GoalUnmet {
                    goal: goal(unmet),
                    non_const_impl: non_const_impl.map(|id| crate::Impl { id }),
                })
            }
            SolverDiagnosticKind::Overflow { goal: overflowed, suggest_increasing_limit } => {
                error(TraitSolverErrorKind::Overflow {
                    goal: goal(overflowed),
                    suggest_increasing_limit: *suggest_increasing_limit,
                })
            }
            &SolverDiagnosticKind::ProjectionMismatch { goal: ref projection, ref normalized, impl_ } => {
                let normalized = normalized.as_ref().map(|arg| match arg.as_ref().kind() {
                    rustc_type_ir::GenericArgKind::Type(it) => {
                        Either::Left(Type { owner: type_owner, ty: EarlyBinder::bind(interner, it) })
                    }
                    // alloc: error path, a reported diagnostic's const as rustc prints it
                    _ => Either::Right(arg.as_ref().display(db, target).to_string()),
                });
                error(TraitSolverErrorKind::ProjectionMismatch {
                    goal: goal(projection),
                    normalized,
                    impl_: impl_.map(|id| crate::Impl { id }),
                })
            }
            SolverDiagnosticKind::TypesDiffer { expected, found } => {
                error(TraitSolverErrorKind::TypesDiffer { expected: ty(expected), found: ty(found) })
            }
            SolverDiagnosticKind::ConstArgHasWrongType { ct, ct_ty, expected_ty } => {
                error(TraitSolverErrorKind::ConstArgHasWrongType {
                    // alloc: error path, a reported diagnostic's const as rustc prints it
                    ct: ct.as_ref().display(db, target).to_string(),
                    ct_ty: ty(ct_ty),
                    expected_ty: ty(expected_ty),
                })
            }
            SolverDiagnosticKind::HigherRankedOutlives => {
                error(TraitSolverErrorKind::HigherRankedOutlives)
            }
            &SolverDiagnosticKind::Unreported { code } => {
                error(TraitSolverErrorKind::Unreported(code))
            }
            SolverDiagnosticKind::TraitUnimplemented {
                trait_predicate,
                parent_trait_predicates,
                parent_impls,
            } => {
                let trait_predicate =
                    crate::TraitClause::new(interner, type_owner, trait_predicate.get(interner));
                let parent_trait_predicates = parent_trait_predicates
                    .iter()
                    .map(|trait_predicate| {
                        crate::TraitClause::new(interner, type_owner, trait_predicate.get(interner))
                    })
                    // alloc: error path, the diagnostic's own list
                    .collect();
                let parent_impls_of = parent_impls;
                let parent_impls = parent_impls_of
                    .iter()
                    .map(|&it| it.map(|(impl_, _)| crate::Impl::from(impl_)))
                    // alloc: error path, the diagnostic's own list
                    .collect();
                let parent_impl_bounds = parent_impls_of
                    .iter()
                    .map(|&it| match it? {
                        (AnyImplId::ImplId(impl_), bound_index) => {
                            bound_source(db, impl_.into(), bound_index)
                        }
                        (AnyImplId::BuiltinDeriveImplId(_), _) => None,
                    })
                    // alloc: error path, the diagnostic's own list
                    .collect();
                UnimplementedTrait {
                    span,
                    cause,
                    trait_predicate,
                    parent_trait_predicates,
                    parent_impls,
                    parent_impl_bounds,
                }
                .into()
            }
            &SolverDiagnosticKind::ClosureKindMismatch { expected, found, is_async, ref closure } => {
                let fn_trait = |kind| match (kind, is_async) {
                    (rustc_type_ir::ClosureKind::Fn, false) => crate::FnTrait::Fn,
                    (rustc_type_ir::ClosureKind::FnMut, false) => crate::FnTrait::FnMut,
                    (rustc_type_ir::ClosureKind::FnOnce, false) => crate::FnTrait::FnOnce,
                    (rustc_type_ir::ClosureKind::Fn, true) => crate::FnTrait::AsyncFn,
                    (rustc_type_ir::ClosureKind::FnMut, true) => crate::FnTrait::AsyncFnMut,
                    (rustc_type_ir::ClosureKind::FnOnce, true) => crate::FnTrait::AsyncFnOnce,
                };
                ClosureKindMismatch {
                    span,
                    cause,
                    expected: fn_trait(expected),
                    found: fn_trait(found),
                    closure: Self::closure_syntax(db, closure.as_ref()),
                }
                .into()
            }
        })
    }

    /// The syntax of the closure `ty` is the type of, in whichever body it is written.
    fn closure_syntax(
        db: &'db Db,
        ty: hir_ty::next_solver::Ty<'db>,
    ) -> Option<InFile<AstPtr<ast::ClosureExpr>>> {
        let hir_ty::db::InternedClosure { owner, expr, .. } = match ty.kind() {
            TyKind::Closure(id, _) => id.0.loc(db),
            TyKind::CoroutineClosure(id, _) => id.0.loc(db),
            _ => return None,
        };
        let (_, source_map) = ExpressionStore::with_source_map(db, owner.expression_store_owner(db));
        let ptr = source_map.expr_syntax(expr).ok()?;
        Some(ptr.with_value(ptr.value.cast::<ast::ClosureExpr>()?))
    }

    fn ambiguous_goal(
        db: &'db Db,
        note: &'db solver_errors::AmbiguityNote,
        type_owner: TypeOwnerId,
        target: DisplayTarget,
    ) -> AmbiguousGoal<'db> {
        let interner = DbInterner::new_no_crate(db);
        let candidates = note
            .candidates
            .iter()
            .map(|candidate| match candidate {
                &solver_errors::AmbiguityCandidate::Impl(id) => {
                    AmbiguityCandidate::Impl(crate::Impl { id })
                }
                solver_errors::AmbiguityCandidate::ParamEnv { trait_, args } => {
                    let trait_ref = hir_ty::next_solver::TraitRef::new_from_args(
                        interner,
                        (*trait_).into(),
                        args.as_ref(),
                    );
                    AmbiguityCandidate::WhereClause(crate::TraitRef::new(
                        interner, type_owner, trait_ref,
                    ))
                }
            })
            // alloc: error path, a reported ambiguity's candidates
            .collect();
        AmbiguousGoal { goal: Self::solver_goal(db, &note.goal, type_owner, target), candidates }
    }

    fn solver_goal(
        db: &'db Db,
        goal: &'db StoredGoal,
        type_owner: TypeOwnerId,
        target: DisplayTarget,
    ) -> SolverGoal<'db> {
        let interner = DbInterner::new_no_crate(db);
        let ty = |ty: &'db hir_ty::next_solver::StoredTy| Type {
            owner: type_owner,
            ty: EarlyBinder::bind(interner, ty.as_ref()),
        };
        let text = |arg: &'db hir_ty::next_solver::StoredGenericArg| {
            // alloc: error path, a reported goal's argument as rustc prints it
            arg.as_ref().display(db, target).to_string()
        };
        let term = |arg: &'db hir_ty::next_solver::StoredGenericArg| match arg.as_ref().kind() {
            rustc_type_ir::GenericArgKind::Type(it) => Either::Left(Type {
                owner: type_owner,
                ty: EarlyBinder::bind(interner, it),
            }),
            _ => Either::Right(text(arg)),
        };
        let trait_ref = |trait_: hir_def::TraitId, args: &'db hir_ty::next_solver::StoredGenericArgs| {
            hir_ty::next_solver::TraitRef::new_from_args(interner, trait_.into(), args.as_ref())
        };
        match goal {
            &StoredGoal::Trait { trait_, ref args, polarity } => {
                let clause =
                    hir_ty::next_solver::TraitClause { trait_ref: trait_ref(trait_, args), polarity };
                SolverGoal::Trait(crate::TraitClause::new(interner, type_owner, clause))
            }
            &StoredGoal::HostEffect { trait_, ref args, maybe_const } => SolverGoal::HostEffect {
                trait_ref: crate::TraitRef::new(interner, type_owner, trait_ref(trait_, args)),
                maybe_const,
            },
            StoredGoal::Projection { alias, term: projected } => {
                SolverGoal::Projection { alias: term(alias), term: term(projected) }
            }
            StoredGoal::NormalizesTo { alias, term: _ } => {
                SolverGoal::NormalizesTo { alias: term(alias) }
            }
            StoredGoal::WellFormed { term: wf } => SolverGoal::WellFormed(term(wf)),
            StoredGoal::Subtype { a, b } => SolverGoal::Subtype { a: ty(a), b: ty(b) },
            StoredGoal::Coerce { a, b } => SolverGoal::Coerce { a: ty(a), b: ty(b) },
            StoredGoal::ConstEvaluatable { ct } => SolverGoal::ConstEvaluatable(text(ct)),
            StoredGoal::ConstArgHasType { ct, ty: of } => {
                SolverGoal::ConstArgHasType { ct: text(ct), ty: ty(of) }
            }
            StoredGoal::ConstEquate { a, b } => SolverGoal::ConstEquate { a: text(a), b: text(b) },
            &StoredGoal::DynCompatible { trait_ } => SolverGoal::DynCompatible(Trait { id: trait_ }),
            StoredGoal::UnstableFeature { feature } => SolverGoal::UnstableFeature(*feature),
            StoredGoal::Outlives { a, b } => SolverGoal::Outlives { a: term(a), b: text(b) },
            StoredGoal::Ambiguous => SolverGoal::Ambiguous,
        }
    }

    fn path_diagnostic(
        diag: &PathLoweringDiagnostic,
        path: InFile<view::Path<'_>>,
        db: &'db Db,
    ) -> Option<AnyDiagnostic<'db>> {
        Some(match *diag {
            PathLoweringDiagnostic::GenericArgsProhibited { ref segments, reason } => {
                let mut rtn = None;
                // alloc: an error's sites.
                let mut all_args = Vec::with_capacity(segments.len());
                for &segment in segments.iter() {
                    let segment = hir_segment_to_ast_segment_view(path.value, segment)?;
                    if let Some(it) = segment.return_type_syntax() {
                        // RTN errors are emitted as `GenericArgsProhibited` or `ParenthesizedGenericArgsWithoutFnTrait`.
                        rtn.get_or_insert(it);
                        continue;
                    }
                    let args = if let Some(generics) = segment.generic_arg_list() {
                        AstPtr::<ast::GenericArgList>::of(generics).wrap_left()
                    } else {
                        let list = segment.parenthesized_arg_list()?;
                        AstPtr::<ast::ParenthesizedArgList>::of(list).wrap_right()
                    };
                    all_args.push(path.with_value(args));
                }
                let Some(&args) = all_args.first() else {
                    return Some(BadRtn { rtn: path.with_value(AstPtr::of(rtn?)) }.into());
                };
                GenericArgsProhibited { args, all_args: all_args.into_boxed_slice(), reason }.into()
            }
            PathLoweringDiagnostic::ParenthesizedGenericArgsWithoutFnTrait { segment } => {
                let segment = hir_segment_to_ast_segment_view(path.value, segment)?;

                if let Some(rtn) = segment.return_type_syntax() {
                    // RTN errors are emitted as `GenericArgsProhibited` or `ParenthesizedGenericArgsWithoutFnTrait`.
                    return Some(BadRtn { rtn: path.with_value(AstPtr::of(rtn)) }.into());
                }

                let args = AstPtr::of(segment.parenthesized_arg_list()?);
                let args = path.with_value(args);
                ParenthesizedGenericArgsWithoutFnTrait { args }.into()
            }
            PathLoweringDiagnostic::IncorrectGenericsLen {
                generics_source,
                provided_count,
                expected_count,
                kind,
                def,
            } => {
                let generics_or_segment =
                    path_generics_source_to_ast(path.value, generics_source)?;
                let generics_or_segment = path.with_value(AstPtr::of(generics_or_segment));
                IncorrectGenericsLen {
                    generics_or_segment,
                    kind,
                    provided: provided_count,
                    expected: expected_count,
                    def: def.into(),
                }
                .into()
            }
            PathLoweringDiagnostic::IncorrectGenericsOrder {
                generics_source,
                param_id,
                arg_idx,
                has_self_arg,
            } => {
                let generic_args =
                    path_generics_source_to_ast(path.value, generics_source)?.left()?;
                let provided_arg = hir_generic_arg_to_ast_view(generic_args, arg_idx, has_self_arg)?;
                let provided_arg = path.with_value(AstPtr::of(provided_arg));
                let expected_kind = GenericArgKind::from_id(param_id);
                IncorrectGenericsOrder { provided_arg, expected_kind, param: param_id.into() }
                    .into()
            }
            PathLoweringDiagnostic::MissingLifetime { generics_source, expected_count, def }
            | PathLoweringDiagnostic::ElisionFailure { generics_source, expected_count, def } => {
                let generics_or_segment =
                    path_generics_source_to_ast(path.value, generics_source)?;
                let generics_or_segment = path.with_value(AstPtr::of(generics_or_segment));
                let kind = match diag {
                    PathLoweringDiagnostic::ElisionFailure { .. } => {
                        MissingLifetimeKind::ElisionFailure
                    }
                    _ => MissingLifetimeKind::Missing,
                };
                MissingLifetime {
                    generics_or_segment,
                    expected: expected_count,
                    def: def.into(),
                    kind,
                }
                .into()
            }
            PathLoweringDiagnostic::LateBoundLifetimeArgs { generics_source, def, hard_error } => {
                let generics_or_segment =
                    path_generics_source_to_ast(path.value, generics_source)?;
                let generics_or_segment = path.with_value(AstPtr::of(generics_or_segment));
                LateBoundLifetimeArgs { generics_or_segment, def: def.into(), hard_error }.into()
            }
            PathLoweringDiagnostic::AmbiguousAssociatedItem { segment, variant, item, impl_ } => {
                let ItemContainerId::TraitId(bound) = item.lookup(db).container else {
                    never!("an associated type of a trait has a trait");
                    return None;
                };
                AmbiguousAssociatedItem {
                    path: path.map(AstPtr::of),
                    variant: VariantId::from(variant).into(),
                    item: item.into(),
                    self_ty: crate::Impl::from(AnyImplId::ImplId(impl_)).self_ty(db),
                    bound: bound.into(),
                }
                .into()
            }
            PathLoweringDiagnostic::ElidedLifetimesInPath {
                generics_source,
                expected_count,
                def,
                hard_error,
            } => {
                let generics_or_segment =
                    path_generics_source_to_ast(path.value, generics_source)?;
                let generics_or_segment = path.with_value(AstPtr::of(generics_or_segment));
                ElidedLifetimesInPath {
                    generics_or_segment,
                    expected: expected_count,
                    def: def.into(),
                    hard_error,
                }
                .into()
            }
            PathLoweringDiagnostic::GenericDefaultRefersToSelf { segment } => {
                let segment = hir_segment_to_ast_segment_view(path.value, segment)?;
                let segment = path.with_value(AstPtr::of(segment));
                GenericDefaultRefersToSelf { segment }.into()
            }
            PathLoweringDiagnostic::AssocTypeNotResolved { segment, qself, failure } => {
                let segment = hir_segment_to_ast_segment_view(path.value, segment)?;
                let segment = path.with_value(AstPtr::of(segment));
                let qself = match qself {
                    hir_ty::AssocTypeQself::Param(id) => Either::Left(TypeParam { id }),
                    hir_ty::AssocTypeQself::Trait(id) => Either::Right(id.into()),
                };
                AssocTypeNotResolved { segment, qself, failure }.into()
            }
            PathLoweringDiagnostic::AmbiguousAssocType { segment } => {
                let segment = hir_segment_to_ast_segment_view(path.value, segment)?;
                AmbiguousAssocType { segment: path.with_value(AstPtr::of(segment)) }.into()
            }
        })
    }

    fn expr_syntax(
        expr: ExprId,
        source_map: &ExpressionStoreSourceMap,
    ) -> Option<InFile<ExprOrPatPtr>> {
        source_map.expr_syntax(expr).ok()
    }

    fn pat_syntax(
        pat: PatId,
        source_map: &ExpressionStoreSourceMap,
    ) -> Option<InFile<ExprOrPatPtr>> {
        source_map.pat_syntax(pat).ok()
    }

    fn type_syntax(
        type_ref: TypeRefId,
        source_map: &ExpressionStoreSourceMap,
    ) -> Option<InFile<AstPtr<ast::Type>>> {
        source_map.type_syntax(type_ref).ok()
    }

    pub(crate) fn span_syntax(
        span: hir_ty::Span,
        source_map: &ExpressionStoreSourceMap,
    ) -> Option<InFile<AstPtr<SpanAst>>> {
        Some(match span {
            hir_ty::Span::ExprId(idx) => Self::expr_syntax(idx, source_map)?.map(|it| it.upcast()),
            hir_ty::Span::PatId(idx) => Self::pat_syntax(idx, source_map)?.map(|it| it.upcast()),
            hir_ty::Span::TypeRefId(idx) => {
                Self::type_syntax(idx, source_map)?.map(|it| it.upcast())
            }
            hir_ty::Span::BindingId(idx) => {
                let &pat = source_map.patterns_for_binding(idx).first()?;
                Self::pat_syntax(pat, source_map)?.map(|it| it.upcast())
            }
            hir_ty::Span::Dummy => return None,
        })
    }

    fn ty_diagnostic(
        diag: &TyLoweringDiagnostic,
        source_map: &ExpressionStoreSourceMap,
        db: &'db Db,
    ) -> Option<AnyDiagnostic<'db>> {
        Some(match diag {
            TyLoweringDiagnostic::PathDiagnostic { source, diag } => {
                let source = Self::type_syntax(*source, source_map)?;
                let view::Type::PathType(syntax) = source.to_view(db) else { return None };
                Self::path_diagnostic(diag, source.with_value(syntax.path()?), db)?
            }
            TyLoweringDiagnostic::InferVarsNotAllowed { sources } => {
                let mut nodes = sources
                    .iter()
                    .map(|&source| Some(Self::span_syntax(source, source_map)?.map(Into::into)))
                    .collect::<Option<Box<[InFile<SyntaxNodePtr>]>>>()?;
                nodes.sort_by_key(|it| it.value.text_range().start());
                InferVarsNotAllowed { node: *nodes.first()?, nodes }.into()
            }
            TyLoweringDiagnostic::MissingLifetimes { sites, kind } => {
                let sites = sites
                    .iter()
                    .map(|&site| Self::missing_lifetime_site(site, source_map, db))
                    .collect::<Option<Box<[_]>>>()?;
                MissingLifetimes { sites, kind: *kind }.into()
            }
            TyLoweringDiagnostic::InvalidConstArg { site, reason } => InvalidConstArg {
                site: Self::span_syntax(*site, source_map)?,
                reason: reason.clone(),
            }
            .into(),
            TyLoweringDiagnostic::ImplTraitNotAllowed { source } => {
                ImplTraitNotAllowed { ty: Self::type_syntax(*source, source_map)? }.into()
            }
            TyLoweringDiagnostic::AdditionalNonAutoTraits { first, additional } => {
                AdditionalNonAutoTraits {
                    first: Self::type_syntax(*first, source_map)?,
                    additional: additional
                        .iter()
                        .map(|&it| Self::type_syntax(it, source_map))
                        .collect::<Option<_>>()?,
                }
                .into()
            }
            TyLoweringDiagnostic::ConflictingAssocTypeBindings { source, assoc } => {
                ConflictingAssocTypeBindings {
                    bound: Self::type_syntax(*source, source_map)?,
                    assoc: (*assoc).into(),
                }
                .into()
            }
            TyLoweringDiagnostic::MissingSelfDefaultParams { source, trait_, params } => {
                MissingSelfDefaultParams {
                    bound: Self::type_syntax(*source, source_map)?,
                    trait_: (*trait_).into(),
                    // alloc: error path, a reported diagnostic
                    params: params.iter().map(|&id| TypeParam { id }).collect(),
                }
                .into()
            }
            TyLoweringDiagnostic::ObjectWithoutTrait { source } => {
                ObjectWithoutTrait { bound: Self::type_syntax(*source, source_map)? }.into()
            }
            TyLoweringDiagnostic::PatternTypeNotLowered { source, reason } => {
                PatternTypeNotLowered { ty: Self::type_syntax(*source, source_map)?, reason: *reason }
                    .into()
            }
        })
    }

    fn missing_lifetime_site(
        site: ElidedLifetimeSite,
        source_map: &ExpressionStoreSourceMap,
        db: &'db Db,
    ) -> Option<MissingLifetimeSite> {
        Some(match site {
            ElidedLifetimeSite::Reference(type_ref) => {
                let source = Self::type_syntax(type_ref, source_map)?;
                let source = source.map(|it| it.cast::<ast::RefType>()).transpose()?;
                MissingLifetimeSite::Reference(source)
            }
            ElidedLifetimeSite::Placeholder(lifetime) => {
                let source = source_map.lifetime_syntax(lifetime).ok()?;
                MissingLifetimeSite::Placeholder(source)
            }
            ElidedLifetimeSite::Path { source, generics_source, def, expected_count } => {
                let source = Self::type_syntax(source, source_map)?;
                let view::Type::PathType(syntax) = source.to_view(db) else { return None };
                let generics_or_segment =
                    path_generics_source_to_ast(syntax.path()?, generics_source)?;
                MissingLifetimeSite::Path {
                    generics_or_segment: source.with_value(AstPtr::of(generics_or_segment)),
                    expected: expected_count,
                    def: def.into(),
                }
            }
        })
    }
}

/// A body's source map, asked for the first time a diagnostic of the body is placed. The map
/// holds file positions, so it is made again on every edit before the body in its file; a
/// body with nothing to report never reads it (docs/v2/plan/lanes/firewall/notes.md).
struct Placing<'db> {
    db: &'db Db,
    def: DefWithBodyId,
    map: std::cell::OnceCell<&'db hir_def::expr_store::BodySourceMap>,
}

impl<'db> Placing<'db> {
    fn new(db: &'db Db, def: DefWithBodyId) -> Placing<'db> {
        Placing { db, def, map: std::cell::OnceCell::new() }
    }

    fn get(&self) -> &'db hir_def::expr_store::BodySourceMap {
        self.map.get_or_init(|| &Body::with_source_map(self.db, self.def).1)
    }
}

/// Whether lowering `def`'s signature store found anything: read from the map counted from the
/// signature's own syntax (`relative`), which an edit elsewhere in its file leaves as it was. A
/// signature a macro writes has no such syntax, and its one map is in the macro's file.
/// Whether a macro call lowered into `store` failed to expand (`compile_error!` in a body or a
/// type): its error is placed with the store's diagnostics, so the store's map is read.
fn store_macro_failed(db: &Db, store: &ExpressionStore) -> bool {
    store.macro_calls().iter().any(|call| call.parse_macro_expansion_error(db).is_some())
}

fn signature_store_has_diagnostics(db: &Db, def: GenericDefId) -> bool {
    use hir_def::signatures::{EnumSignature, StaticSignature, StructSignature, UnionSignature};
    macro_rules! of {
        ($Sig:ident, $id:expr) => {
            match $Sig::relative(db, $id) {
                Some((_, map)) => !map.diagnostics().is_empty(),
                None => !$Sig::with_source_map(db, $id).1.diagnostics().is_empty(),
            }
        };
    }
    match def {
        GenericDefId::AdtId(AdtId::EnumId(id)) => of!(EnumSignature, id),
        GenericDefId::AdtId(AdtId::StructId(id)) => of!(StructSignature, id),
        GenericDefId::AdtId(AdtId::UnionId(id)) => of!(UnionSignature, id),
        GenericDefId::ConstId(id) => of!(ConstSignature, id),
        GenericDefId::FunctionId(id) => of!(FunctionSignature, id),
        GenericDefId::ImplId(id) => of!(ImplSignature, id),
        GenericDefId::StaticId(id) => of!(StaticSignature, id),
        GenericDefId::TraitId(id) => of!(TraitSignature, id),
        GenericDefId::TypeAliasId(id) => of!(TypeAliasSignature, id),
    }
}

/// The syntax of `span` in the anonymous constant `def`'s MIR, read from the store it is
/// written in; a span of no expression, pattern or binding is the constant's own expression.
fn anon_const_node(
    db: &Db,
    source_map: &ExpressionStoreSourceMap,
    def: AnonConstId<'_>,
    span: hir_ty_mir::mir::MirSpan,
) -> Option<InFile<SyntaxNodePtr>> {
    use hir_ty_mir::mir::MirSpan;
    let source = match span {
        MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => source_map.expr_syntax(expr),
        MirSpan::PatId(pat) => source_map.pat_syntax(pat),
        MirSpan::BindingId(binding) => {
            source_map.pat_syntax(*source_map.patterns_for_binding(binding).first()?)
        }
        _ => source_map.expr_syntax(def.loc(db).expr),
    };
    Some(source.ok()?.map(|it| it.syntax_node_ptr()))
}

/// What [`ConstEvalFailed`] reports of `error`, the failed evaluation of the value `own`, and
/// where in that value it failed when known. `None` when the error is one reported at its own
/// place: code that does not type or resolve, in the value or in a function it calls.
fn const_eval_failure<'db>(
    db: &'db Db,
    own: InferBodyId<'db>,
    error: &hir_ty::consteval::ConstEvalError<'db>,
    display_target: DisplayTarget,
) -> Option<(ConstEvalFailure, Option<hir_ty_mir::mir::MirSpan>)> {
    use hir_ty::{
        consteval::ConstEvalError,
        mir::{MirEvalError, MirLowerError},
    };
    use hir_ty_mir::mir_pretty::ConstEvalErrorPretty as _;
    let reported = |error: &MirLowerError<'_>| {
        matches!(
            error,
            MirLowerError::HasErrors
                | MirLowerError::IncompleteExpr
                | MirLowerError::IncompletePattern
                | MirLowerError::UnresolvedName { .. }
                | MirLowerError::RecordLiteralWithoutPath
                | MirLowerError::UnresolvedMethod(_)
                | MirLowerError::UnresolvedField
                | MirLowerError::TypeError(_)
                | MirLowerError::ContinueWithoutLoop
                | MirLowerError::BreakWithoutLoop
                | MirLowerError::UnresolvedLabel
        )
    };
    let unsupported = || {
        // alloc: error path, the evaluator's account of what it cannot do
        let mut printed = String::new();
        _ = error.pretty_print(&mut printed, db, |_, _| String::new(), display_target); // alloc: error path, no place text
        // alloc: error path, the same on one line
        let mut text = String::new();
        for line in printed.lines().map(str::trim) {
            if line.is_empty() || line == "Mir eval error:" {
                continue;
            }
            if !text.is_empty() {
                text.push_str("; ");
            }
            text.push_str(line);
        }
        ConstEvalFailure::Unsupported(text)
    };
    let mut err = match error {
        ConstEvalError::MirLowerError(MirLowerError::Loop) => {
            return Some((ConstEvalFailure::Cycle, None));
        }
        ConstEvalError::MirLowerError(error) if reported(error) => return None,
        ConstEvalError::MirLowerError(_) => return Some((unsupported(), None)),
        ConstEvalError::MirEvalError(error) => error,
    };
    let mut span = None;
    while let MirEvalError::InFunction(inner, stack) = err {
        if span.is_none() {
            span = stack.iter().find(|&&(_, _, body)| body == own).map(|&(_, at, _)| at);
        }
        err = inner;
    }
    let at_own = |at: &Option<(InferBodyId<'db>, hir_ty_mir::mir::MirSpan)>| {
        at.and_then(|(body, at)| (body == own).then_some(at))
    };
    let failure = match err {
        MirEvalError::StackOverflow => ConstEvalFailure::Failed(
            // alloc: error path, a reported diagnostic's message
            "reached the configured maximum number of stack frames".to_owned(),
        ),
        // alloc: error path, a reported diagnostic's message
        MirEvalError::Panic(message) => ConstEvalFailure::Failed(format!("evaluation panicked: {message}")),
        MirEvalError::Aborted(message) => ConstEvalFailure::Failed(
            // alloc: error path, a reported diagnostic's message
            format!("the evaluated program aborted execution: {message}"),
        ),
        // clone: error path, a reported diagnostic's message
        MirEvalError::UndefinedBehavior(message) => ConstEvalFailure::Failed(message.clone()),
        MirEvalError::OverBudget { spent, at } => {
            span = span.or_else(|| at_own(at));
            // alloc: error path, a reported diagnostic's message
            ConstEvalFailure::Failed(format!(
                "constant evaluation {spent} (raise {} under `[workspace.metadata.blaze]`)",
                spent.setting()
            ))
        }
        MirEvalError::LongRunning { at } => {
            span = span.or_else(|| at_own(at));
            // alloc: error path, a reported diagnostic's message
            ConstEvalFailure::Failed("constant evaluation is taking a long time".to_owned())
        }
        MirEvalError::HeapInValue { of_const_allocate: true } => ConstEvalFailure::Failed(
            // alloc: error path, a reported diagnostic's message
            "encountered `const_allocate` pointer in final value that was not made global".to_owned(),
        ),
        MirEvalError::HeapInValue { of_const_allocate: false } => ConstEvalFailure::Failed(
            // alloc: error path, a reported diagnostic's message
            "encountered dangling pointer in final value of constant".to_owned(),
        ),
        MirEvalError::ConstEvalError(name, inner) => {
            let (failure, inner_span) = const_eval_failure(db, own, inner, display_target)?;
            span = span.or(inner_span);
            match failure {
                ConstEvalFailure::Failed(reason) => {
                    // alloc: error path, a reported diagnostic's message
                    ConstEvalFailure::Failed(format!("evaluation of constant `{name}` failed: {reason}"))
                }
                ConstEvalFailure::Cycle => ConstEvalFailure::Cycle,
                ConstEvalFailure::Unsupported(reason) => {
                    // alloc: error path, a reported diagnostic's message
                    ConstEvalFailure::Unsupported(format!("constant `{name}`: {reason}"))
                }
            }
        }
        MirEvalError::MirLowerError(_, error) | MirEvalError::MirLowerErrorForClosure(_, error)
            if reported(error) =>
        {
            return None;
        }
        MirEvalError::InFunction(..) => unreachable!("every `InFunction` was taken off above"),
        MirEvalError::LayoutError(..)
        | MirEvalError::TargetDataLayoutNotAvailable(_)
        | MirEvalError::MirLowerError(..)
        | MirEvalError::MirLowerErrorForClosure(..)
        | MirEvalError::TypeIsUnsized(..)
        | MirEvalError::NotSupported(_)
        | MirEvalError::InvalidConst
        | MirEvalError::MachineStop
        | MirEvalError::InvalidVTableId(_)
        | MirEvalError::CoerceUnsizedError(_)
        | MirEvalError::InternalError(_) => unsupported(),
    };
    Some((failure, span))
}

/// The text of the type checking diagnostic `d` of `def` that
/// [`AnyDiagnostic::inference_diagnostic`] could not place: rustc's code and message, the
/// binding it is of where it has one, and the item it is in.
fn unplaced_inference_text<'db>(
    db: &'db Db,
    target: DisplayTarget,
    def: InferBodyId<'db>,
    d: &'db InferenceDiagnostic,
) -> String {
    let ty = |ty: &'db StoredTy| ty.as_ref().display(db, target).to_string();
    let seen = |found: &'db StoredTy, at_error: &'db Option<StoredTy>| ty(at_error.as_ref().unwrap_or(found));
    let fn_name = |f: FunctionId| FunctionSignature::of(db, f).name.as_str().to_owned();
    // alloc: each arm, error path, the error's text
    let what = match d {
        InferenceDiagnostic::NoSuchField { field, .. } => match field.unpack() {
            ExprOrPatId::ExprId(_) => "E0560: the struct literal names a field the struct does not have".to_owned(),
            ExprOrPatId::PatId(_) => "E0026: the pattern names a field the struct does not have".to_owned(),
        },
        &InferenceDiagnostic::MismatchedArrayPatLen { expected, found, has_rest, .. } => match has_rest {
            true => format!("E0528: pattern requires at least {found} elements but array has {expected}"),
            false => format!("E0527: pattern requires {found} elements but array has {expected}"),
        },
        InferenceDiagnostic::ArrayPatternWithoutFixedLength { .. } => {
            "E0730: cannot pattern-match on an array without a fixed length".to_owned()
        }
        InferenceDiagnostic::ExpectedArrayOrSlicePat { found, .. } => {
            format!("E0529: expected an array or slice, found `{}`", ty(found))
        }
        InferenceDiagnostic::InvalidRangePatType { .. } => {
            "E0029: only `char` and numeric types are allowed in range patterns".to_owned()
        }
        InferenceDiagnostic::DuplicateField { field, .. } => match field.unpack() {
            ExprOrPatId::ExprId(_) => "E0062: field specified more than once".to_owned(),
            ExprOrPatId::PatId(_) => "E0025: field bound multiple times in the pattern".to_owned(),
        },
        InferenceDiagnostic::PrivateField { .. } => "E0616: the field read is private".to_owned(),
        InferenceDiagnostic::PrivateAssocItem { .. } => "E0624: the associated item used is private".to_owned(),
        InferenceDiagnostic::UnresolvedField { receiver, name, .. } => {
            format!("E0609: no field `{}` on type `{}`", name.as_str(), ty(receiver))
        }
        InferenceDiagnostic::UnresolvedMethodCall { receiver, name, .. } => {
            format!("E0599: no method named `{}` found for `{}`", name.as_str(), ty(receiver))
        }
        InferenceDiagnostic::UnresolvedAssocItem { ty: on, name, .. } => {
            format!("E0599: no associated item named `{}` found for `{}`", name.as_str(), ty(on))
        }
        InferenceDiagnostic::UnresolvedIdent { .. } => "E0425: cannot find a value of this name in this scope".to_owned(),
        InferenceDiagnostic::UnresolvedPath { .. } => "E0433: failed to resolve a path".to_owned(),
        &InferenceDiagnostic::BreakOutsideOfLoop { is_break, .. } => {
            format!("E0268: `{}` outside of a loop", if is_break { "break" } else { "continue" })
        }
        &InferenceDiagnostic::UnlabeledJumpInLabeledBlock { is_break, .. } => {
            format!("E0695: unlabeled `{}` inside of a labeled block", if is_break { "break" } else { "continue" })
        }
        InferenceDiagnostic::AsyncCoroutinesNotSupported { .. } => {
            "E0727: `async` coroutines are not yet supported".to_owned()
        }
        InferenceDiagnostic::NonExhaustiveRecordExpr { .. } => {
            "E0639: cannot create non-exhaustive struct using struct expression".to_owned()
        }
        InferenceDiagnostic::NonExhaustiveRecordPat { .. } => {
            "E0638: a pattern of a `#[non_exhaustive]` struct needs `..`".to_owned()
        }
        InferenceDiagnostic::UnionPatMustHaveExactlyOneField { .. } => {
            "union patterns should have exactly one field".to_owned()
        }
        InferenceDiagnostic::UnionPatHasRest { .. } => "`..` cannot be used in union patterns".to_owned(),
        InferenceDiagnostic::FunctionalRecordUpdateOnNonStruct { .. } => {
            "E0436: functional record update syntax requires a struct".to_owned()
        }
        &InferenceDiagnostic::MismatchedArgCount { expected, found, is_fn_trait_call, .. } => format!(
            "{}: this function takes {expected} arguments but {found} were supplied",
            if is_fn_trait_call { "E0057" } else { "E0061" }
        ),
        InferenceDiagnostic::IncorrectArguments { .. } => "E0308: arguments to this function are incorrect".to_owned(),
        &InferenceDiagnostic::MismatchedTupleStructPatArgCount { expected, found, .. } => format!(
            "E0023: this pattern has {found} fields, but the corresponding tuple variant has {expected} fields"
        ),
        InferenceDiagnostic::ExpectedFunction { found, .. } => format!("E0618: expected function, found `{}`", ty(found)),
        InferenceDiagnostic::CannotBeDereferenced { found, at_error, .. } => {
            format!("E0614: type `{}` cannot be dereferenced", seen(found, at_error))
        }
        InferenceDiagnostic::UnaryOperatorCannotBeApplied { op, found, at_error, .. } => {
            let op = match op {
                hir_def::hir::UnaryOp::Deref => "*",
                hir_def::hir::UnaryOp::Not => "!",
                hir_def::hir::UnaryOp::Neg => "-",
            };
            format!("E0600: cannot apply unary operator `{op}` to type `{}`", seen(found, at_error))
        }
        InferenceDiagnostic::BinaryOperatorCannotBeApplied { op, lhs, rhs, at_error, .. } => {
            let (lhs, rhs) = match at_error.as_deref() {
                Some((lhs, rhs)) => (ty(lhs), ty(rhs)),
                None => (ty(lhs), ty(rhs)),
            };
            format!("E0369: binary operation `{op}` cannot be applied to `{lhs}` and `{rhs}`")
        }
        InferenceDiagnostic::MutRefInImmRefPat { .. } => {
            "E0308: mismatched types: a `&mut` pattern matches a shared reference".to_owned()
        }
        InferenceDiagnostic::CannotImplicitlyDerefTraitObject { found, .. } => {
            format!("E0033: type `{}` cannot be dereferenced", ty(found))
        }
        InferenceDiagnostic::CannotIndexInto { found, at_error, .. } => {
            format!("E0608: cannot index into a value of type `{}`", seen(found, at_error))
        }
        InferenceDiagnostic::TypedHole { .. } => {
            "in expressions, `_` can only be used on the left-hand side of an assignment".to_owned()
        }
        InferenceDiagnostic::CastToUnsized { cast_ty, .. } => format!("E0620: cast to unsized type: `{}`", ty(cast_ty)),
        InferenceDiagnostic::InvalidCast { expr_ty, cast_ty, .. } => {
            format!("casting `{}` as `{}` is invalid", ty(expr_ty), ty(cast_ty))
        }
        InferenceDiagnostic::TyDiagnostic { .. } => "a type written in the body is invalid".to_owned(),
        InferenceDiagnostic::PathDiagnostic { .. } => "a path written in the body is invalid".to_owned(),
        &InferenceDiagnostic::MethodCallIncorrectGenericsLen { provided_count, expected_count, .. } => format!(
            "E0107: a method call is given {provided_count} generic arguments, but the method takes {expected_count}"
        ),
        &InferenceDiagnostic::MethodCallIllegalSizedBound { method, .. } => {
            format!("the `{}` method cannot be invoked on a trait object", fn_name(method))
        }
        &InferenceDiagnostic::EditionDependentMethodCall { method, .. } => {
            format!("the call of `{}` resolves to another method in a later edition", fn_name(method))
        }
        InferenceDiagnostic::FullyQualifiedCallFrom2018 { self_ty, .. } => {
            format!("the call on `{}` resolves to another trait's item with the 2021 prelude", ty(self_ty))
        }
        InferenceDiagnostic::MethodCallIncorrectGenericsOrder { .. } => {
            "E0747: the method call's generic arguments are in the wrong order".to_owned()
        }
        InferenceDiagnostic::MethodCallLateBoundLifetimeArgs { .. } => {
            "cannot specify lifetime arguments explicitly if late bound lifetime parameters are present".to_owned()
        }
        InferenceDiagnostic::InvalidLhsOfAssignment { op, .. } => {
            format!("{}: invalid left-hand side of assignment", if op.is_some() { "E0067" } else { "E0070" })
        }
        InferenceDiagnostic::TypeMustBeKnown { kind, top_term, .. } => {
            let code = match kind {
                TypeAnnotationNeeded::E0282 => "E0282",
                TypeAnnotationNeeded::E0283 => "E0283",
                TypeAnnotationNeeded::E0284 => "E0284",
            };
            match top_term {
                Some(term) => format!("{code}: type annotations needed for `{}`", term.as_ref().display(db, target)),
                None => format!("{code}: type annotations needed"),
            }
        }
        InferenceDiagnostic::UnionExprMustHaveExactlyOneField { .. } => {
            "E0784: union expressions should have exactly one field".to_owned()
        }
        InferenceDiagnostic::TypeMismatch { expected, found, at_error, .. } => {
            let (expected, found) = match at_error.as_deref() {
                Some((expected, found)) => (ty(expected), ty(found)),
                None => (ty(expected), ty(found)),
            };
            format!("E0308: mismatched types: expected `{expected}`, found `{found}`")
        }
        InferenceDiagnostic::SolverDiagnostic(it) => solver_text(db, target, def, &it.kind),
        InferenceDiagnostic::ExplicitDropMethodUse { .. } => "E0040: explicit use of destructor method".to_owned(),
        InferenceDiagnostic::MutableRefBinding { .. } => "binding cannot be both mutable and by-reference".to_owned(),
        InferenceDiagnostic::YieldOutsideCoroutine { .. } => {
            "E0627: yield expression outside of coroutine literal".to_owned()
        }
        InferenceDiagnostic::ReturnOutsideFunction { kind, .. } => match kind {
            ReturnKind::ReturnExpr => "E0572: return statement outside of function body".to_owned(),
            ReturnKind::BecomeExpr => "E0572: become statement outside of function body".to_owned(),
        },
        InferenceDiagnostic::UnsafeBinderCastOfNonBinder { found, .. } => {
            format!("`{}` is not an unsafe binder type (`unsafe<..> T`)", ty(found))
        }
        InferenceDiagnostic::RecordMissingFields { record, missed_fields, .. } => match record {
            ExprOrPatId::ExprId(_) => format!("E0063: {} fields missing in the struct literal", missed_fields.len()),
            ExprOrPatId::PatId(_) => format!("E0027: the pattern does not mention {} fields", missed_fields.len()),
        },
        InferenceDiagnostic::InvalidConstArg { .. } => "a const argument that is not a constant".to_owned(),
        InferenceDiagnostic::NotAStructPath { .. } => {
            "E0574: the path does not name a struct, variant or union of the kind needed here".to_owned()
        }
        InferenceDiagnostic::AutoderefRecursionLimit { ty: at, suggested_limit, .. } => format!(
            "E0055: reached the recursion limit while auto-dereferencing `{}` (consider `#![recursion_limit = \"{suggested_limit}\"]`)",
            ty(at)
        ),
        InferenceDiagnostic::MethodOnUnknownRawPointee { .. } => {
            "E0282: type annotations needed: cannot call a method on a raw pointer with an unknown pointee type"
                .to_owned()
        }
        InferenceDiagnostic::TyvarBehindRawPointer { .. } => {
            "type annotations needed: a method call on a raw pointer with an unknown pointee type".to_owned()
        }
        InferenceDiagnostic::InheritedRefMutabilityMismatch { .. } => {
            "E0308: mismatched types: cannot match inherited `&` with `&mut` pattern".to_owned()
        }
        InferenceDiagnostic::FnTraitArgsNotTuple { .. } => {
            "E0059: the arguments of a call through a function trait are not a tuple".to_owned()
        }
        InferenceDiagnostic::SelfCtorNotTupleOrUnit { self_ty, .. } => format!(
            "the `Self` constructor can only be used with tuple or unit structs, and `Self` is `{}`",
            ty(self_ty)
        ),
    };
    let place = match d {
        InferenceDiagnostic::SolverDiagnostic(it) => {
            let binding = match (it.span, def) {
                (hir_ty::Span::BindingId(binding), InferBodyId::DefWithBodyId(def)) => {
                    Some(Body::of(db, def)[binding].name.as_str())
                }
                _ => None,
            };
            match (binding, ObligationCauseCode::from(it.cause)) {
                (Some(name), ObligationCauseCode::SizedArgumentType) => format!(", for the parameter `{name}`"),
                (_, ObligationCauseCode::SizedReturnType) => ", for the return type".to_owned(),
                (Some(name), _) => format!(", for the binding `{name}`"),
                (None, _) => String::new(),
            }
        }
        _ => String::new(),
    };
    // alloc: error path, the error's text
    format!("{what}{place}, in {}", body_item_text(db, target, def))
}

/// rustc's code and message of the trait solver error `kind` in `def`.
fn solver_text<'db>(
    db: &'db Db,
    target: DisplayTarget,
    def: InferBodyId<'db>,
    kind: &'db SolverDiagnosticKind,
) -> String {
    let interner = DbInterner::new_no_crate(db);
    let ty = |ty: &'db StoredTy| ty.as_ref().display(db, target).to_string();
    let goal = |goal| goal_text(db, target, goal);
    // alloc: each arm, error path, the error's text
    match kind {
        SolverDiagnosticKind::TraitUnimplemented { trait_predicate, .. } => {
            let pred = trait_predicate.get(interner);
            let krate = hir_def::HasModule::krate(&def, db);
            let sized = hir_def::lang_item::lang_items(db, krate).Sized;
            if sized == Some(pred.def_id().0) && pred.polarity == rustc_type_ir::ClausePolarity::Positive {
                // `Sized`'s `#[rustc_on_unimplemented]` message.
                format!(
                    "E0277: the size for values of type `{}` cannot be known at compilation time (`{}`)",
                    pred.self_ty().display(db, target),
                    pred.display(db, target)
                )
            } else {
                format!("E0277: the trait bound `{}` is not satisfied", pred.display(db, target))
            }
        }
        SolverDiagnosticKind::ClosureKindMismatch { expected, found, .. } => format!(
            "E0525: expected a closure that implements the `{}` trait, but this closure only implements `{}`",
            expected.as_str(),
            found.as_str()
        ),
        SolverDiagnosticKind::GoalUnmet { goal: unmet, .. } => {
            format!("the requirement `{}` is not satisfied", goal(unmet))
        }
        SolverDiagnosticKind::Overflow { goal: overflowed, .. } => {
            format!("E0275: overflow evaluating the requirement `{}`", goal(overflowed))
        }
        SolverDiagnosticKind::ProjectionMismatch { goal: projection, .. } => {
            format!("E0271: type mismatch resolving `{}`", goal(projection))
        }
        SolverDiagnosticKind::TypesDiffer { expected, found } => {
            format!("E0308: mismatched types: expected `{}`, found `{}`", ty(expected), ty(found))
        }
        SolverDiagnosticKind::ConstArgHasWrongType { ct, expected_ty, .. } => {
            format!("the constant `{}` is not of type `{}`", ct.as_ref().display(db, target), ty(expected_ty))
        }
        SolverDiagnosticKind::HigherRankedOutlives => {
            "higher-ranked lifetime bound could not be satisfied".to_owned()
        }
        SolverDiagnosticKind::Unreported { code } => {
            let what = match code {
                solver_errors::UnreportedSolverError::Cycle => "a cycle",
                solver_errors::UnreportedSolverError::SignatureMismatch => "a signature mismatch",
                solver_errors::UnreportedSolverError::TraitDynIncompatible => "a trait that is not dyn compatible",
                solver_errors::UnreportedSolverError::NotConstEvaluatable => "a constant that cannot be evaluated",
                solver_errors::UnreportedSolverError::SelectionOverflow => "an overflow in selection",
                solver_errors::UnreportedSolverError::ConstEquate => "constants that differ",
            };
            format!("the trait solver found {what}, which only rustc's old solver reports")
        }
    }
}

/// `goal` as rustc prints a predicate.
fn goal_text<'db>(db: &'db Db, target: DisplayTarget, goal: &'db StoredGoal) -> String {
    let interner = DbInterner::new_no_crate(db);
    let arg = |arg: &'db hir_ty::next_solver::StoredGenericArg| arg.as_ref().display(db, target).to_string();
    let ty = |ty: &'db StoredTy| ty.as_ref().display(db, target).to_string();
    let trait_ref = |trait_: TraitId, args: &'db hir_ty::next_solver::StoredGenericArgs| {
        hir_ty::next_solver::TraitRef::new_from_args(interner, trait_.into(), args.as_ref())
    };
    let trait_name = |trait_: TraitId| TraitSignature::of(db, trait_).name.as_str().to_owned();
    // alloc: each arm, error path, the error's text
    match goal {
        &StoredGoal::Trait { trait_, ref args, polarity } => {
            let tr = trait_ref(trait_, args);
            let not = if polarity == rustc_type_ir::ClausePolarity::Negative { "!" } else { "" };
            format!("{}: {not}{}", tr.self_ty().display(db, target), tr.display(db, target))
        }
        &StoredGoal::HostEffect { trait_, ref args, maybe_const } => {
            let tr = trait_ref(trait_, args);
            let constness = if maybe_const { "[const]" } else { "const" };
            format!("{}: {constness} {}", tr.self_ty().display(db, target), tr.display(db, target))
        }
        StoredGoal::Projection { alias, term } | StoredGoal::NormalizesTo { alias, term } => {
            format!("{} == {}", arg(alias), arg(term))
        }
        StoredGoal::WellFormed { term } => format!("{} well-formed", arg(term)),
        StoredGoal::Subtype { a, b } => format!("{} <: {}", ty(a), ty(b)),
        StoredGoal::Coerce { a, b } => format!("{} coerces to {}", ty(a), ty(b)),
        StoredGoal::ConstEvaluatable { ct } => format!("the constant `{}` can be evaluated", arg(ct)),
        StoredGoal::ConstArgHasType { ct, ty: of } => format!("the constant `{}` has type `{}`", arg(ct), ty(of)),
        StoredGoal::ConstEquate { a, b } => format!("{} == {}", arg(a), arg(b)),
        &StoredGoal::DynCompatible { trait_ } => format!("the trait `{}` is dyn compatible", trait_name(trait_)),
        StoredGoal::UnstableFeature { feature } => format!("the feature `{}` is enabled", feature.as_str()),
        StoredGoal::Outlives { a, b } => format!("{}: {}", arg(a), arg(b)),
        StoredGoal::Ambiguous => "an ambiguous goal".to_owned(),
    }
}

/// The item `def` is the body of, as an error names it: `` `Trait::method` ``, a constant in
/// one, or a constant in an item's signature.
fn body_item_text(db: &Db, target: DisplayTarget, def: InferBodyId<'_>) -> String {
    use hir_def::ExpressionStoreOwnerId;
    let def = match def {
        InferBodyId::DefWithBodyId(def) => def,
        InferBodyId::AnonConstId(konst) => {
            return match konst.loc(db).owner {
                ExpressionStoreOwnerId::Body(def) => {
                    // alloc: error path, the error's text
                    format!("a constant in {}", body_item_text(db, target, InferBodyId::DefWithBodyId(def)))
                }
                ExpressionStoreOwnerId::Signature(_) | ExpressionStoreOwnerId::VariantFields(_) => {
                    "a constant in the signature or fields of the item it is placed at".to_owned()
                }
            };
        }
    };
    // alloc: error path, the error's text
    match def {
        DefWithBodyId::FunctionId(f) => {
            let name = FunctionSignature::of(db, f).name.as_str().to_owned();
            match f.lookup(db).container {
                ItemContainerId::TraitId(t) => format!("`{}::{name}`", TraitSignature::of(db, t).name.as_str()),
                ItemContainerId::ImplId(i) => format!(
                    "`<{}>::{name}`",
                    db.impl_self_ty(i).instantiate_identity().skip_norm_wip().display(db, target)
                ),
                ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => format!("`{name}`"),
            }
        }
        _ => match crate::DefWithBody::from(def).name(db) {
            Some(name) => format!("`{}`", name.as_str()),
            None => "an unnamed constant".to_owned(),
        },
    }
}

/// Where an [`UnplacedDiagnostic`] of the body `def` is: an anonymous constant's expression in
/// `source_map`, its owner's store's, or the item whose body or signature it is.
pub(crate) fn unplaced_owner(
    db: &Db,
    def: InferBodyId<'_>,
    source_map: &ExpressionStoreSourceMap,
) -> InFile<SyntaxNodePtr> {
    use hir_def::{ExpressionStoreOwnerId, src::HasSource as _};
    let body = match def {
        InferBodyId::DefWithBodyId(def) => def,
        InferBodyId::AnonConstId(konst) => {
            let loc = konst.loc(db);
            if let Ok(expr) = source_map.expr_syntax(loc.expr) {
                return expr.map(|it| it.syntax_node_ptr());
            }
            match loc.owner {
                ExpressionStoreOwnerId::Body(def) => def,
                ExpressionStoreOwnerId::Signature(def) => {
                    return match def {
                        GenericDefId::AdtId(AdtId::StructId(it)) => {
                            it.lookup(db).ast_ptr(db).map(Into::into)
                        }
                        GenericDefId::AdtId(AdtId::EnumId(it)) => {
                            it.lookup(db).ast_ptr(db).map(Into::into)
                        }
                        GenericDefId::AdtId(AdtId::UnionId(it)) => {
                            it.lookup(db).ast_ptr(db).map(Into::into)
                        }
                        GenericDefId::ConstId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                        GenericDefId::FunctionId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                        GenericDefId::ImplId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                        GenericDefId::StaticId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                        GenericDefId::TraitId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                        GenericDefId::TypeAliasId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                    };
                }
                ExpressionStoreOwnerId::VariantFields(def) => {
                    return match def {
                        VariantId::EnumVariantId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                        VariantId::StructId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                        VariantId::UnionId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                    };
                }
            }
        }
    };
    match body {
        DefWithBodyId::FunctionId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
        DefWithBodyId::StaticId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
        DefWithBodyId::ConstId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
        DefWithBodyId::VariantId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
    }
}

fn path_generics_source_to_ast<'t>(
    path: view::Path<'t>,
    generics_source: PathGenericsSource,
) -> Option<Either<view::GenericArgList<'t>, view::NameRef<'t>>> {
    Some(match generics_source {
        PathGenericsSource::Segment(segment) => {
            let segment = hir_segment_to_ast_segment_view(path, segment)?;
            segment
                .generic_arg_list()
                .map(Either::Left)
                .or_else(|| segment.name_ref().map(Either::Right))?
        }
        PathGenericsSource::AssocType { segment, assoc_type } => {
            let segment = hir_segment_to_ast_segment_view(path, segment)?;
            let segment_args = segment.generic_arg_list()?;
            let assoc = hir_assoc_type_binding_to_ast_view(segment_args, assoc_type)?;
            assoc
                .generic_arg_list()
                .map(Either::Left)
                .or_else(|| assoc.name_ref().map(Either::Right))?
        }
    })
}
