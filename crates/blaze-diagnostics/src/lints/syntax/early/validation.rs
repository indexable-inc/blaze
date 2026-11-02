//! What rustc's AST validation buffers for the early lint pass (`deprecated_where_clause_location`,
//! `missing_abi`, `missing_unsafe_on_extern`, `patterns_in_fns_without_body`,
//! `unused_visibilities`), what its parser buffers (`varargs_without_pattern`,
//! `break_with_label_and_loop`, in [`parser`]), and `non_camel_case_types` of type parameters.
//!
//! `non_camel_case_types` of the other names rustc's `NonCamelCaseTypes` checks (types,
//! traits, trait aliases, associated types, variants) is hir's `IncorrectCase`
//! (`hir_ty::diagnostics::decl_check`), lowered in `crate::lower::incorrect_case`: it checks
//! no type parameter, so each name is linted once.
//!
//! AST validation runs on the crate once it is expanded and `cfg` has removed what it removes,
//! so it sees what [`Walk`] has; each lint is buffered at the node rustc gives it and emitted
//! as the early lint pass comes to that node ([`Pass::Buffered`]).
//!
//! Follows rustc's `compiler/rustc_ast_passes/src/ast_validation.rs` (`visit_item`,
//! `visit_assoc_item`, `visit_fn`, `visit_ty_common`, `handle_missing_abi`,
//! `check_decl_no_pat`, `check_type_alias_where_clause_location`) and `diagnostics.rs`
//! (`MissingAbiSugg`, `MissingUnsafeOnExternLint`, `UnusedVisibility`,
//! `PatternsInFnsWithoutBody`, `DeprecatedWhereClauseLocation`),
//! `compiler/rustc_lint/src/nonstandard_style.rs` (`NonCamelCaseTypes::check_generic_param`)
//! and `lints.rs` (`NonCamelCaseType`), and `compiler/rustc_lint/src/early.rs`
//! (`visit_generic_param`) (MIT or Apache-2.0).

mod parser;
pub(crate) mod pretty;

use hir::{HirFileId, InFile};
use span::{Edition, SyntaxContext};
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasName as _, HasVisibility as _},
};

use crate::{
    cx::{Cx, item_range_view},
    lints::{
        registry::LintId,
        syntax::{Emitted, Pass, Tree, Walk, source_fact},
    },
    lower::{
        Phase,
        incorrect_case::{is_upper_camel_case, to_upper_camel_case},
    },
    model::{Applicability, Diag, Span, SuggestionStyle},
};

/// The checks of this part, as the early pass's one walk (`super::lints`) calls them: AST
/// validation, the parser's lints and the type parameter check fused into rustc's combined early
/// pass, without rustc's second walk for AST validation. Each node costs one `kind` read and a
/// `match` on it; a view is cast only for the few kinds a lint looks at, and
/// nothing is allocated unless a lint fires. The only other nodes read are those of the
/// subtrees `cfg` removes and attribute macros replace, which only the parser sees.
pub(super) struct Validation {
    parsed: parser::Parsed,
}

impl Validation {
    pub(super) fn new(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Validation {
        Validation { parsed: parser::Parsed::new(cx, walk) }
    }

    /// `node` of `tree`, the `index`th tree of the walk.
    pub(super) fn node(&mut self, cx: &Cx<'_, '_>, tree: &Tree<'_>, index: u32, node: ast::Node<'_>, out: &mut Vec<Emitted>) {
        validate(cx, tree, node, out);
        self.parsed.check(cx, tree, index, node);
    }

    /// After the nodes of `tree`: what the parser sees of it that the walk does not.
    pub(super) fn tree(&mut self, cx: &Cx<'_, '_>, tree: &Tree<'_>, index: u32) {
        let unseen = tree.off.iter().chain(&tree.replaced);
        // clone: a chain of two slice iterators, no allocation
        for root in unseen.clone() {
            let range = root.text_range();
            // clone: a chain of two slice iterators, no allocation
            let nested = unseen.clone().any(|it| it.text_range() != range && it.text_range().contains_range(range));
            if nested {
                continue;
            }
            for node in root.descendants() {
                self.parsed.check(cx, tree, index, node);
            }
        }
    }

    pub(super) fn finish(self) -> impl Iterator<Item = Emitted> {
        self.parsed.finish()
    }
}

/// The lints AST validation buffers at `node`, in the order it buffers them, and
/// `non_camel_case_types` of a type parameter.
fn validate(cx: &Cx<'_, '_>, tree: &Tree<'_>, node: ast::Node<'_>, out: &mut Vec<Emitted>) {
    match node.kind() {
        SyntaxKind::EXTERN_BLOCK => {
            let Some(block) = ast::ExternBlock::cast(node) else { return };
            if block.unsafe_token().is_none() && cx.lint_may_fire(LintId::named("missing_unsafe_on_extern")) {
                missing_unsafe_on_extern(cx, tree, node, out);
            }
            if let Some(abi) = block.abi() {
                missing_abi(cx, tree, abi, node, out);
            }
        }
        SyntaxKind::FN => {
            let Some(item) = ast::Fn::cast(node) else { return };
            if let Some(abi) = item.abi() {
                missing_abi(cx, tree, abi, node, out);
            }
            if in_assoc_item_list(node) && item.body().is_none() {
                patterns_without_body(cx, tree, item, out);
            }
        }
        SyntaxKind::FN_PTR_TYPE => {
            let Some(abi) = ast::FnPtrType::cast(node).and_then(|it| it.abi()) else { return };
            missing_abi(cx, tree, abi, node, out);
        }
        SyntaxKind::CONST if !in_assoc_item_list(node) => {
            let Some(item) = ast::Const::cast(node) else { return };
            unused_visibility(cx, tree, item, out);
        }
        SyntaxKind::TYPE_ALIAS if in_assoc_item_list(node) => {
            let Some(alias) = ast::TypeAlias::cast(node) else { return };
            where_clause_location(cx, tree, alias, out);
        }
        SyntaxKind::TYPE_PARAM => {
            let Some(param) = ast::TypeParam::cast(node) else { return };
            type_param_case(cx, tree, param, out);
        }
        _ => {}
    }
}

/// Whether `node` is an item of a trait or an impl.
fn in_assoc_item_list(node: ast::Node<'_>) -> bool {
    node.parent().is_some_and(|it| it.kind() == SyntaxKind::ASSOC_ITEM_LIST)
}

/// The lint `diag` found at `read` (whose source it records it read), its level decided at
/// `at`.
fn emit(
    cx: &Cx<'_, '_>,
    diag: Diag,
    pass: Pass,
    at: InFile<SyntaxNodePtr>,
    read: InFile<SyntaxNodePtr>,
    out: &mut Vec<Emitted>,
) {
    out.push(emitted(cx, diag, pass, at, read));
}

fn emitted(
    cx: &Cx<'_, '_>,
    mut diag: Diag,
    pass: Pass,
    at: InFile<SyntaxNodePtr>,
    read: InFile<SyntaxNodePtr>,
) -> Emitted {
    source_fact(cx, &mut diag, read);
    Emitted::new(diag, Phase::Resolve, pass, at)
}

/// The syntax context of the token at `offset` of `file`: rustc's `Span::ctxt` of it, which
/// says which expansion made it and in which edition.
fn ctxt_at(cx: &Cx<'_, '_>, file: HirFileId, offset: TextSize) -> SyntaxContext {
    match file.macro_file() {
        None => SyntaxContext::root(file.edition(cx.db)),
        Some(call) => call.expansion_span_map(cx.db).span_at(offset).ctx,
    }
}

/// rustc's `Span::edition` of the token at `offset` of `file`.
fn edition_at(cx: &Cx<'_, '_>, file: HirFileId, offset: TextSize) -> Edition {
    ctxt_at(cx, file, offset).edition(cx.db)
}

fn span(cx: &Cx<'_, '_>, tree: &Tree<'_>, range: TextRange) -> Span {
    cx.span(InFile::new(tree.file, range))
}

/// `missing_unsafe_on_extern`: an `extern` block not marked `unsafe`, before Rust 2024 (in
/// 2024 it is the error `extern blocks must be unsafe`).
fn missing_unsafe_on_extern(cx: &Cx<'_, '_>, tree: &Tree<'_>, node: ast::Node<'_>, out: &mut Vec<Emitted>) {
    let range = item_range_view(node);
    if edition_at(cx, tree.file, range.start()).at_least_2024() {
        return;
    }
    let span = span(cx, tree, range);
    // clone: a lint's span, for a lint found
    let mut diag = Diag::lint("missing_unsafe_on_extern", "extern blocks should be unsafe", span.clone());
    diag.suggestion(
        span.shrink_to_lo(),
        "needs `unsafe` before the extern keyword",
        "unsafe ",
        Applicability::MachineApplicable,
    );
    emit(cx, diag, Pass::Buffered, tree.ptr(node), tree.ptr(node), out);
}

/// `missing_abi`: `extern` with no ABI, on an `extern` block, a function or a function pointer
/// type `at`. rustc skips an `extern` whose span is an attribute's (a proc macro's tokens).
fn missing_abi(cx: &Cx<'_, '_>, tree: &Tree<'_>, abi: ast::Abi<'_>, at: ast::Node<'_>, out: &mut Vec<Emitted>) {
    if abi.string_token().is_some() {
        return;
    }
    let Some(extern_) = abi.extern_token() else { return };
    let span = span(cx, tree, extern_.text_range());
    if !cx.snippet(&span).is_some_and(|it| !it.starts_with("#[")) {
        return;
    }
    let mut diag =
        // clone: a lint's span, for a lint found
        Diag::lint("missing_abi", "`extern` declarations without an explicit ABI are deprecated", span.clone());
    diag.suggestion(span, "explicitly specify the \"C\" ABI", "extern \"C\"", Applicability::MachineApplicable);
    emit(cx, diag, Pass::Buffered, tree.ptr(at), tree.ptr(at), out);
}

/// `patterns_in_fns_without_body`: a `mut x` parameter of an associated function without a
/// body (any other pattern there is the error E0642, of a foreign function E0130).
fn patterns_without_body(cx: &Cx<'_, '_>, tree: &Tree<'_>, item: ast::Fn<'_>, out: &mut Vec<Emitted>) {
    let Some(params) = item.param_list() else { return };
    let at = tree.ptr(item.syntax());
    let mut lint = |range: TextRange, name: ast::Name<'_>| {
        let span = span(cx, tree, range);
        let edition = edition_at(cx, tree.file, range.start());
        let mut diag =
            // clone: a lint's span, for a lint found
            Diag::lint("patterns_in_fns_without_body", "patterns aren't allowed in functions without bodies", span.clone());
        let mut bare = String::with_capacity(name.text().len() + 2);
        pretty::push_ident(&mut bare, name.text(), edition);
        diag.suggestion(span, "remove `mut` from the parameter", bare, Applicability::MachineApplicable);
        emit(cx, diag, Pass::Buffered, at, at, out);
    };
    if let Some(it) = params.self_param()
        && it.amp_token().is_none()
        && let (Some(mut_), Some(name)) = (it.mut_token(), it.name())
    {
        lint(TextRange::new(mut_.text_range().start(), name.syntax().text_range().end()), name);
    }
    for param in params.params() {
        let Some(ast::Pat::IdentPat(pat)) = param.pat() else { continue };
        if pat.mut_token().is_none() || pat.ref_token().is_some() || pat.pat().is_some() {
            continue;
        }
        let Some(name) = pat.name() else { continue };
        lint(pat.syntax().text_range(), name);
    }
}

/// `unused_visibilities`: a visibility on `const _`, when it and the `_` are of one syntax
/// context (rustc's `eq_ctxt`: not a `pub` of a macro on a `_` its caller gave).
fn unused_visibility(cx: &Cx<'_, '_>, tree: &Tree<'_>, item: ast::Const<'_>, out: &mut Vec<Emitted>) {
    let Some(underscore) = item.underscore_token() else { return };
    let Some(vis) = item.visibility() else { return };
    let vis_range = vis.syntax().text_range();
    if ctxt_at(cx, tree.file, underscore.text_range().start()) != ctxt_at(cx, tree.file, vis_range.start()) {
        return;
    }
    let span = span(cx, tree, vis_range);
    let mut diag =
        // clone: a lint's span, for a lint found
        Diag::lint("unused_visibilities", "visibility qualifiers have no effect on `const _` declarations", span.clone());
    diag.note("`const _` does not declare a name, so there is nothing for the qualifier to apply to");
    diag.suggestion_with_style(
        // alloc: the lint's text, for a lint found
        vec![(span, String::new())],
        "remove the qualifier",
        Applicability::MachineApplicable,
        SuggestionStyle::HideCodeInline,
    );
    emit(cx, diag, Pass::Buffered, tree.ptr(item.syntax()), tree.ptr(item.syntax()), out);
}

/// `deprecated_where_clause_location`: an associated type with a `where` clause before its
/// `= Type`, with rustc's suggestion to move its predicates after the type (or, when there are
/// none and a `where` is already there, to remove it).
fn where_clause_location(cx: &Cx<'_, '_>, tree: &Tree<'_>, alias: ast::TypeAlias<'_>, out: &mut Vec<Emitted>) {
    let mut clauses = alias.syntax().children().filter(|it| it.kind() == SyntaxKind::WHERE_CLAUSE);
    let Some(before) = clauses.next() else { return };
    let (Some(eq), Some(ty)) = (alias.eq_token(), alias.ty()) else { return };
    let eq = eq.text_range();
    let range = before.text_range();
    if range.end() > eq.start() {
        return;
    }
    let Some(before) = ast::WhereClause::cast(before) else { return };
    let after = clauses.next().and_then(ast::WhereClause::cast);
    let span = span(cx, tree, range);
    // clone: a lint's span, for a lint found
    let mut diag = Diag::lint("deprecated_where_clause_location", "where clause not allowed here", span.clone());
    diag.note("see issue #89122 <https://github.com/rust-lang/rust/issues/89122> for more information");
    let has_predicates = before.predicates().next().is_some();
    if has_predicates || after.is_none() {
        let edition = edition_at(cx, tree.file, range.start());
        let mut needs_comma = after.is_some_and(|it| it.predicates().next().is_some());
        // alloc: the lint's text, for a lint found
        let mut moved = String::new();
        match after {
            None => moved.push_str(" where "),
            Some(_) if !needs_comma => moved.push(' '),
            Some(_) => {}
        }
        for predicate in before.predicates() {
            if needs_comma {
                moved.push_str(", ");
            }
            needs_comma = true;
            pretty::where_predicate(&mut moved, predicate, edition);
        }
        // rustc's printer writes a break it ends on as nothing.
        moved.truncate(moved.trim_end().len());
        let end = after.map_or(ty.syntax().text_range().end(), |it| it.syntax().text_range().end());
        let right = cx.span(InFile::new(tree.file, TextRange::empty(end)));
        diag.multipart_suggestion(
            "move it to the end of the type declaration",
            // alloc: the lint's text, for a lint found
            vec![(extended_back_over_white_space(cx, &span), String::new()), (right, moved)],
            Applicability::MachineApplicable,
        );
    } else {
        diag.suggestion(span, "remove this `where`", "", Applicability::MachineApplicable);
    }
    emit(cx, diag, Pass::Buffered, tree.ptr(alias.syntax()), tree.ptr(alias.syntax()), out);
}

/// rustc's `span_extend_prev_while(span, char::is_whitespace)`: `span` with the white space
/// before it.
fn extended_back_over_white_space(cx: &Cx<'_, '_>, span: &Span) -> Span {
    // clone: a lint's span, for a lint found
    let Some(file) = cx.file_of(span) else { return span.clone() };
    // clone: a lint's span, for a lint found
    let Some(before) = cx.file_text(file).get(..span.lo as usize) else { return span.clone() };
    let lo = before.trim_end_matches(char::is_whitespace).len();
    span.with_lo(lo as u32)
}

/// `non_camel_case_types` of a type parameter, which rustc checks as its early lint pass comes
/// to it (`check_generic_param`), its level decided by the parameter's attributes.
fn type_param_case(cx: &Cx<'_, '_>, tree: &Tree<'_>, param: ast::TypeParam<'_>, out: &mut Vec<Emitted>) {
    let Some(name) = param.name() else { return };
    let written = name.text();
    let name_str = written.strip_prefix("r#").unwrap_or(written);
    if is_upper_camel_case(name_str) {
        return;
    }
    let span = span(cx, tree, name.syntax().text_range());
    let mut diag = Diag::lint(
        "non_camel_case_types",
        // alloc: the lint's text, for a lint found
        format!("type parameter `{name_str}` should have an upper camel case name"),
        // clone: a lint's span, for a lint found
        span.clone(),
    );
    let camel = to_upper_camel_case(name_str);
    if camel != name_str {
        diag.suggestion(span, "convert the identifier to upper camel case", camel, Applicability::MaybeIncorrect);
    } else {
        diag.span_label(span, "should have an UpperCamelCase name");
    }
    emit(cx, diag, Pass::Early, tree.ptr(param.syntax()), tree.ptr(param.syntax()), out);
}
