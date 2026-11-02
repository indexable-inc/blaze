//! Lexing, bridging to parser (which does the actual parsing) and
//! incremental reparsing.

mod reparsing;

use std::sync::Arc;

use parser::{Edition, OutlineStrStep, StrStep};
use syntax_tree::{FlatTree, Text, TextRange, TextSize};

use crate::{SyntaxError, SyntaxKind, SyntaxTreeBuilder, Tree, syntax_node::RustLanguage};

pub(crate) use crate::parsing::reparsing::{
    edit_between, incremental_reparse, incremental_reparse_onto, reparse_all,
};

/// A source file's tree: in full, or as an outline whose `fn` bodies are lazy nodes.
pub(crate) fn parse_text(
    text: &Text,
    edition: Edition,
    outline: bool,
) -> (Tree, Vec<SyntaxError>) {
    let _p = tracing::info_span!("parse_text", outline).entered();
    let lexed = parser::LexedStr::new(edition, text);
    let parser_input = lexed.to_input(edition);
    let parser_output = match outline {
        true => parser::TopEntryPoint::SourceFile.parse_outline(&parser_input),
        false => parser::TopEntryPoint::SourceFile.parse(&parser_input),
    };
    let (tree, errors, _eof) = build_tree(text.clone(), &lexed, &parser_output, edition);
    (Tree::new(Arc::new(tree)), errors)
}

pub(crate) fn parse_text_at(
    text: &Text,
    entry: parser::TopEntryPoint,
    edition: Edition,
) -> (Tree, Vec<SyntaxError>) {
    let _p = tracing::info_span!("parse_text_at").entered();
    let lexed = parser::LexedStr::new(edition, text);
    let parser_input = lexed.to_input(edition);
    let parser_output = entry.parse(&parser_input);
    let (tree, errors, _eof) = build_tree(text.clone(), &lexed, &parser_output, edition);
    (Tree::new(Arc::new(tree)), errors)
}

/// The tree of `lexed`, the lexing of `source`, from the parser's output (full or outline),
/// its parser errors then its lexer errors, and whether every token was taken.
pub(crate) fn build_tree(
    source: Text,
    lexed: &parser::LexedStr<'_>,
    parser_output: &parser::Output,
    edition: Edition,
) -> (FlatTree, Vec<SyntaxError>, bool) {
    let range = TextRange::up_to(TextSize::of(&*source));
    let (tree, mut errors, is_eof) = build_parsed(source, range, lexed, parser_output, edition);
    errors.extend(lexer_errors(lexed, TextSize::new(0)));
    (tree, errors, is_eof)
}

/// [`build_tree`] with the parser's errors only, of `lexed`, the lexing of `range` of
/// `source`: the tree is over that range of `source`, and its errors' ranges are in `source`.
fn build_parsed(
    source: Text,
    range: TextRange,
    lexed: &parser::LexedStr<'_>,
    parser_output: &parser::Output,
    edition: Edition,
) -> (FlatTree, Vec<SyntaxError>, bool) {
    let _p = tracing::info_span!("build_tree").entered();
    debug_assert_eq!(lexed.as_str(), &source[range]);
    let start = range.start();
    let mut builder = SyntaxTreeBuilder::sourced_range(source, range);

    let is_eof = lexed.intersperse_trivia_outline(parser_output, &mut |step| match step {
        OutlineStrStep::Step(StrStep::Token { kind, text }) => builder.token(kind, text),
        OutlineStrStep::Step(StrStep::Enter { kind }) => builder.start_node(kind),
        OutlineStrStep::Step(StrStep::Exit) => builder.finish_node(),
        OutlineStrStep::Step(StrStep::Error { msg, pos }) => {
            builder.error(msg.to_owned(), TextSize::try_from(pos).unwrap() + start)
        }
        OutlineStrStep::Lazy { kind, text } => builder.lazy(kind, TextSize::of(text)),
    });

    let (tree, errors) = builder.finish_raw(edition);
    (tree, errors, is_eof)
}

/// [`build_tree`] for a node of `kind` parsed again alone, over `range` of `source` (`lexed`
/// is its lexing), its errors' ranges from the node's start: `None` unless the parse took
/// every token into one node of `kind`, its root.
pub(crate) fn build_node(
    source: Text,
    range: TextRange,
    lexed: &parser::LexedStr<'_>,
    parser_output: &parser::Output,
    edition: Edition,
    kind: SyntaxKind,
) -> Option<(FlatTree, Vec<SyntaxError>)> {
    let (mut depth, mut roots, mut stray) = (0usize, 0usize, false);
    let is_eof = lexed.intersperse_trivia_outline(parser_output, &mut |step| match step {
        OutlineStrStep::Step(StrStep::Enter { kind: entered }) => {
            if depth == 0 {
                roots += 1;
                stray |= entered != kind;
            }
            depth += 1;
        }
        OutlineStrStep::Step(StrStep::Exit) => depth = depth.saturating_sub(1),
        OutlineStrStep::Step(StrStep::Token { .. }) | OutlineStrStep::Lazy { .. } => {
            stray |= depth == 0
        }
        OutlineStrStep::Step(StrStep::Error { .. }) => (),
    });
    if !is_eof || stray || roots != 1 {
        return None;
    }
    let (tree, errors, _eof) = build_parsed(source, range, lexed, parser_output, edition);
    let start = range.start();
    // A parser error is at its offset in `source`; the node's are given from its start.
    let mut errors: Vec<SyntaxError> = errors
        .into_iter()
        .map(|it| {
            let range = it.range() - start;
            it.with_range(range)
        })
        .collect(); // alloc: none, collected in place (and none without errors)
    errors.extend(lexer_errors(lexed, TextSize::new(0)));
    Some((tree, errors))
}

/// The errors of `lexed`'s tokens, their ranges moved by `offset`.
pub(crate) fn lexer_errors<'a>(
    lexed: &'a parser::LexedStr<'_>,
    offset: TextSize,
) -> impl Iterator<Item = SyntaxError> + 'a {
    lexed.errors().map(move |(i, err)| {
        let text_range = lexed.text_range(i);
        let text_range = TextRange::new(
            text_range.start.try_into().unwrap(),
            text_range.end.try_into().unwrap(),
        );
        SyntaxError::new(err, text_range + offset)
    })
}

/// What parses an outline's lazy node: the body block of a `fn`, its text parsed alone as a
/// block in the file's edition (`data`), which is exactly the subtree a full parse of the
/// file makes there (`parser`'s outline module says why), with that parse's parser errors in
/// the text ([`body_errors`] reads them). Its lexer errors are the outline's, which lexed it.
pub(crate) fn expand_lazy(
    kind: syntax_tree::SyntaxKind,
    source: &Text,
    range: TextRange,
    data: u32,
) -> (FlatTree, Vec<(TextRange, Box<str>)>) {
    let kind = <RustLanguage as syntax_tree::Language>::kind_from_raw(kind);
    let edition = Edition::from_u32(data);
    let reparser = parser::Reparser::for_node(kind, None, None)
        .unwrap_or_else(|| panic!("a lazy {kind:?}: only blocks are left unparsed"));
    let lexed = parser::LexedStr::new(edition, &source[range]);
    let output = reparser.parse(&lexed.to_input(edition));
    // clone: one count of the file's text, which the part shares
    let (tree, errors, is_eof) = build_parsed(source.clone(), range, &lexed, &output, edition);
    assert!(is_eof, "a lazy {kind:?} of {} bytes parsed short of its end", u32::from(range.len()));
    let errors = errors
        .into_iter()
        .map(|it| {
            let (msg, range) = it.into_parts();
            (range, msg.into_boxed_str())
        })
        .collect();
    (tree, errors)
}

/// The parser's errors in lazy node `at` of `tree`, an outline, parsing it if it was not yet:
/// the errors a full parse of the file reports there, from the one parse of the body
/// ([`FlatTree::part_errors`]).
pub(crate) fn body_errors(tree: &FlatTree, at: u32) -> impl Iterator<Item = SyntaxError> + '_ {
    tree.part_errors(at).iter().map(|(range, msg)| SyntaxError::new(&**msg, *range))
}
