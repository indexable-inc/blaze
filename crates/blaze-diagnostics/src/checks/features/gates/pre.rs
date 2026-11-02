//! The spans rustc's parser gates (`psess.gated_spans.gate`), found in blaze's syntax trees:
//! every tree rustc parses, all of it (what `cfg` removes later and the items an attribute
//! macro is handed included), each feature's spans in the order the parser meets them.
//!
//! The parser gates some constructs as it starts them (`super let`, `auto trait`) and some
//! once it has parsed them whole (`yield e`, `try {}`, a trait alias), so a construct nested
//! in another of the same feature is gated first or after it as rustc's parser does; [`Gated`]
//! keeps the offset each is gated at.
//!
//! Follows rustc's `compiler/rustc_parse/src/lexer/mod.rs` (`frontmatter`) and
//! `compiler/rustc_parse/src/parser/{expr,item,pat,path,stmt,ty,mod}.rs` (each
//! `psess.gated_spans.gate`, and `ungate_last` of `specialization` for a `default fn`), and
//! `compiler/rustc_session/src/parse.rs` (`GatedSpans`) (MIT or Apache-2.0).

use blaze_ids::radix;
use hir::InFile;
use syntax::{
    NodeOrToken, SyntaxKind, T, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use super::Gated;
use crate::{cx::item_range_view as item_range, lints::syntax::Walk};

/// A feature whose syntax rustc's parser gates, in the order `check_crate` reads them: its
/// `gate_all!`s, then those it reads with its own rules, then the soft gates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pre {
    BuiltinSyntax,
    ConstClosures,
    ConstTraitImpl,
    Coroutines,
    DefaultFieldValues,
    ExplicitTailCalls,
    Frontmatter,
    GenBlocks,
    GenericConstItems,
    ImplRestriction,
    MinGenericConstArgs,
    MoreQualifiedPaths,
    MutRestriction,
    ReturnTypeNotation,
    SuperLet,
    TryBlocksHeterogeneous,
    UnnamedEnumVariants,
    UnsafeFields,
    YeetExpr,
    AsyncTraitBounds,
    ClosureLifetimeBinder,
    AssociatedConstEquality,
    MgcaTypeConstSyntax,
    YieldExpr,
    AutoTraits,
    DeclMacro,
    NegativeImpls,
    Specialization,
    TraitAlias,
    TryBlocks,
    MinSpecialization,
}

/// How many [`Pre`]s there are.
pub(super) const COUNT: usize = Pre::MinSpecialization as usize + 1;

/// The spans the parser gates, by [`Pre`], each feature's in the order the parser met them.
pub(super) type Spans = [Vec<Gated>; COUNT];

/// What rustc's parser gates in the crate: the files' trees in the order rustc loads them,
/// then the expansions' in the order the walk comes to them.
pub(super) fn spans(walk: &Walk<'_>) -> Spans {
    // alloc: empty, each allocates only when its feature is gated somewhere
    let mut spans: Spans = std::array::from_fn(|_| Vec::new());
    // alloc: setup, once per check, reused for every tree
    let mut found: Vec<(u32, Pre, Gated)> = Vec::new();
    let files = walk.trees.iter().filter(|it| it.in_file());
    let expansions = walk.trees.iter().filter(|it| !it.in_file());
    for tree in files.chain(expansions) {
        found.clear();
        for node in tree.root.descendants() {
            let mut push = |pre: Pre, at: TextRange, key: TextSize| {
                let gated = Gated { at: InFile::new(tree.file, at), node: InFile::new(tree.file, ast::ptr(node)) };
                found.push((u32::from(key), pre, gated));
            };
            gates_of(node, &mut push);
        }
        radix::sort_by_key(&mut found, |it| it.0);
        for &(_, pre, gated) in &found {
            spans[pre as usize].push(gated);
        }
    }
    spans
}

/// The spans the parser gates at `node`: each with its feature and the offset it is gated
/// at.
fn gates_of(node: ast::Node<'_>, push: &mut impl FnMut(Pre, TextRange, TextSize)) {
    let range = node.text_range();
    match node.kind() {
        SyntaxKind::SOURCE_FILE => {
            if let Some(token) = token(node, SyntaxKind::FRONTMATTER) {
                let text = token.text();
                let len = TextSize::of(text.trim_end());
                let at = TextRange::at(token.text_range().start(), len);
                push(Pre::Frontmatter, at, at.start());
            }
        }
        SyntaxKind::OFFSET_OF_EXPR
        | SyntaxKind::ASM_EXPR
        | SyntaxKind::FORMAT_ARGS_EXPR
        | SyntaxKind::INCLUDE_BYTES_EXPR
        | SyntaxKind::CFG_PRED_EXPR
        | SyntaxKind::PATTERN_TYPE
        | SyntaxKind::DEREF_PAT
        | SyntaxKind::CFG_PRED_PAT => {
            if let Some(ident) = builtin_ident(node) {
                let at = ident.text_range();
                push(Pre::BuiltinSyntax, at, at.start());
            }
        }
        SyntaxKind::CLOSURE_EXPR => {
            if let Some(binder) = child(node, SyntaxKind::FOR_BINDER) {
                let at = binder.text_range();
                push(Pre::ClosureLifetimeBinder, at, at.end());
            }
            if let Some(it) = token(node, T![const]) {
                push(Pre::ConstClosures, it.text_range(), it.text_range().start());
            }
            if let Some(it) = token(node, T![static]) {
                push(Pre::Coroutines, it.text_range(), it.text_range().start());
            }
            if let Some(at) = gen_kind(node) {
                // Gated once the closure's body is parsed.
                push(Pre::GenBlocks, at, range.end());
            }
        }
        SyntaxKind::BLOCK_EXPR => {
            if let Some(at) = gen_kind(node) {
                push(Pre::GenBlocks, at, at.start());
            }
            if let Some(modifier) = child(node, SyntaxKind::TRY_BLOCK_MODIFIER) {
                let at = TextRange::new(modifier.text_range().start(), range.end());
                let pre = match bikeshed(modifier) {
                    true => Pre::TryBlocksHeterogeneous,
                    false => Pre::TryBlocks,
                };
                push(pre, at, at.end());
            }
        }
        SyntaxKind::FN => {
            if let Some(at) = gen_kind(node) {
                push(Pre::GenBlocks, at, at.start());
            }
            if let Some(it) = token(node, T![default]) {
                push(Pre::MinSpecialization, it.text_range(), it.text_range().start());
            }
        }
        SyntaxKind::IMPL => {
            if let Some(it) = token(node, T![default]) {
                push(Pre::Specialization, it.text_range(), it.text_range().start());
            }
            if let Some(it) = token(node, T![!]) {
                push(Pre::NegativeImpls, it.text_range(), it.text_range().start());
            }
            if let Some(it) = token(node, T![const]) {
                push(Pre::ConstTraitImpl, it.text_range(), it.text_range().start());
            }
        }
        SyntaxKind::TRAIT => {
            if let Some(it) = token(node, T![const]) {
                push(Pre::ConstTraitImpl, it.text_range(), it.text_range().start());
            }
            if let Some(it) = token(node, T![auto]) {
                push(Pre::AutoTraits, it.text_range(), it.text_range().start());
            }
            if token(node, T![=]).is_some() {
                let at = item_range(node);
                push(Pre::TraitAlias, at, at.end());
            }
        }
        SyntaxKind::TYPE_ALIAS => {
            if let Some(it) = token(node, T![default]) {
                push(Pre::Specialization, it.text_range(), it.text_range().start());
            }
        }
        SyntaxKind::CONST => {
            if let Some(it) = token(node, T![default]) {
                push(Pre::Specialization, it.text_range(), it.text_range().start());
            }
            if let Some(params) = child(node, SyntaxKind::GENERIC_PARAM_LIST) {
                let at = params.text_range();
                push(Pre::GenericConstItems, at, at.end());
            }
            if let Some(clause) = child(node, SyntaxKind::WHERE_CLAUSE) {
                let at = clause.text_range();
                push(Pre::GenericConstItems, at, at.end());
            }
            if token(node, T![type]).is_some()
                && let Some(konst) = token(node, T![const])
            {
                let at = TextRange::new(item_range(node).start(), konst.text_range().end());
                push(Pre::MgcaTypeConstSyntax, at, range.end());
            }
        }
        SyntaxKind::MACRO_DEF => {
            let at = item_range(node);
            push(Pre::DeclMacro, at, at.end());
        }
        SyntaxKind::TYPE_BOUND => {
            if let Some(at) = bound_constness(node) {
                push(Pre::ConstTraitImpl, at, at.start());
            }
            if let Some(it) = token(node, T![async]) {
                push(Pre::AsyncTraitBounds, it.text_range(), it.text_range().start());
            }
        }
        SyntaxKind::RECORD_FIELD => {
            if let Some(it) = token(node, T![unsafe]) {
                push(Pre::UnsafeFields, it.text_range(), it.text_range().start());
            }
            if token(node, T![=]).is_some()
                && let Some(ty) = node.children().find(|it| ast::Type::can_cast(it.kind()))
                && let Some(default) = child(node, SyntaxKind::CONST_ARG)
            {
                let at = TextRange::new(ty.text_range().end(), default.text_range().end());
                push(Pre::DefaultFieldValues, at, at.end());
            }
        }
        SyntaxKind::VARIANT => {
            if token(node, T![_]).is_some() {
                let at = item_range(node);
                push(Pre::UnnamedEnumVariants, at, at.end());
            }
        }
        SyntaxKind::IMPL_RESTRICTION => push(Pre::ImplRestriction, range, range.end()),
        SyntaxKind::MUT_RESTRICTION => push(Pre::MutRestriction, range, range.end()),
        SyntaxKind::RETURN_TYPE_SYNTAX => push(Pre::ReturnTypeNotation, range, range.end()),
        SyntaxKind::BECOME_EXPR => {
            let at = item_range(node);
            push(Pre::ExplicitTailCalls, at, at.end());
        }
        SyntaxKind::YEET_EXPR => {
            let at = item_range(node);
            push(Pre::YeetExpr, at, at.end());
        }
        SyntaxKind::YIELD_EXPR => {
            let at = item_range(node);
            push(Pre::YieldExpr, at, at.end());
        }
        SyntaxKind::LET_STMT => {
            if let Some(it) = token(node, T![super]) {
                push(Pre::SuperLet, it.text_range(), it.text_range().start());
            }
        }
        SyntaxKind::CONST_ARG => {
            let in_args = node.parent().is_some_and(|it| {
                matches!(
                    it.kind(),
                    SyntaxKind::GENERIC_ARG_LIST | SyntaxKind::ASSOC_TYPE_ARG | SyntaxKind::CONST_PARAM
                )
            });
            if in_args && let Some(at) = const_block(node) {
                push(Pre::MinGenericConstArgs, at, at.end());
            }
        }
        SyntaxKind::ASSOC_TYPE_ARG => {
            if token(node, T![=]).is_some()
                && let Some(name) = child(node, SyntaxKind::NAME_REF)
                && let Some(arg) = child(node, SyntaxKind::CONST_ARG)
            {
                let at = TextRange::new(name.text_range().start(), arg.text_range().end());
                push(Pre::AssociatedConstEquality, at, at.end());
            }
        }
        SyntaxKind::RECORD_EXPR | SyntaxKind::TUPLE_STRUCT_PAT => {
            if let Some(path) = qualified_path(node) {
                push(Pre::MoreQualifiedPaths, path, range.end());
            }
        }
        SyntaxKind::RECORD_PAT => {
            // Gated before the fields are parsed.
            if let Some(path) = qualified_path(node) {
                push(Pre::MoreQualifiedPaths, path, path.end());
            }
        }
        _ => {}
    }
}

/// The first token of `kind` directly in `node`.
pub(super) fn token(node: ast::Node<'_>, kind: SyntaxKind) -> Option<ast::Token<'_>> {
    node.children_with_tokens().filter_map(NodeOrToken::into_token).find(|it| it.kind() == kind)
}

/// The first node of `kind` directly in `node`.
pub(super) fn child(node: ast::Node<'_>, kind: SyntaxKind) -> Option<ast::Node<'_>> {
    node.children().find(|it| it.kind() == kind)
}

/// The name after `builtin #`, where `node` is written as `builtin # name(..)`.
fn builtin_ident(node: ast::Node<'_>) -> Option<ast::Token<'_>> {
    let mut tokens = node
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .filter(|it| !it.kind().is_trivia())
        .skip_while(|it| it.kind() != T![builtin]);
    tokens.next()?;
    let pound = tokens.next()?;
    if pound.kind() != T![#] {
        return None;
    }
    tokens.next()
}

/// The span of `gen` or `async gen` in front of a block, a closure or a function: rustc's
/// `CoroutineKind::Gen` or `AsyncGen`.
fn gen_kind(node: ast::Node<'_>) -> Option<TextRange> {
    let gen_ = token(node, T![gen])?;
    let start = match token(node, T![async]) {
        Some(it) if it.text_range().start() < gen_.text_range().start() => it.text_range().start(),
        _ => gen_.text_range().start(),
    };
    Some(TextRange::new(start, gen_.text_range().end()))
}

/// Whether the `try` of a block is `try bikeshed Ty`.
fn bikeshed(modifier: ast::Node<'_>) -> bool {
    modifier
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .any(|it| it.kind() == T![bikeshed] || (it.kind() == SyntaxKind::IDENT && it.text() == "bikeshed"))
}

/// The span of a bound's constness: `~const`, `[const]` or `const`.
fn bound_constness(bound: ast::Node<'_>) -> Option<TextRange> {
    let mut tokens = bound.children_with_tokens().filter_map(NodeOrToken::into_token).filter(|it| !it.kind().is_trivia());
    let mut before = None;
    let konst = loop {
        let it = tokens.next()?;
        if it.kind() == T![const] {
            break it;
        }
        before = Some(it);
    };
    let start = match before {
        Some(it) if matches!(it.kind(), T![~] | T!['[']) => it.text_range().start(),
        _ => konst.text_range().start(),
    };
    let end = match tokens.next() {
        Some(it) if it.kind() == T![']'] => it.text_range().end(),
        _ => konst.text_range().end(),
    };
    Some(TextRange::new(start, end))
}

/// The span of a const argument written as an unbraced `const { .. }` block.
fn const_block(arg: ast::Node<'_>) -> Option<TextRange> {
    if let Some(konst) = token(arg, T![const])
        && let Some(block) = child(arg, SyntaxKind::BLOCK_EXPR)
    {
        return Some(TextRange::new(konst.text_range().start(), block.text_range().end()));
    }
    let block = child(arg, SyntaxKind::BLOCK_EXPR)?;
    token(block, T![const])?;
    Some(item_range(block))
}

/// The span of the path of a struct expression or pattern, when it is qualified
/// (`<T as Trait>::Assoc { .. }`).
fn qualified_path(node: ast::Node<'_>) -> Option<TextRange> {
    let path = child(node, SyntaxKind::PATH)?;
    let mut first = path;
    while let Some(qualifier) = child(first, SyntaxKind::PATH) {
        first = qualifier;
    }
    let segment = child(first, SyntaxKind::PATH_SEGMENT)?;
    child(segment, SyntaxKind::TYPE_ANCHOR)?;
    Some(path.text_range())
}

#[cfg(test)]
mod tests {
    use syntax::{Edition, SourceFile};

    use super::*;

    /// The gates of `text`, parsed as a 2024 file, in the order [`spans`] keeps them within a
    /// tree: by the offset each is gated at.
    fn gates(text: &str) -> Vec<(Pre, &str)> {
        let parse = SourceFile::parse(text, Edition::Edition2024);
        let mut found = Vec::new();
        for node in parse.root().descendants() {
            gates_of(node, &mut |pre, at, key| found.push((key, pre, at)));
        }
        found.sort_by_key(|it| it.0);
        found.into_iter().map(|(_, pre, at)| (pre, &text[at])).collect()
    }

    /// Finding the gates of a tree reads its views and allocates nothing, gated syntax
    /// included: zero at 4 items and at 400. The first walk parses what is parsed lazily.
    #[test]
    fn finding_the_gates_of_a_tree_allocates_nothing() {
        for n in [4, 400] {
            let text: String = (0..n)
                .map(|i| format!("default impl<T> Tr for T {{}}\nfn f{i}() {{ let _ = try {{ yield 1 }}; }}\n"))
                .collect();
            let parse = SourceFile::parse(&text, Edition::Edition2024);
            let mut gated = 0;
            let mut walk = || {
                for node in parse.root().descendants() {
                    gates_of(node, &mut |_, _, _| gated += 1);
                }
            };
            walk();
            blaze_alloc_count::assert_at_most(0, "the gates of a tree", &mut walk);
            assert_eq!(gated, 2 * 3 * n, "each item's `default impl`, `try` and `yield`, walked twice");
        }
    }

    #[test]
    fn a_nested_yield_is_gated_before_the_one_around_it() {
        let text = "fn f() { yield (yield 1); }";
        assert_eq!(gates(text), [(Pre::YieldExpr, "yield 1"), (Pre::YieldExpr, "yield (yield 1)")]);
    }

    #[test]
    fn a_default_fn_is_min_specialization_and_a_default_impl_specialization() {
        let text = "default impl<T> Tr for T { default fn f() {} }";
        assert_eq!(gates(text), [(Pre::Specialization, "default"), (Pre::MinSpecialization, "default")]);
    }

    #[test]
    fn bound_constness_spans_its_brackets_or_tilde() {
        let text = "fn f<T: [const] A + ~const B + const C>() {}";
        let spans: Vec<&str> = gates(text).into_iter().map(|it| it.1).collect();
        assert_eq!(spans, ["[const]", "~const", "const"]);
    }

    #[test]
    fn a_trait_alias_is_gated_whole_and_an_auto_trait_at_auto() {
        assert_eq!(gates("trait A = B;"), [(Pre::TraitAlias, "trait A = B;")]);
        assert_eq!(gates("#[x] auto trait A {}"), [(Pre::AutoTraits, "auto")]);
    }

    #[test]
    fn a_default_field_value_is_gated_from_the_end_of_its_type() {
        let text = "struct S { a: u8 = 1, }";
        assert_eq!(gates(text), [(Pre::DefaultFieldValues, " = 1")]);
    }

    #[test]
    fn a_try_block_is_gated_once_parsed() {
        let text = "fn f() { let _ = try { 1 }; let _ = try bikeshed u8 { 1 }; }";
        assert_eq!(
            gates(text),
            [(Pre::TryBlocks, "try { 1 }"), (Pre::TryBlocksHeterogeneous, "try bikeshed u8 { 1 }")]
        );
    }
}
