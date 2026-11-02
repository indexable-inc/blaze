//! E0133: an unsafe operation outside an `unsafe` block or function; and its two lints,
//! `unsafe_op_in_unsafe_fn` (one in the body of an `unsafe fn`, outside a block) and
//! `deprecated_safe_2024` (a call of a function that is unsafe only from edition 2024, before
//! it).
//!
//! Follows rustc's `compiler/rustc_mir_build/src/check_unsafety.rs`
//! (`UnsafetyVisitor::requires_unsafe`, `emit_deprecated_safe_fn_call`, `visit_inner_body`,
//! `UnsafeOpKind::emit_unsafe_op_in_unsafe_fn_lint` and `emit_requires_unsafe_err`) and
//! `compiler/rustc_mir_build/src/diagnostics.rs` (the `*RequiresUnsafe*` diagnostics,
//! `UnsafeNotInheritedNote`, `UnsafeNotInheritedLintNote`,
//! `CallToDeprecatedSafeFnRequiresUnsafe`), with `compiler/rustc_span/src/source_map.rs`
//! (`guess_head_span`, `indentation_before`) (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use cfg::CfgAtom;
use hir::{
    AsAssocItem as _, AssocItemContainer, CallableKind, InFile, ItemInNs, ModuleDef, PathResolution,
    UnsafeLint, UnsafetyReason,
};
use hir_def::{ItemContainerId, Lookup as _, attrs::AttrFlags, signatures::FunctionSignature};
use intern::{Symbol, sym};
use syntax::{
    NodeOrToken, SyntaxKind, SyntaxNodePtr,
    view::{self as ast, AstNode as _, AstToken as _, HasAttrs as _},
};

use stdx::list::Join;
use crate::{
    checks::head_range_view as head_range,
    cx::{Cx, signature_range_view as signature_range},
    lints::{body_item, code_fact, registry::LintId},
    lower::{Found, Phase, break_outside_of_loop::up},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MissingUnsafe) -> Vec<Found> {
    const DEPRECATED_SAFE: LintId = LintId::named("deprecated_safe_2024");
    if matches!(d.lint, UnsafeLint::UnsafeOpInUnsafeFn) && !cx.lint_may_fire(UNSAFE_OP) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let ptr = d.node.map(|it| it.syntax_node_ptr());
    let node = cx.syntax_view(ptr);
    let span = cx.span_of(ptr);
    let op = Op::of(cx, d.reason, node, ptr);
    let refused = match (&op, d.lint) {
        // clone: a handle, on the error path, for an operation hir found unsafe
        (Op::InitializingTypeWith, _) => Some(initializing_type_with(span.clone())),
        (Op::CallDropExplicitly(_), UnsafeLint::UnsafeOpInUnsafeFn) => Some(Diag::error(
            None,
            "blaze found an explicit call of a `Drop` method in an `unsafe fn` body, which rustc's `check_unsafety` never reaches (`span_bug!`)",
            // clone: a handle, on the error path, for an operation hir found unsafe
            span.clone(),
        )),
        _ => None,
    };
    if let Some(mut diag) = refused {
        code_fact(cx, &mut diag, ptr);
        // alloc: the diagnostic's parts, for an operation hir found unsafe
        return vec![Found::new(diag, Phase::Mir)];
    }
    let found = match d.lint {
        UnsafeLint::HardError => Found::new(requires_unsafe(cx, &op, node, span, ptr), Phase::Mir),
        UnsafeLint::UnsafeOpInUnsafeFn => {
            let mut diag = op.diag(cx, Some("unsafe_op_in_unsafe_fn"), "block", span);
            if d.first {
                unsafe_fn_note(cx, &mut diag, node);
            }
            op.facts(cx, &mut diag);
            code_fact(cx, &mut diag, ptr);
            Found::lint(diag, Phase::Mir, lint_at(cx, node))
        }
        UnsafeLint::DeprecatedSafe2024 => match deprecated_audit(cx, &op) {
            // Allowed everywhere, a call whose attribute says what to audit gives nothing; one
            // without is E0133 whatever the lint's level.
            // alloc: empty, allocates nothing
            Some(_) if !cx.lint_may_fire(DEPRECATED_SAFE) => return Vec::new(),
            Some((function, audit)) => {
                let mut diag = deprecated_safe(cx, function, audit, span);
                op.facts(cx, &mut diag);
                code_fact(cx, &mut diag, ptr);
                Found::lint(diag, Phase::Mir, lint_at(cx, node))
            }
            // rustc lints a call only when the function's attribute says what to audit, else
            // the call is the error of any call of an unsafe function.
            None => Found::new(requires_unsafe(cx, &op, node, span, ptr), Phase::Mir),
        },
    };
    // alloc: the diagnostic's parts, for an operation hir found unsafe
    vec![found]
}

/// rustc's `UnsafeOpKind`, of the operations hir finds.
enum Op {
    /// A call of an unsafe function, which is named unless it is called through a pointer.
    Call(Option<hir::Function>),
    /// A call of a safe function with `#[target_feature]` from a body without its features:
    /// those missing, in the order the callee names them, and those of them the build enables.
    CallWithFeatures { function: hir::Function, missing: Vec<Symbol>, build_enabled: Vec<Symbol> },
    InlineAsm,
    MutableStatic(Option<hir::Static>),
    ExternStatic(Option<hir::Static>),
    RawPtrDeref,
    UnionField(Option<hir::Union>),
    UseOfUnsafeField,
    InitializingTypeWithUnsafeField,
    MutationOfLayoutConstrainedField,
    BorrowOfLayoutConstrainedField,
    UnsafeBinderCast,
    /// A call of `Drop::drop` or `Drop::pin_drop` by path.
    CallDropExplicitly(Option<hir::Function>),
    /// A struct expression of a type with `#[rustc_layout_scalar_valid_range_*]`, which hir
    /// reports and rustc's `check_unsafety` does not.
    InitializingTypeWith,
}

impl Op {
    fn of<'db>(cx: &Cx<'_, 'db>, reason: UnsafetyReason, node: InFile<ast::Node<'db>>, at: InFile<SyntaxNodePtr>) -> Op {
        match reason {
            UnsafetyReason::UnsafeFnCall => call(cx, node, at),
            UnsafetyReason::InlineAsm => Op::InlineAsm,
            UnsafetyReason::RawPtrDeref => Op::RawPtrDeref,
            UnsafetyReason::MutableStatic => Op::MutableStatic(static_of(cx, node)),
            UnsafetyReason::ExternStatic => Op::ExternStatic(static_of(cx, node)),
            UnsafetyReason::UnionField => Op::UnionField(union_of(cx, node)),
            UnsafetyReason::UseOfUnsafeField => Op::UseOfUnsafeField,
            UnsafetyReason::InitializingTypeWithUnsafeField => Op::InitializingTypeWithUnsafeField,
            UnsafetyReason::CallDropExplicitly => Op::CallDropExplicitly(callee(cx, node.value)),
            UnsafetyReason::InitializingTypeWith => Op::InitializingTypeWith,
            UnsafetyReason::MutationOfLayoutConstrainedField => Op::MutationOfLayoutConstrainedField,
            UnsafetyReason::BorrowOfLayoutConstrainedField => Op::BorrowOfLayoutConstrainedField,
            UnsafetyReason::UnsafeBinderCast => Op::UnsafeBinderCast,
        }
    }

    /// The diagnostic that the operation "requires unsafe {needs}": the error E0133, or the
    /// lint `lint` with the code E0133, which names a callee by its untrimmed path as rustc's
    /// lints do.
    fn diag(&self, cx: &Cx<'_, '_>, lint: Option<&str>, needs: &str, span: Span) -> Diag {
        let path = |function: hir::Function| match lint {
            Some(_) => untrimmed_path(cx, function),
            None => cx.def_path(ModuleDef::Function(function)),
        };
        let (what, label) = match self {
            // alloc: the diagnostic's text, for an operation hir found unsafe
            Op::Call(Some(function)) => (format!("call to unsafe function `{}`", path(*function)), "call to unsafe function"),
            // alloc: the diagnostic's text, for an operation hir found unsafe
            Op::Call(None) => ("call to unsafe function".to_owned(), "call to unsafe function"),
            Op::CallWithFeatures { function, .. } => (
                // alloc: the diagnostic's text, for an operation hir found unsafe
                format!("call to function `{}` with `#[target_feature]`", path(*function)),
                "call to function with `#[target_feature]`",
            ),
            // alloc: the diagnostic's text, for an operation hir found unsafe
            Op::InlineAsm => ("use of inline assembly".to_owned(), "use of inline assembly"),
            // alloc: the diagnostic's text, for an operation hir found unsafe
            Op::MutableStatic(_) => ("use of mutable static".to_owned(), "use of mutable static"),
            // alloc: the diagnostic's text, for an operation hir found unsafe
            Op::ExternStatic(_) => ("use of extern static".to_owned(), "use of extern static"),
            // alloc: the diagnostic's text, for an operation hir found unsafe
            Op::RawPtrDeref => ("dereference of raw pointer".to_owned(), "dereference of raw pointer"),
            // alloc: the diagnostic's text, for an operation hir found unsafe
            Op::UnionField(_) => ("access to union field".to_owned(), "access to union field"),
            // alloc: the diagnostic's text, for an operation hir found unsafe
            Op::UseOfUnsafeField => ("use of unsafe field".to_owned(), "use of unsafe field"),
            Op::InitializingTypeWithUnsafeField => (
                // alloc: the diagnostic's text, for an operation hir found unsafe
                "initializing type with an unsafe field".to_owned(),
                "initialization of struct with unsafe field",
            ),
            Op::MutationOfLayoutConstrainedField => (
                // alloc: the diagnostic's text, for an operation hir found unsafe
                "mutation of layout constrained field".to_owned(),
                "mutation of layout constrained field",
            ),
            Op::BorrowOfLayoutConstrainedField => (
                // alloc: the diagnostic's text, for an operation hir found unsafe
                "borrow of layout constrained field with interior mutability".to_owned(),
                "borrow of layout constrained field with interior mutability",
            ),
            // alloc: the diagnostic's text, for an operation hir found unsafe
            Op::UnsafeBinderCast => ("unsafe binder cast".to_owned(), "unsafe binder cast"),
            Op::CallDropExplicitly(function) => return call_drop_explicitly(cx, *function, span),
            Op::InitializingTypeWith => return initializing_type_with(span),
        };
        // rustc's two unsafe-field errors say "block" where `unsafe_op_in_unsafe_fn` is allowed
        // too, and its unsafe binder cast's message carries the end of an older note's sentence.
        let needs = match self {
            Op::UseOfUnsafeField | Op::InitializingTypeWithUnsafeField => "block",
            _ => needs,
        };
        let message = match self {
            // alloc: the diagnostic's text, for an operation hir found unsafe
            Op::UnsafeBinderCast => format!(
                "unsafe binder cast is unsafe and requires unsafe {} information that may be required to uphold safety guarantees of a type",
                if needs == "block" { "block" } else { "block or unsafe fn" }
            ),
            // alloc: the diagnostic's text, for an operation hir found unsafe
            _ => format!("{what} is unsafe and requires unsafe {needs}"),
        };
        let mut diag = match lint {
            // clone: a handle, on the error path, for an operation hir found unsafe
            Some(lint) => Diag::lint_with_code(lint, "E0133", message, span.clone()),
            // clone: a handle, on the error path, for an operation hir found unsafe
            None => Diag::error(Some("E0133"), message, span.clone()),
        };
        diag.span_label(span, label);
        match self {
            Op::CallWithFeatures { missing, build_enabled, .. } => {
                let features = match missing.len() {
                    1 => "feature",
                    _ => "features",
                };
                // alloc: the diagnostic's text, for an operation hir found unsafe
                diag.help(format!(
                    "in order for the call to be safe, the context requires the following additional target {features}: {}",
                    stdx::list::list(missing.iter(), Join::And, false)
                ));
                if !build_enabled.is_empty() {
                    let (features, them) = match build_enabled.len() {
                        1 => ("feature", "it"),
                        _ => ("features", "them"),
                    };
                    // alloc: the diagnostic's text, for an operation hir found unsafe
                    diag.note(format!(
                        "the {} target {features} being enabled in the build configuration does not remove the requirement to list {them} in `#[target_feature]`",
                        stdx::list::list(build_enabled.iter(), Join::And, false)
                    ));
                }
            }
            _ => {
                if let Some(note) = self.note(lint.is_some()) {
                    diag.note(note);
                }
            }
        }
        diag
    }

    /// What rustc's note says the operation risks, in the lint `unsafe_op_in_unsafe_fn` when
    /// `in_lint`: none for an unsafe binder cast, nor for the lint of a borrow of a layout
    /// constrained field.
    fn note(&self, in_lint: bool) -> Option<&'static str> {
        Some(match self {
            Op::Call(_) | Op::CallWithFeatures { .. } => {
                "consult the function's documentation for information on how to avoid undefined behavior"
            }
            Op::InlineAsm => "inline assembly is entirely unchecked and can cause undefined behavior",
            Op::MutableStatic(_) => {
                "mutable statics can be mutated by multiple threads: aliasing violations or data races will cause undefined behavior"
            }
            Op::ExternStatic(_) => {
                "extern statics are not controlled by the Rust type system: invalid data, aliasing violations or data races will cause undefined behavior"
            }
            Op::RawPtrDeref => {
                "raw pointers may be null, dangling or unaligned; they can violate aliasing rules and cause data races: all of these are undefined behavior"
            }
            Op::UnionField(_) => {
                "the field may not be properly initialized: using uninitialized data will cause undefined behavior"
            }
            Op::UseOfUnsafeField | Op::InitializingTypeWithUnsafeField => "unsafe fields may carry library invariants",
            Op::MutationOfLayoutConstrainedField => {
                "mutating layout constrained fields cannot statically be checked for valid values"
            }
            Op::BorrowOfLayoutConstrainedField if !in_lint => {
                "references to fields of layout constrained fields lose the constraints. Coupled with interior mutability, the field can be changed to invalid values"
            }
            Op::BorrowOfLayoutConstrainedField
            | Op::UnsafeBinderCast
            | Op::CallDropExplicitly(_)
            | Op::InitializingTypeWith => return None,
        })
    }

    /// Records what made the operation unsafe: the callee's signature, the static's, the
    /// union's definition.
    fn facts(&self, cx: &Cx<'_, '_>, diag: &mut Diag) {
        match self {
            Op::Call(Some(function)) | Op::CallWithFeatures { function, .. } => {
                if let Ok(id) = hir_def::FunctionId::try_from(*function) {
                    let path = cx.def_path(ModuleDef::Function(*function));
                    // alloc: the diagnostic's text, for an operation hir found unsafe
                    diag.fact(Fact::Signature(cx.key(ItemId::Function(id))), format!("the signature of `{path}`"));
                }
            }
            Op::MutableStatic(Some(it)) | Op::ExternStatic(Some(it)) => {
                let path = cx.def_path(ModuleDef::Static(*it));
                // alloc: the diagnostic's text, for an operation hir found unsafe
                diag.fact(Fact::Signature(cx.key(ItemId::Static((*it).into()))), format!("the signature of `{path}`"));
            }
            Op::UnionField(Some(it)) => {
                let adt = hir::Adt::Union(*it);
                let path = cx.def_path(ModuleDef::Adt(adt));
                // alloc: the diagnostic's text, for an operation hir found unsafe
                diag.fact(Fact::Signature(cx.key(ItemId::Adt(adt.into()))), format!("the definition of `{path}`"));
            }
            _ => {}
        }
    }
}

/// rustc's `CallDropExplicitlyRequiresUnsafe`: no label and no note, the same where
/// `unsafe_op_in_unsafe_fn` is allowed.
fn call_drop_explicitly(cx: &Cx<'_, '_>, function: Option<hir::Function>, span: Span) -> Diag {
    let Some(function) = function else {
        return Diag::error(None, "blaze cannot name the `Drop` method this call calls", span);
    };
    // alloc: the diagnostic's text, for an operation hir found unsafe
    let message = format!(
        "call `{}` explicitly is unsafe and requires unsafe block",
        cx.def_path(ModuleDef::Function(function))
    );
    Diag::error(Some("E0133"), message, span)
}

/// hir's `InitializingTypeWith`, which rustc's `check_unsafety` has no kind of: the pinned
/// rustc (16d1ffac) reports no struct expression of a type with
/// `#[rustc_layout_scalar_valid_range_*]`, so a program it builds would stop here.
fn initializing_type_with(span: Span) -> Diag {
    Diag::error(
        None,
        "blaze reports initializing a type with `#[rustc_layout_scalar_valid_range_*]` outside `unsafe`, which rustc does not (hir-ty-unsafeck's `InitializingTypeWith`)",
        span,
    )
}

/// rustc's `emit_requires_unsafe_err`: the error, and the label on the `unsafe` block or
/// function around the item the operation is in, which it does not inherit.
fn requires_unsafe<'db>(
    cx: &Cx<'_, 'db>,
    op: &Op,
    node: InFile<ast::Node<'db>>,
    span: Span,
    at: InFile<SyntaxNodePtr>,
) -> Diag {
    let needs = match op_in_unsafe_fn_allowed(cx, at) {
        true => "function or block",
        false => "block",
    };
    let mut diag = op.diag(cx, None, needs, span);
    if let Some(head) = not_inherited(cx, node) {
        diag.span_label(head, "items do not inherit unsafety from separate enclosing items");
    }
    op.facts(cx, &mut diag);
    code_fact(cx, &mut diag, at);
    diag
}

const UNSAFE_OP: LintId = LintId::named("unsafe_op_in_unsafe_fn");

/// Whether `unsafe_op_in_unsafe_fn` is `allow` where the operation is, which makes rustc's
/// error say "requires unsafe function or block" (`unsafe_op_in_unsafe_fn_allowed`, from
/// `lint_level_at_node`; `expect` is not `allow`).
fn op_in_unsafe_fn_allowed(cx: &Cx<'_, '_>, at: InFile<SyntaxNodePtr>) -> bool {
    cx.lint_level(UNSAFE_OP, at) == crate::lints::Level::Allow
}

/// The head (`guess_head_span`) of the first `unsafe` block or `unsafe fn` around `node`'s
/// item: rustc's `UnsafeNotInheritedNote`. Closures and inline consts are checked with their
/// body, so one in a block is never in error.
fn not_inherited<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> Option<Span> {
    let around = up(cx, node).skip(1).find(|it| is_unsafe(it.value))?;
    Some(cx.span(around.with_value(head_range(around.value))))
}

/// Whether `node` is an `unsafe` block or an `unsafe fn`.
fn is_unsafe(node: ast::Node<'_>) -> bool {
    match ast::BlockExpr::cast(node) {
        Some(block) => block.unsafe_token().is_some(),
        None => ast::Fn::cast(node).is_some_and(|it| it.unsafe_token().is_some()),
    }
}

/// rustc's `UnsafeNotInheritedLintNote`, given on the operation hir marks `first` (rustc's
/// `suggest_unsafe_block`, set by hir-ty-unsafeck in its one walk): the function's signature,
/// and the tool-only suggestion to wrap its body in an `unsafe` block.
fn unsafe_fn_note<'db>(cx: &Cx<'_, 'db>, diag: &mut Diag, node: InFile<ast::Node<'db>>) {
    let function = up(cx, node).find_map(|it| ast::Fn::cast(it.value).map(|f| it.with_value(f)));
    let Some(function) = function else { return };
    if function.value.unsafe_token().is_none() {
        return;
    }
    let signature = cx.span(function.with_value(signature_range(function.value)));
    diag.span_note(signature, "an unsafe function restricts its caller, but its body is safe by default");
    if let Some(body) = function.value.body() {
        let body = cx.span(function.with_value(body.syntax().text_range()));
        diag.suggestion_with_style(
            // alloc: the diagnostic's text, for an operation hir found unsafe
            vec![(body.shrink_to_lo(), "{ unsafe ".to_owned()), (body.shrink_to_hi(), "}".to_owned())],
            "consider wrapping the function body in an unsafe block",
            Applicability::MachineApplicable,
            SuggestionStyle::CompletelyHidden,
        );
    }
}

/// The callee of a call rustc lints as `deprecated_safe_2024`, and the string of its
/// `#[rustc_deprecated_safe_2024(audit_that = "..")]`, which says what to audit.
fn deprecated_audit<'db>(cx: &Cx<'_, 'db>, op: &Op) -> Option<(hir::Function, ast::String<'db>)> {
    let Op::Call(Some(function)) = op else { return None };
    Some((*function, audit_that(cx, *function)?))
}

/// rustc's `CallToDeprecatedSafeFnRequiresUnsafe`.
fn deprecated_safe(cx: &Cx<'_, '_>, function: hir::Function, audit: ast::String<'_>, span: Span) -> Diag {
    let audit = audit.value().expect("`audit_that` gives a string that unescapes");
    let path = untrimmed_path(cx, function);
    // alloc: the diagnostic's text, for an operation hir found unsafe
    let message = format!("call to deprecated safe function `{path}` is unsafe and requires unsafe block");
    // clone: a handle, on the error path, for an operation hir found unsafe
    let mut diag = Diag::lint("deprecated_safe_2024", message, span.clone());
    // clone: a handle, on the error path, for an operation hir found unsafe
    diag.span_label(span.clone(), "call to unsafe function");
    let (line_start, indent) = line_of(cx, &span);
    diag.multipart_suggestion(
        // alloc: the diagnostic's text, for an operation hir found unsafe
        format!("you can wrap the call in an `unsafe` block if you can guarantee that {audit}"),
        // alloc: the diagnostic's parts, for an operation hir found unsafe
        vec![
            // alloc: the diagnostic's text, for an operation hir found unsafe
            (line_start, format!("{indent}// FIXME: Audit that {audit}.\n")),
            // alloc: the diagnostic's text, for an operation hir found unsafe
            (span.shrink_to_lo(), "unsafe { ".to_owned()),
            // alloc: the diagnostic's text, for an operation hir found unsafe
            (span.shrink_to_hi(), " }".to_owned()),
        ],
        Applicability::MachineApplicable,
    );
    diag
}

/// The start of the line `span` starts on, and the white space it starts with before `span`
/// (`span_extend_to_line`, `indentation_before`).
fn line_of(cx: &Cx<'_, '_>, span: &Span) -> (Span, String) {
    let Some(text) = cx.file_of(span).map(|it| cx.file_text(it)) else {
        // alloc: the diagnostic's text, for an operation hir found unsafe
        return (span.shrink_to_lo(), String::new());
    };
    let before = &text[..span.lo as usize];
    let start = before.rfind('\n').map_or(0, |it| it + 1);
    let line = &before[start..];
    let indent = line.find(|c: char| !c.is_whitespace()).map_or(line, |it| &line[..it]);
    // alloc: clone: the diagnostic's parts, for an operation hir found unsafe
    (Span { lo: start as u32, hi: start as u32, ..span.clone() }, indent.to_owned())
}

/// The string of `#[rustc_deprecated_safe_2024(audit_that = "..")]` on `function`, when it
/// unescapes: read, not copied, so a call whose lint is allowed allocates nothing.
fn audit_that<'db>(cx: &Cx<'_, 'db>, function: hir::Function) -> Option<ast::String<'db>> {
    let source = cx.vsema.source(function)?;
    source.value.attrs().find_map(|attr| {
        let (name, tt) = attr.as_simple_call()?;
        if name != "rustc_deprecated_safe_2024" {
            return None;
        }
        let mut tokens = tt
            .syntax()
            .children_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .filter(|it| !it.kind().is_trivia());
        while let Some(token) = tokens.next() {
            if token.kind() == SyntaxKind::IDENT && token.text() == "audit_that" {
                if tokens.next()?.kind() != SyntaxKind::EQ {
                    return None;
                }
                let value = ast::String::cast(tokens.next()?)?;
                let unescapes = value.value().is_ok();
                return unescapes.then_some(value);
            }
        }
        None
    })
}

/// The operation of a call hir found unsafe: of the function it calls (an `unsafe fn`, one of
/// an `extern` block, one safe only before 2024), of one with `#[target_feature]`, or through
/// a pointer.
fn call<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>, at: InFile<SyntaxNodePtr>) -> Op {
    let Some(function) = callee(cx, node.value) else { return Op::Call(None) };
    let Ok(id) = hir_def::FunctionId::try_from(function) else { return Op::Call(Some(function)) };
    let signature = FunctionSignature::of(cx.db, id);
    let extern_ = matches!(id.lookup(cx.db).container, ItemContainerId::ExternBlockId(_));
    let declared_unsafe = signature.is_unsafe() || signature.is_deprecated_safe_2024() || (extern_ && !signature.is_safe());
    if declared_unsafe || !signature.has_target_feature() {
        return Op::Call(Some(function));
    }
    let caller = match body_item(cx, at) {
        Some((ItemId::Function(caller), _)) => hir_ty::TargetFeatures::from_fn(cx.db, caller),
        _ => hir_ty::TargetFeatures::default(),
    };
    let missing: Vec<Symbol> = AttrFlags::target_feature_list(cx.db, id)
        .into_iter()
        .filter(|it| !caller.enables_name(it.as_str()))
        // alloc: the diagnostic's parts, for an operation hir found unsafe
        .collect();
    let cfg = cx.krate.cfg(cx.db);
    let build_enabled = missing
        .iter()
        // clone: a handle, on the error path, for an operation hir found unsafe
        .filter(|it| cfg.check_atom(&CfgAtom::KeyValue { key: sym::target_feature, value: (*it).clone() }))
        .cloned()
        // alloc: the diagnostic's parts, for an operation hir found unsafe
        .collect();
    Op::CallWithFeatures { function, missing, build_enabled }
}

/// The function a call or a method call calls, by the type of its callee: `None` through a
/// pointer.
fn callee<'db>(cx: &Cx<'_, 'db>, node: ast::Node<'db>) -> Option<hir::Function> {
    if let Some(call) = ast::MethodCallExpr::cast(node) {
        return cx.vsema.resolve_method_call(call);
    }
    let callee = ast::CallExpr::cast(node)?.expr()?;
    match cx.vsema.resolve_expr_as_callable(callee)?.kind() {
        CallableKind::Function(it) => Some(it),
        _ => None,
    }
}

/// The static a path (an expression or the target of an assignment) names.
fn static_of<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> Option<hir::Static> {
    let path = match ast::PathExpr::cast(node.value) {
        Some(it) => it.path()?,
        None => ast::PathPat::cast(node.value)?.path()?,
    };
    match cx.vsema.resolve_path(path)? {
        PathResolution::Def(ModuleDef::Static(it)) => Some(it),
        _ => None,
    }
}

/// The union whose field a field expression reads, or a pattern of whose fields `node` is.
fn union_of<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> Option<hir::Union> {
    if let Some(field) = ast::FieldExpr::cast(node.value) {
        return match cx.vsema.resolve_field(field)?.left()?.parent_def(cx.db) {
            hir::Variant::Union(it) => Some(it),
            _ => None,
        };
    }
    let record = node.value.ancestors().find_map(ast::RecordPat::cast)?;
    match cx.vsema.resolve_path(record.path()?)? {
        PathResolution::Def(ModuleDef::Adt(hir::Adt::Union(it))) => Some(it),
        _ => None,
    }
}

/// The node rustc emits the lint of an operation at (`hir_context`): the expression itself;
/// for a pattern, the expression it is in (a `let`'s block, a `match`, a closure), or the item
/// of a parameter.
fn lint_at<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> InFile<SyntaxNodePtr> {
    up(cx, node)
        .find(|it| ast::Expr::can_cast(it.value.kind()) || ast::Item::can_cast(it.value.kind()))
        .unwrap_or(node)
        .map(ast::ptr)
}

/// rustc's `def_path_str` under `with_no_trimmed_paths`, which its lints name callees with:
/// the path from the crate's root (`std::env::set_var`); of a method, its trait's or its
/// type's path, then the rest as rustc prints it.
fn untrimmed_path(cx: &Cx<'_, '_>, function: hir::Function) -> String {
    let trimmed = cx.def_path(ModuleDef::Function(function));
    let container = match function.as_assoc_item(cx.db).map(|it| it.container(cx.db)) {
        None => return full_path(cx, ItemInNs::Values(ModuleDef::Function(function))).unwrap_or(trimmed),
        Some(AssocItemContainer::Trait(trait_)) => ModuleDef::Trait(trait_),
        Some(AssocItemContainer::Impl(impl_)) => {
            match impl_.trait_(cx.db).is_none().then(|| impl_.self_ty(cx.db).as_adt()).flatten() {
                Some(adt) => ModuleDef::Adt(adt),
                None => return trimmed,
            }
        }
    };
    let trimmed_container = cx.def_path(container);
    if let Some(full) = full_path(cx, ItemInNs::Types(container))
        && let Some(rest) = trimmed.strip_prefix(trimmed_container.as_str())
    {
        // alloc: the diagnostic's text, for an operation hir found unsafe
        return format!("{full}{rest}");
    }
    trimmed
}

/// The path of an item from the crate's root, as rustc's untrimmed paths name it, else where
/// it is defined.
fn full_path(cx: &Cx<'_, '_>, item: ItemInNs) -> Option<String> {
    let config = hir::FindPathConfig {
        prefer_no_std: false,
        prefer_prelude: false,
        prefer_absolute: false,
        allow_unstable: true,
    };
    let root = cx.krate.root_module(cx.db);
    if let Some(path) = root.find_path(cx.db, item, config) {
        // alloc: the diagnostic's text, for an operation hir found unsafe
        return Some(path.display(cx.db, cx.edition).to_string());
    }
    match item {
        ItemInNs::Types(def) | ItemInNs::Values(def) => def.canonical_path(cx.db, cx.edition),
        ItemInNs::Macros(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Reverse;

    use hir::{AnyDiagnostic, HirFileId, InFile, UnsafeLint};
    use intern::Symbol;
    use syntax::{
        Edition, SourceFile, TextRange, TextSize, view,
        view::{self as ast, AstNode as _},
    };

    use super::{head_range, is_unsafe};
    use crate::{
        cx::{Cx, item_range_view as item_range},
        lower::break_outside_of_loop::{tests::with_file, up},
    };

    /// Unsafe operations in `unsafe fn`s: in a closure, a closure in a closure, an inline
    /// const, an `unsafe` block, a macro's expansion, before and after one another.
    const FIRSTS: &str = r#"
unsafe fn unsf() {}
const unsafe fn cu() -> usize { 0 }
static mut S: i32 = 0;
macro_rules! m { () => { unsf() } }
pub unsafe fn a() { unsf(); unsf(); }
pub unsafe fn b(x: *const i32) -> i32 { let _ = || { unsf(); unsf(); }; let y = *x; y + S }
pub unsafe fn c() { unsafe { unsf(); } unsf(); let _ = || unsf(); }
pub unsafe fn d() { let _ = const { cu() }; let _ = || { let _ = || unsf(); unsf(); }; m!(); m!(); }
pub unsafe fn e() { m!(); let _ = || unsf(); unsf(); }
pub unsafe fn f() { let _ = || unsf(); let _ = || unsf(); }
fn main() {}
"#;

    /// hir's `first`, read by the lint's note, is what the note's old rule found by making the
    /// function's operations again and comparing each with the operation's place.
    #[test]
    fn the_first_flag_is_that_of_the_plain_rule() {
        with_file(FIRSTS, |cx, _| {
            // alloc: a test's diagnostics
            let mut acc = Vec::new();
            cx.krate.root_module(cx.db).diagnostics(cx.db, &mut acc, false);
            let (mut compared, mut firsts) = (0, 0);
            for d in &acc {
                let AnyDiagnostic::MissingUnsafe(d) = d else { continue };
                if !matches!(d.lint, UnsafeLint::UnsafeOpInUnsafeFn) {
                    continue;
                }
                let node = cx.syntax_view(d.node.map(|it| it.syntax_node_ptr()));
                let function = up(cx, node)
                    .find_map(|it| ast::Fn::cast(it.value).map(|f| it.with_value(f)))
                    .expect("an `unsafe_op_in_unsafe_fn` is in a function");
                assert_eq!(d.first, plain_first(cx, node, function), "at {:?}", node.value);
                compared += 1;
                firsts += usize::from(d.first);
            }
            assert!(compared >= 16, "the fixture has 18 linted operations: {compared}");
            assert!(firsts >= 9, "11 of them first: {firsts}");
        });
    }

    /// Whether rustc's `suggest_unsafe_block` is still set at the operation `node`: no operation
    /// outside an `unsafe` block comes before it in its closure (or inline const), or before
    /// that in what that is in, up to `function`. rustc checks a closure with a copy of the
    /// flag, so what is linted in one leaves the flag of what it is in set.
    fn plain_first<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>, function: InFile<ast::Fn<'db>>) -> bool {
        let Some(def) = cx.vsema.to_def(function.value) else { return true };
        let mut chain = Vec::new();
        if !scopes(cx, node, |scope| chain.push(scope)) {
            return true;
        }
        let mut at = None;
        let own = key(node);
        let ops = cx.vsema.get_unsafe_ops(hir::DefWithBody::Function(def).into());
        !ops.iter().any(|other| {
            let other = cx.syntax_view(other.map(|it| it.syntax_node_ptr()));
            if key(other) == own {
                return false;
            }
            let before = match other.file_id == node.file_id {
                true => order(other.value) < order(node.value),
                false => position(cx, other) < *at.get_or_insert_with(|| position(cx, node)),
            };
            if !before {
                return false;
            }
            let mut first = None;
            scopes(cx, other, |scope| {
                first.get_or_insert(scope);
            }) && first.is_some_and(|scope| chain.contains(&scope))
        })
    }

    /// Where a node is, to tell two apart across the trees of a file and its expansions.
    fn key(node: InFile<ast::Node<'_>>) -> (HirFileId, TextRange) {
        (node.file_id, node.value.text_range())
    }

    /// Gives `scope` the bodies rustc checks `node` in, from its own: each closure and inline
    /// const around it, then the function. False when it is in an `unsafe` block, where rustc
    /// lints nothing.
    fn scopes<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>, mut scope: impl FnMut((HirFileId, TextRange))) -> bool {
        for it in up(cx, node).skip(1) {
            if let Some(block) = ast::BlockExpr::cast(it.value) {
                if block.unsafe_token().is_some() {
                    return false;
                }
                if block.const_token().is_some() {
                    scope(key(it));
                }
            } else if ast::ClosureExpr::can_cast(it.value.kind()) {
                scope(key(it));
            } else if ast::Fn::can_cast(it.value.kind()) {
                scope(key(it));
                return true;
            }
        }
        true
    }

    /// [`position`] within one file or expansion.
    fn order(node: ast::Node<'_>) -> (TextSize, Reverse<TextSize>) {
        let range = node.text_range();
        (range.start(), Reverse(range.end()))
    }

    /// The order rustc visits operations in: by where each starts in the file and in each
    /// expansion down to it, one around another first.
    fn position<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> Vec<(TextSize, Reverse<TextSize>)> {
        let mut at = node;
        let mut position = Vec::new();
        loop {
            let range = at.value.text_range();
            position.push((range.start(), Reverse(range.end())));
            let Some(call) = at.file_id.macro_file() else { break };
            at = call.loc(cx.db).to_node_view(cx.db);
        }
        position.reverse();
        position
    }

    /// [`head_range`] by the text: the node's text cut at its first `{` by `split`.
    fn plain_head_range(node: view::Node<'_>) -> TextRange {
        let range = item_range(node);
        let text = node.text();
        let base = node.text_range().start();
        let written = &text[usize::from(range.start() - base)..usize::from(range.end() - base)];
        let head = written.split('{').next().unwrap_or("").trim_end();
        match !head.is_empty() && !head.contains('\n') {
            true => TextRange::at(range.start(), TextSize::of(head)),
            false => range,
        }
    }

    /// The head read from the tokens is the one the text gives, for heads on one line, over
    /// several, empty, after attributes and with a `{` in a comment.
    #[test]
    fn the_head_read_in_place_is_the_texts() {
        let text = r#"
#[inline]
unsafe fn one(x: u8) -> u8 { x }
unsafe fn two(
    x: u8,
) { unsafe { let _ = x; } }
unsafe fn three /* { */ () {}
pub unsafe fn four()   
{}
fn five() { unsafe {} ; unsafe
{ } }
"#;
        let parse = SourceFile::parse(text, Edition::Edition2021);
        let mut compared = 0;
        for node in parse.root().descendants().filter(|it| is_unsafe(*it)) {
            assert_eq!(head_range(node), plain_head_range(node), "at {node:?}");
            compared += 1;
        }
        assert!(compared >= 7, "the text has `unsafe` blocks and functions: {compared}");
    }

    #[test]
    fn lists_are_joined_as_rustc_joins_them() {
        let list = |items: &[&str]| stdx::list::list(items.iter().map(|it| Symbol::intern(it)), stdx::list::Join::And, false);
        assert_eq!(list(&["avx"]), "avx");
        assert_eq!(list(&["avx", "bmi2"]), "avx and bmi2");
        assert_eq!(list(&["avx", "bmi2", "fma"]), "avx, bmi2, and fma");
    }
}
