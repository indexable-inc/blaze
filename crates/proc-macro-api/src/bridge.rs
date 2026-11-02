//! What blaze-macro-eval's in-process server speaks to a macro: the lexer of text a macro asks
//! to be tokens of ([`lex`]), the span operations it answers ([`spans`]) and the questions of
//! them only the database can answer ([`questions`]), and the span numbering and token-tree
//! columns of an expansion ([`wire`]).

pub mod lex;
pub mod questions;
pub mod spans;
pub mod wire;

/// A span of the root of a test file, at `start..end`.
#[cfg(test)]
fn at(start: u32, end: u32) -> span::Span {
    use span::{EditionedFileId, Span, SpanAnchor, SyntaxContext, TextRange};
    let file_id = EditionedFileId::new(span::FileId::from_raw(7), span::Edition::CURRENT);
    Span {
        range: TextRange::new(start.into(), end.into()),
        anchor: SpanAnchor { file_id, ast_id: span::ROOT_ERASED_FILE_AST_ID },
        ctx: SyntaxContext::root(span::Edition::CURRENT),
    }
}

/// The tree of `text` lexed at `span`, as a macro's text is given back.
#[cfg(test)]
fn tokens(text: &str, span: span::Span) -> Result<tt::TopSubtree, String> {
    Ok(wire::top_subtree(lex::stream(text, span)?, span))
}
