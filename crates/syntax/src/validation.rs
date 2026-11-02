//! This module implements syntax validation that the parser doesn't handle.
//!
//! A failed validation emits a diagnostic.

mod block;

use itertools::Itertools;
use rustc_literal_escaper::{
    EscapeError, unescape_byte, unescape_byte_str, unescape_c_str, unescape_char, unescape_str,
};
use syntax_tree::Direction;

use crate::{
    SyntaxError,
    SyntaxKind::{BLOCK_EXPR, CONST, FN, INT_NUMBER, TYPE_ALIAS},
    T, TextRange, TextSize, WalkEvent, match_ast,
    view::{
        self as ast, AstNode, HasAttrs as _, HasVisibility as _, IsString as _, Node,
        RangeItem as _, Token,
    },
};

pub(crate) fn validate(root: Node<'_>, errors: &mut Vec<SyntaxError>) {
    let _p = tracing::info_span!("parser::validate").entered();
    // FIXME:
    // * Add unescape validation of raw string literals and raw byte string literals
    // * Add validation of doc comments are being attached to nodes

    for node in root.descendants() {
        validate_node(node, errors);
    }
}

/// [`validate`] for an outline: `lazies` are its lazy nodes, by range and number in preorder.
/// A lazy node and what is in it are validated when `body` says so, which first adds the
/// body's parser errors; `body` is asked before anything walks into the node.
pub(crate) fn validate_outline(
    root: Node<'_>,
    lazies: &[(TextRange, u32)],
    mut body: impl FnMut(u32, &mut Vec<SyntaxError>) -> bool,
    errors: &mut Vec<SyntaxError>,
) {
    let _p = tracing::info_span!("parser::validate_outline").entered();
    let mut lazies = lazies.iter().peekable();
    let mut preorder = root.preorder();
    while let Some(event) = preorder.next() {
        let WalkEvent::Enter(node) = event else { continue };
        if let Some(&&(range, at)) = lazies.peek()
            && node.kind() == BLOCK_EXPR
            && node.text_range() == range
        {
            lazies.next();
            preorder.skip_subtree();
            if body(at, errors) {
                validate(node, errors);
            }
            continue;
        }
        validate_node(node, errors);
    }
}

/// Runs on every node of a validated tree: one `match` on the kind, so a node no check is for
/// (most nodes) costs one jump, and the node is cast only for its check. Each type is a single
/// kind.
fn validate_node(node: Node<'_>, errors: &mut Vec<SyntaxError>) {
    use crate::SyntaxKind as K;
    macro_rules! dispatch {
        ($($kind:ident => $ty:ident($it:ident) => $res:expr,)*) => {
            match node.kind() {
                $(K::$kind => {
                    debug_assert_eq!(<ast::$ty<'_> as AstNode<'_>>::kind(), K::$kind);
                    let $it = ast::$ty::cast(node).expect("the kind casts");
                    $res;
                })*
                _ => {}
            }
        };
    }
    dispatch! {
        LITERAL => Literal(it) => validate_literal(it, errors),
        CONST => Const(it) => validate_const(it, errors),
        BLOCK_EXPR => BlockExpr(it) => block::validate_block_expr(it, errors),
        FIELD_EXPR => FieldExpr(it) => validate_numeric_name(it.name_ref(), errors),
        RECORD_EXPR_FIELD => RecordExprField(it) => validate_numeric_name(it.name_ref(), errors),
        VISIBILITY => Visibility(it) => validate_visibility(it, errors),
        RANGE_EXPR => RangeExpr(it) => validate_range_expr(it, errors),
        PATH_SEGMENT => PathSegment(it) => validate_path_keywords(it, errors),
        REF_TYPE => RefType(it) => validate_trait_object_ref_ty(it, errors),
        PTR_TYPE => PtrType(it) => validate_trait_object_ptr_ty(it, errors),
        FN_PTR_TYPE => FnPtrType(it) => validate_trait_object_fn_ptr_ret_ty(it, errors),
        MACRO_RULES => MacroRules(it) => validate_macro_rules(it, errors),
        LET_EXPR => LetExpr(it) => validate_let_expr(it, errors),
        DYN_TRAIT_TYPE => DynTraitType(it) => errors.extend(validate_trait_object_ty(it)),
        IMPL_TRAIT_TYPE => ImplTraitType(it) => errors.extend(validate_impl_object_ty(it)),
    }
}

fn rustc_unescape_error_to_string(err: &EscapeError) -> (&'static str, bool) {
    use EscapeError as EE;

    #[rustfmt::skip]
    let err_message = match err {
        EE::ZeroChars => {
            "Literal must not be empty"
        }
        EE::MoreThanOneChar => {
            "Literal must be one character long"
        }
        EE::LoneSlash => {
            "Character must be escaped: `\\`"
        }
        EE::InvalidEscape => {
            "Invalid escape"
        }
        EE::BareCarriageReturn | EE::BareCarriageReturnInRawString => {
            "Character must be escaped: `\r`"
        }
        EE::EscapeOnlyChar => {
            "Escape character `\\` must be escaped itself"
        }
        EE::TooShortHexEscape => {
            "ASCII hex escape code must have exactly two digits"
        }
        EE::InvalidCharInHexEscape => {
            "ASCII hex escape code must contain only hex characters"
        }
        EE::OutOfRangeHexEscape => {
            "ASCII hex escape code must be at most 0x7F"
        }
        EE::NoBraceInUnicodeEscape => {
            "Missing `{` to begin the unicode escape"
        }
        EE::InvalidCharInUnicodeEscape => {
            "Unicode escape must contain only hex characters and underscores"
        }
        EE::EmptyUnicodeEscape => {
            "Unicode escape must not be empty"
        }
        EE::UnclosedUnicodeEscape => {
            "Missing `}` to terminate the unicode escape"
        }
        EE::LeadingUnderscoreUnicodeEscape => {
            "Unicode escape code must not begin with an underscore"
        }
        EE::OverlongUnicodeEscape => {
            "Unicode escape code must have at most 6 digits"
        }
        EE::LoneSurrogateUnicodeEscape => {
            "Unicode escape code must not be a surrogate"
        }
        EE::OutOfRangeUnicodeEscape => {
            "Unicode escape code must be at most 0x10FFFF"
        }
        EE::UnicodeEscapeInByte => {
            "Byte literals must not contain unicode escapes"
        }
        EE::NonAsciiCharInByte  => {
            "Byte literals must not contain non-ASCII characters"
        }
        EE::NulInCStr  => {
            "C strings literals must not contain null characters"
        }
        EE::UnskippedWhitespaceWarning => "Whitespace after this escape is not skipped",
        EE::MultipleSkippedLinesWarning => "Multiple lines are skipped by this escape",

    };

    (err_message, err.is_fatal())
}

fn validate_literal(literal: ast::Literal<'_>, acc: &mut Vec<SyntaxError>) {
    // FIXME: move this function to outer scope (https://github.com/rust-lang/rust-analyzer/pull/2834#discussion_r366196658)
    fn unquote(text: &str, prefix_len: usize, end_delimiter: char) -> Option<&str> {
        text.rfind(end_delimiter).and_then(|end| text.get(prefix_len..end))
    }

    let token = literal.token();
    let text = token.text();

    // FIXME: lift this lambda refactor to `fn` (https://github.com/rust-lang/rust-analyzer/pull/2834#discussion_r366199205)
    let mut push_err = |prefix_len, off, err: EscapeError| {
        let off = token.text_range().start() + TextSize::try_from(off + prefix_len).unwrap();
        let (message, is_err) = rustc_unescape_error_to_string(&err);
        // FIXME: Emit lexer warnings
        if is_err {
            acc.push(SyntaxError::new_at_offset(message, off));
        }
    };

    match literal.kind() {
        ast::LiteralKind::String(s) => {
            if !s.is_raw()
                && let Some(without_quotes) = unquote(text, 1, '"')
            {
                unescape_str(without_quotes, |range, char| {
                    if let Err(err) = char {
                        push_err(1, range.start, err);
                    }
                });
            }
        }
        ast::LiteralKind::ByteString(s) => {
            if !s.is_raw()
                && let Some(without_quotes) = unquote(text, 2, '"')
            {
                unescape_byte_str(without_quotes, |range, char| {
                    if let Err(err) = char {
                        push_err(1, range.start, err);
                    }
                });
            }
        }
        ast::LiteralKind::CString(s) => {
            if !s.is_raw()
                && let Some(without_quotes) = unquote(text, 2, '"')
            {
                unescape_c_str(without_quotes, |range, char| {
                    if let Err(err) = char {
                        push_err(1, range.start, err);
                    }
                });
            }
        }
        ast::LiteralKind::Char(_) => {
            if let Some(without_quotes) = unquote(text, 1, '\'')
                && let Err(err) = unescape_char(without_quotes)
            {
                push_err(1, 0, err);
            }
        }
        ast::LiteralKind::Byte(_) => {
            if let Some(without_quotes) = unquote(text, 2, '\'')
                && let Err(err) = unescape_byte(without_quotes)
            {
                push_err(2, 0, err);
            }
        }
        ast::LiteralKind::IntNumber(_)
        | ast::LiteralKind::FloatNumber(_)
        | ast::LiteralKind::Bool(_) => {}
    }
}

pub(crate) fn validate_block_structure(root: Node<'_>) {
    // alloc: a debug-only structure check, switched off in `syntax_node.rs`
    let mut stack = Vec::new();
    for node in root.descendants_with_tokens() {
        match node.kind() {
            T!['{'] => stack.push(node),
            T!['}'] => {
                if let Some(pair) = stack.pop() {
                    assert_eq!(
                        node.parent(),
                        pair.parent(),
                        "\nunpaired curlies:\n{}\n{:#?}\n",
                        root.text(),
                        root,
                    );
                    assert!(
                        node.next_sibling_or_token().is_none()
                            && pair.prev_sibling_or_token().is_none(),
                        "\nfloating curlies at {:?}\nfile:\n{}\nerror:\n{}\n",
                        node,
                        root.text(),
                        node,
                    );
                }
            }
            _ => (),
        }
    }
}

fn validate_numeric_name(name_ref: Option<ast::NameRef<'_>>, errors: &mut Vec<SyntaxError>) {
    if let Some(int_token) = int_token(name_ref)
        && int_token.text().chars().any(|c| !c.is_ascii_digit())
    {
        errors.push(SyntaxError::new(
            "Tuple (struct) field access is only allowed through \
                decimal integers with no underscores or suffix",
            int_token.text_range(),
        ));
    }

    fn int_token(name_ref: Option<ast::NameRef<'_>>) -> Option<Token<'_>> {
        name_ref?.syntax().first_child_or_token()?.into_token().filter(|it| it.kind() == INT_NUMBER)
    }
}

fn validate_visibility(vis: ast::Visibility<'_>, errors: &mut Vec<SyntaxError>) {
    let path_without_in_token = if let Some(inner) = vis.visibility_inner() {
        inner.in_token().is_none()
            && inner
                .path()
                .and_then(|p| p.as_single_name_ref())
                .and_then(|n| n.ident_token())
                .is_some()
    } else {
        false
    };
    if path_without_in_token {
        errors.push(SyntaxError::new("incorrect visibility restriction", vis.syntax().text_range()));
    }
    let parent = match vis.syntax().parent() {
        Some(it) => it,
        None => return,
    };
    match parent.kind() {
        FN | CONST | TYPE_ALIAS => (),
        _ => return,
    }

    let impl_def = match parent.parent().and_then(|it| it.parent()).and_then(ast::Impl::cast) {
        Some(it) => it,
        None => return,
    };
    // FIXME: disable validation if there's an attribute, since some proc macros use this syntax.
    // ideally the validation would run only on the fully expanded code, then this wouldn't be necessary.
    if impl_def.trait_().is_some() && impl_def.attrs().next().is_none() {
        errors.push(SyntaxError::new("Unnecessary visibility qualifier", vis.syntax().text_range()));
    }
}

fn validate_range_expr(expr: ast::RangeExpr<'_>, errors: &mut Vec<SyntaxError>) {
    if expr.op_kind() == Some(ast::RangeOp::Inclusive) && expr.end().is_none() {
        errors.push(SyntaxError::new(
            "An inclusive range must have an end expression",
            expr.syntax().text_range(),
        ));
    }
}

fn validate_path_keywords(segment: ast::PathSegment<'_>, errors: &mut Vec<SyntaxError>) {
    let path = segment.parent_path();
    let is_path_start = segment.coloncolon_token().is_none() && path.qualifier().is_none();
    if let Some(token) = segment.self_token() {
        let is_last_segment = path.parent_path().is_none();
        if !is_path_start && !is_last_segment {
            errors.push(SyntaxError::new(
                "The `self` keyword is only allowed at the start or at the end of a path",
                token.text_range(),
            ));
        }
    } else if let Some(token) = segment.crate_token()
        && (!is_path_start || use_prefix(path).is_some())
    {
        errors.push(SyntaxError::new(
            "The `crate` keyword is only allowed as the first segment of a path",
            token.text_range(),
        ));
    }

    fn use_prefix(mut path: ast::Path<'_>) -> Option<ast::Path<'_>> {
        for node in path.syntax().ancestors().skip(1) {
            match_ast! {
                match node {
                    ast::UseTree(it) => if let Some(tree_path) = it.path() {
                        // Even a top-level path exists within a `UseTree` so we must explicitly
                        // allow our path but disallow anything else
                        if tree_path != path {
                            return Some(tree_path);
                        }
                    },
                    ast::UseTreeList(_) => continue,
                    ast::Path(parent) => path = parent,
                    _ => return None,
                }
            };
        }
        None
    }
}

fn validate_trait_object_ref_ty(ty: ast::RefType<'_>, errors: &mut Vec<SyntaxError>) {
    match ty.ty() {
        Some(ast::Type::DynTraitType(ty)) => {
            if let Some(err) = validate_trait_object_ty_plus(ty) {
                errors.push(err);
            }
        }
        Some(ast::Type::ImplTraitType(ty)) => {
            if let Some(err) = validate_impl_object_ty_plus(ty) {
                errors.push(err);
            }
        }
        _ => (),
    }
}

fn validate_trait_object_ptr_ty(ty: ast::PtrType<'_>, errors: &mut Vec<SyntaxError>) {
    match ty.ty() {
        Some(ast::Type::DynTraitType(ty)) => {
            if let Some(err) = validate_trait_object_ty_plus(ty) {
                errors.push(err);
            }
        }
        Some(ast::Type::ImplTraitType(ty)) => {
            if let Some(err) = validate_impl_object_ty_plus(ty) {
                errors.push(err);
            }
        }
        _ => (),
    }
}

fn validate_trait_object_fn_ptr_ret_ty(ty: ast::FnPtrType<'_>, errors: &mut Vec<SyntaxError>) {
    match ty.ret_type().and_then(|ty| ty.ty()) {
        Some(ast::Type::DynTraitType(ty)) => {
            if let Some(err) = validate_trait_object_ty_plus(ty) {
                errors.push(err);
            }
        }
        Some(ast::Type::ImplTraitType(ty)) => {
            if let Some(err) = validate_impl_object_ty_plus(ty) {
                errors.push(err);
            }
        }
        _ => (),
    }
}

fn validate_trait_object_ty(ty: ast::DynTraitType<'_>) -> Option<SyntaxError> {
    let tbl = ty.type_bound_list()?;
    let no_bounds = tbl.bounds().filter_map(|it| it.ty()).next().is_none();

    match no_bounds {
        true => Some(SyntaxError::new(
            "At least one trait is required for an object type",
            ty.syntax().text_range(),
        )),
        false => None,
    }
}

fn validate_impl_object_ty(ty: ast::ImplTraitType<'_>) -> Option<SyntaxError> {
    let tbl = ty.type_bound_list()?;
    let no_bounds = tbl.bounds().filter_map(|it| it.ty()).next().is_none();

    match no_bounds {
        true => Some(SyntaxError::new(
            "At least one trait is required for an object type",
            ty.syntax().text_range(),
        )),
        false => None,
    }
}

/// The first token from `token` on in `direction` that is not trivia: `algo::skip_trivia_token`.
fn skip_trivia_token(mut token: Token<'_>, direction: Direction) -> Option<Token<'_>> {
    while token.kind().is_trivia() {
        token = match direction {
            Direction::Next => token.next_token()?,
            Direction::Prev => token.prev_token()?,
        }
    }
    Some(token)
}

// FIXME: This is not a validation error, this is a context dependent parse error
fn validate_trait_object_ty_plus(ty: ast::DynTraitType<'_>) -> Option<SyntaxError> {
    let dyn_token = ty.dyn_token()?;
    let preceding_token = skip_trivia_token(dyn_token.prev_token()?, Direction::Prev)?;
    let tbl = ty.type_bound_list()?;
    let more_than_one_bound = tbl.bounds().next_tuple::<(_, _)>().is_some();

    if more_than_one_bound && !matches!(preceding_token.kind(), T!['('] | T![<] | T![=]) {
        Some(SyntaxError::new("ambiguous `+` in a type", ty.syntax().text_range()))
    } else {
        None
    }
}

// FIXME: This is not a validation error, this is a context dependent parse error
fn validate_impl_object_ty_plus(ty: ast::ImplTraitType<'_>) -> Option<SyntaxError> {
    let dyn_token = ty.impl_token()?;
    let preceding_token = skip_trivia_token(dyn_token.prev_token()?, Direction::Prev)?;
    let tbl = ty.type_bound_list()?;
    let more_than_one_bound = tbl.bounds().next_tuple::<(_, _)>().is_some();

    if more_than_one_bound && !matches!(preceding_token.kind(), T!['('] | T![<] | T![=]) {
        Some(SyntaxError::new("ambiguous `+` in a type", ty.syntax().text_range()))
    } else {
        None
    }
}

fn validate_macro_rules(mac: ast::MacroRules<'_>, errors: &mut Vec<SyntaxError>) {
    if let Some(vis) = mac.visibility() {
        errors.push(SyntaxError::new(
            "visibilities are not allowed on `macro_rules!` items",
            vis.syntax().text_range(),
        ));
    }
}

fn validate_const(const_: ast::Const<'_>, errors: &mut Vec<SyntaxError>) {
    if let Some(mut_token) = const_
        .const_token()
        .and_then(|t| t.next_token())
        .and_then(|t| skip_trivia_token(t, Direction::Next))
        .filter(|t| t.kind() == T![mut])
    {
        errors.push(SyntaxError::new("const globals cannot be mutable", mut_token.text_range()));
    }
}

fn validate_let_expr(let_: ast::LetExpr<'_>, errors: &mut Vec<SyntaxError>) {
    let mut ancestor = let_.syntax().parent();
    while let Some(node) = ancestor {
        let kind = node.kind();
        if ast::ParenExpr::can_cast(kind) {
            ancestor = node.parent();
        } else if let Some(it) = ast::BinExpr::cast(node) {
            if it.op_kind() != Some(ast::BinaryOp::LogicOp(ast::LogicOp::And)) {
                break;
            }
            ancestor = it.syntax().parent();
        } else if ast::IfExpr::can_cast(kind)
            || ast::WhileExpr::can_cast(kind)
            || ast::MatchGuard::can_cast(kind)
        {
            // It must be part of the condition since the expressions are inside a block.
            return;
        } else {
            break;
        }
    }
    errors.push(SyntaxError::new(
        "`let` expressions are not supported here",
        let_.syntax().text_range(),
    ));
}
