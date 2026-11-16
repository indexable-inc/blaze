//! A file cut into its top-level items, and the outlines of the parts put back together
//! (crate docs, "A file is outlined item by item").

use blaze_hash::ByHash;
use blaze_ids::ContentId;
use blaze_lex::{Cpu, DocStyle, FrontmatterAllowed, TokenKind, skip_body, tokenize};
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::{AttrSpan, ErrorOrigin, ItemIndex, Range, SyntaxError};

use crate::Outline;

/// The parts of `text` its top-level items are in, in order, contiguous, covering it: each
/// part is one item or more, with the trivia, attributes and doc comments before them.
///
/// The scan lexes only outside top-level brace groups, whose ends `blaze_lex::skip_body` finds.
/// It cuts after a `;` outside brackets unless an inner attribute or doc comment follows (that
/// is an error entry, which the walk tells apart only after an item), and after a top-level
/// `}` when what follows can only start an item: an identifier other than `as` or `else`, an
/// outer attribute, an outer doc comment. Nowhere else: no expression or type at the top level
/// has a `;` outside brackets, or a `}` followed by such a token, inside one item. A brace
/// group with no end (an unclosed `{`) ends the cutting; the rest is one part.
pub fn split_top_level<'a>(text: &str, alloc: Alloc<'a>) -> AVec<'a, Range> {
    let cpu = Cpu::BASELINE;
    let bytes = text.as_bytes();
    let mut cuts: AVec<'a, u32> = Vec::new_in(alloc);
    let offset = |at: usize| u32::try_from(at).expect("a file under 4 GiB");
    // A shebang line is the parser's to read: the scan starts past it.
    let mut pos = match bytes.starts_with(b"#!") && !bytes.starts_with(b"#![") {
        true => blaze_memchr::memchr(b'\n', bytes).map_or(bytes.len(), |at| at + 1),
        false => 0,
    };
    let mut frontmatter = FrontmatterAllowed::Yes;
    let mut depth = 0u32;
    let mut pending = Pending::None;
    'restart: while pos < bytes.len() {
        let mut at = pos;
        for token in tokenize(&text[pos..], frontmatter, cpu) {
            let start = at;
            at += usize::try_from(token.len).expect("a token's length fits usize");
            let kind = token.kind;
            if is_trivia(kind) {
                continue;
            }
            pending = match pending {
                Pending::None => Pending::None,
                Pending::Semi(cut) | Pending::Brace(cut) if kind == TokenKind::Pound => Pending::Pound(cut),
                Pending::Pound(cut) => {
                    if kind != TokenKind::Bang {
                        cuts.push(cut);
                    }
                    Pending::None
                }
                Pending::Semi(cut) => {
                    if !is_doc(kind, DocStyle::Inner) {
                        cuts.push(cut);
                    }
                    Pending::None
                }
                Pending::Brace(cut) => {
                    let word = &text[start..at];
                    if kind == TokenKind::Ident && word != "as" && word != "else" || is_doc(kind, DocStyle::Outer) {
                        cuts.push(cut);
                    }
                    Pending::None
                }
            };
            match kind {
                TokenKind::OpenBrace if depth == 0 => {
                    let Ok(inside) = skip_body(cpu, &text[start..]) else { break 'restart };
                    pos = start + inside + 1;
                    pending = Pending::Brace(offset(pos));
                    frontmatter = FrontmatterAllowed::No;
                    continue 'restart;
                }
                TokenKind::Semi if depth == 0 => pending = Pending::Semi(offset(at)),
                TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace => depth += 1,
                // An unbalanced closer is an error the parser reports; the scan stays at the top.
                TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => {
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
        }
        break;
    }
    let mut parts: AVec<'a, Range> = Vec::with_capacity_in(cuts.len() + 1, alloc);
    let mut from = 0;
    for cut in cuts {
        parts.push(Range::new(from, cut));
        from = cut;
    }
    parts.push(Range::new(from, offset(bytes.len())));
    parts
}

/// A cut the scan may make, once it reads the next token: after a `;`, after a top-level `}`,
/// or after either when a `#` followed (the cut is made unless a `!` follows the `#`).
#[derive(Clone, Copy)]
enum Pending {
    None,
    Semi(u32),
    Brace(u32),
    Pound(u32),
}

fn is_trivia(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Whitespace
            | TokenKind::LineComment { doc_style: None }
            | TokenKind::BlockComment { doc_style: None, .. }
    )
}

fn is_doc(kind: TokenKind, style: DocStyle) -> bool {
    match kind {
        TokenKind::LineComment { doc_style } | TokenKind::BlockComment { doc_style, .. } => doc_style == Some(style),
        _ => false,
    }
}

/// The outline of a file from the outlines of its parts ([`split_top_level`],
/// [`outline_item`](crate::outline_item)), each with the file offset its part starts at, in
/// order: equal to the outline of the whole file. Each entry's file range, parent and end, and
/// each error's range, move by its part's base; the top-level disambiguators are counted again
/// over every part's sibling keys; the file's attributes are the first part's.
///
/// # Panics
///
/// When a part but the first holds file attributes, which [`split_top_level`] never cuts for.
pub fn assemble<'o, 'a: 'o>(
    parts: impl Iterator<Item = (u32, &'o Outline<'a>)> + Clone,
    alloc: Alloc<'a>,
) -> Outline<'a> {
    let mut out = Outline {
        items: Vec::new_in(alloc),
        attrs: Vec::new_in(alloc),
        file_attrs: AttrSpan::default(),
        errors: Vec::new_in(alloc),
        messages: Vec::new_in(alloc),
        keys: Vec::new_in(alloc),
    };
    let mut siblings: ByHash<ContentId, u32, Alloc<'a>> = ByHash::new_in(alloc);
    let count = |len: usize| u32::try_from(len).expect("an outline's entries are counted in u32");
    for (index, (base, part)) in parts.clone().enumerate() {
        let items = count(out.items.len());
        let attrs = count(out.attrs.len());
        if index == 0 {
            out.file_attrs = part.file_attrs;
        } else {
            assert!(part.file_attrs.start == part.file_attrs.end, "a part but the first holds file attributes");
        }
        let mut keys = part.keys.iter();
        for item in part.items.iter() {
            let mut item = *item;
            item.range = item.range.shifted(base);
            item.parent = item.parent.map(|parent| ItemIndex(parent.0 + items));
            item.end = ItemIndex(item.end.0 + items);
            item.attrs = AttrSpan { start: item.attrs.start + attrs, end: item.attrs.end + attrs };
            if item.parent.is_none() {
                let key = *keys.next().expect("a part's outline has a key for each top-level entry");
                let seen = siblings.entry(key).or_insert(0);
                item.disambiguator = *seen;
                *seen += 1;
                out.keys.push(key);
            }
            out.items.push(item);
        }
        out.attrs.extend_from_slice(&part.attrs);
    }
    // The parser's errors in the order found, then the lexer's, as a whole file's walk has them.
    for origin in [ErrorOrigin::Parser, ErrorOrigin::Lexer] {
        for (base, part) in parts.clone() {
            for error in part.errors.iter().filter(|error| error.origin == origin) {
                let start = count(out.messages.len());
                out.messages.extend_from_slice(&part.messages[error.message.to_usize()]);
                let end = count(out.messages.len());
                out.errors.push(SyntaxError {
                    range: error.range.shifted(base),
                    origin,
                    message: Range::new(start, end),
                });
            }
        }
    }
    out
}
