//! The portable form to tt's token trees and back, for mbe and `stringify!`.
//!
//! A token on its way through them gets a span that says where it is from and nothing else:
//! its anchor's file is the stream's role ([`Role`]), its range starts at twice its index
//! (and is two long when the token is joint with the next, so mbe's `joined` check of two
//! source tokens holds), and its context is the root of its edition. Every span that comes
//! back maps to a [`TokenOrigin`]; one of another anchor is refused.

use intern::Symbol;
use span::{
    EditionedFileId, FileId, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext, TextRange,
    TextSize,
};

use crate::{
    error::{ExpandError, Stream, StreamError, Table},
    expand::Out,
    expansion::TokenOrigin,
    tokens::{
        Delim, Edition, LitKind, Raw, Spacing, TokenKind, TokenStream, Transparency, span_edition,
    },
};

/// Which stream a span's token is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Role {
    Input,
    AttrArgs,
    Def,
    Params,
    /// The call site: what mbe and the derives give a token they make.
    Site,
}

impl Role {
    fn raw(self) -> u32 {
        match self {
            Role::Input => 0,
            Role::AttrArgs => 1,
            Role::Def => 2,
            Role::Params => 3,
            Role::Site => 4,
        }
    }
}

/// The span of token `at` of a stream of `role`.
fn span_of(role: Role, at: usize, edition: Edition, spacing: Spacing) -> Result<Span, StreamError> {
    let too_large = StreamError::TooLarge { table: Table::Positions };
    let start = u32::try_from(at).ok().and_then(|at| at.checked_mul(2)).ok_or(too_large)?;
    let end = start.checked_add(1 + u32::from(spacing != Spacing::Alone)).ok_or(too_large)?;
    Ok(Span {
        range: TextRange::new(TextSize::from(start), TextSize::from(end)),
        anchor: SpanAnchor {
            file_id: EditionedFileId::new(FileId::from_raw(role.raw()), span_edition(edition)),
            ast_id: ROOT_ERASED_FILE_AST_ID,
        },
        ctx: SyntaxContext::root(span_edition(edition)),
    })
}

/// The span of the call site, in `edition`.
pub(crate) fn site_span(edition: Edition) -> Span {
    span_of(Role::Site, 0, edition, Spacing::Alone).expect("index 0 has a position")
}

/// `stream` as a token tree under an invisible top group, each token's span its origin.
pub(crate) fn to_tt(
    stream: &TokenStream<'_>,
    role: Role,
    which: Stream,
    site: Edition,
) -> Result<tt::TopSubtree, ExpandError> {
    if let Some(token) = stream.unbalanced() {
        return Err(ExpandError::Unbalanced { stream: which, token });
    }
    let in_stream = |error| ExpandError::Stream { stream: Some(which), error };
    let mut builder = tt::TopSubtreeBuilder::new(tt::Delimiter::invisible_spanned(site_span(site)));
    for (at, token) in stream.tokens().iter().enumerate() {
        let edition = stream.edition_of(token.ctx);
        let span = span_of(role, at, edition, token.kind.spacing()).map_err(in_stream)?;
        let text = stream.text_of(token);
        match token.kind {
            TokenKind::Open(delim) => builder.open(delim.to_tt(), span),
            TokenKind::Close(_) => builder.close(span),
            TokenKind::Ident { raw, .. } => builder.push(tt::Leaf::Ident(tt::Ident {
                sym: Symbol::intern(text),
                span,
                is_raw: match raw {
                    Raw::No => tt::IdentIsRaw::No,
                    Raw::Yes => tt::IdentIsRaw::Yes,
                },
            })),
            TokenKind::Punct { char, spacing } => {
                builder.push(tt::Leaf::Punct(tt::Punct { char, spacing: spacing.to_tt(), span }));
            }
            TokenKind::Literal { kind, suffix_len, .. } => {
                builder.push(tt::Leaf::Literal(tt::Literal {
                    text_and_suffix: Symbol::intern(text),
                    span,
                    kind: kind.to_tt(),
                    suffix_len,
                }));
            }
        }
    }
    Ok(builder.build())
}

/// The origin a span given by [`to_tt`] or [`site_span`] names; a site span is a token made
/// with `made`.
pub(crate) fn origin_of(span: Span, made: Transparency) -> Result<TokenOrigin, ExpandError> {
    let role = span.anchor.file_id.file_id().index();
    let at = u32::from(span.range.start()) / 2;
    match role {
        0 => Ok(TokenOrigin::Input(at)),
        1 => Ok(TokenOrigin::AttrArgs(at)),
        2 => Ok(TokenOrigin::Def(at)),
        3 => Ok(TokenOrigin::Params(at)),
        4 => Ok(TokenOrigin::Made(made)),
        _ => Err(ExpandError::ForeignSpan { role }),
    }
}

/// The spacing a span says: two long is joint with the next token.
fn spacing_of(span: Span) -> Spacing {
    match u32::from(span.range.len()) {
        2 => Spacing::JointHidden,
        _ => Spacing::Alone,
    }
}

/// Appends the token trees of `iter` to `out`, each with the origin its span names.
pub(crate) fn from_tt(
    out: &mut Out<'_, '_>,
    iter: tt::TtIter<'_>,
    made: Transparency,
) -> Result<(), ExpandError> {
    for element in iter {
        match element {
            tt::TtElement::Leaf(tt::Leaf::Ident(ident)) => {
                let origin = origin_of(ident.span, made)?;
                let raw = if ident.is_raw.yes() { Raw::Yes } else { Raw::No };
                let kind = TokenKind::Ident { raw, spacing: spacing_of(ident.span) };
                out.push(kind, ident.sym.as_str(), origin)?;
            }
            tt::TtElement::Leaf(tt::Leaf::Punct(punct)) => {
                let kind =
                    TokenKind::Punct { char: punct.char, spacing: Spacing::of_tt(punct.spacing) };
                out.push(kind, "", origin_of(punct.span, made)?)?;
            }
            tt::TtElement::Leaf(tt::Leaf::Literal(literal)) => {
                let kind = TokenKind::Literal {
                    kind: LitKind::of_tt(literal.kind),
                    suffix_len: literal.suffix_len,
                    spacing: spacing_of(literal.span),
                };
                out.push(kind, literal.text_and_suffix.as_str(), origin_of(literal.span, made)?)?;
            }
            tt::TtElement::Subtree(subtree, inner) => {
                let delim = Delim::of_tt(subtree.delimiter.kind);
                out.push(TokenKind::Open(delim), "", origin_of(subtree.delimiter.open, made)?)?;
                from_tt(out, inner, made)?;
                out.push(TokenKind::Close(delim), "", origin_of(subtree.delimiter.close, made)?)?;
            }
        }
    }
    Ok(())
}

/// `tt::stringify`'s question about a token of a stream given by [`to_tt`]: whether the source
/// wrote it right against the next one.
pub(crate) fn source_joined(stream: &TokenStream<'_>, span: Span) -> Option<bool> {
    if span.anchor.file_id.file_id().index() != Role::Input.raw() {
        return None;
    }
    let at = (u32::from(span.range.start()) / 2) as usize;
    stream.tokens().get(at).map(|token| token.kind.spacing() != Spacing::Alone)
}
