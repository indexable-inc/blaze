//! The outline of a token record (drive.md D122 rule 3, "`outline_tokens` from tokens"): the
//! items an expansion declares, read from its token record with no text, no lexer and no byte.
//!
//! The record's words go to the item grammar as they are ([`Tokens::parser_tokens`]: the
//! parser's words with their pad, each with the `JOINT` bit its writer set, so which tokens are
//! joined is the record's own fact and nothing is re-joined from where tokens stand in a text).
//! As in [`crate::outline`], a stack tracks the nodes open at each step. Every grammar node
//! directly in the file or in an entry's list ([`is_list`]) gets an entry, including item
//! forms the text outline marks unhandled, except for attributes and error nodes.
//!
//! Every position is a [`TokenRange`] of the record: an entry's extent from its first token
//! (an attribute's, a doc comment's `#`) to its last, each outer attribute's, its name as one
//! token. A name is that token's lexeme ([`Tokens::text`]); an entry's digest is
//! [`Tokens::digest`] of its extent, so it changes when one of its tokens does (its text, its
//! word, its join, its context's marks) and not when a token of another entry or a space or a
//! comment of the text the tokens came from does.
//!
//! The file path of an outline (crate::walk::rows) is over bytes and stays so: a file is text.
//! This path is for a record that has no text.

use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, Vec};
use blaze_tokens::{TokenRange, Tokens};
use parser::{
    Step,
    SyntaxKind::{self, *},
    TopEntryPoint,
};

use super::{is_error_entry, is_list, is_name_child};

/// One entry of a token outline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TokenEntry {
    pub kind: SyntaxKind,
    /// The entry it is written directly in; `None` for one in the record's top level.
    pub parent: Option<u32>,
    /// One past its last descendant: its descendants are the entries after it up to here.
    pub end: u32,
    /// Its tokens, its outer attributes and doc comments first.
    pub extent: TokenRange,
    /// Its outer attributes, a range of [`TokenOutline::attrs`].
    pub attrs: TokenRange,
    /// The token that is its name, for a named entry.
    pub name: Option<u32>,
    /// [`Tokens::digest`] of `extent`.
    pub digest: ContentId,
}

/// What [`outline_token_record`] gives, in the allocator its caller names.
pub struct TokenOutline<'a> {
    entries: AVec<'a, TokenEntry>,
    attrs: AVec<'a, TokenRange>,
    errors: AVec<'a, u32>,
}

impl<'a> TokenOutline<'a> {
    /// Every entry, in token order: an entry before the entries written inside it.
    pub fn entries(&self) -> &[TokenEntry] {
        &self.entries
    }

    /// Every entry's outer attributes, each entry's together ([`TokenEntry::attrs`]).
    pub fn attrs(&self, entry: &TokenEntry) -> &[TokenRange] {
        &self.attrs[entry.attrs.start as usize..entry.attrs.end as usize]
    }

    /// `entry`'s name in `tokens`, the record it is the outline of.
    pub fn name<'t>(&self, entry: &TokenEntry, tokens: Tokens<'t>) -> Option<&'t str> {
        entry.name.map(|at| tokens.text(at as usize))
    }

    /// The token before which each syntax error of the parse was found, in the order found.
    pub fn errors(&self) -> &[u32] {
        &self.errors
    }
}

/// Why a token record has no outline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenOutlineRefused {
    /// The view does not reach its record's end, so its words carry no pad for the parser: an
    /// outline is of a whole record.
    NotWhole { offset: u32, tokens: u32 },
    /// The parse's steps took `read` tokens of a record of `tokens`.
    Steps { read: u32, tokens: u32 },
}

impl core::fmt::Display for TokenOutlineRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TokenOutlineRefused::NotWhole { offset, tokens } => write!(
                f,
                "a token outline of {tokens} tokens from token {offset}: a view that does not reach its record's end"
            ),
            TokenOutlineRefused::Steps { read, tokens } => {
                write!(f, "a token outline whose parse read {read} of the record's {tokens} tokens")
            }
        }
    }
}

impl core::error::Error for TokenOutlineRefused {}

/// An open node of the walk.
#[derive(Clone, Copy)]
struct Open {
    kind: SyntaxKind,
    start: u32,
    /// The entry it is, if it is one.
    entry: Option<u32>,
    /// The nodes directly in it are entries: the file, or an entry's list.
    list: bool,
}

/// The outline of `tokens`, a whole token record, in `alloc` (see the module's doc).
pub fn outline_token_record<'a>(
    tokens: Tokens<'_>,
    alloc: Alloc<'a>,
) -> Result<TokenOutline<'a>, TokenOutlineRefused> {
    let count = u32::try_from(tokens.len()).expect("a token record counts its tokens in u32");
    let input = tokens
        .parser_tokens()
        .ok_or(TokenOutlineRefused::NotWhole { offset: tokens.offset(), tokens: count })?;
    let output = TopEntryPoint::SourceFile.parse(input);
    let mut out = TokenOutline { entries: Vec::new_in(alloc), attrs: Vec::new_in(alloc), errors: Vec::new_in(alloc) };
    let mut open: AVec<'_, Open> = Vec::new_in(alloc);
    let mut at = 0u32;
    for step in output.iter() {
        match step {
            Step::Enter { kind } => {
                let parent = open.last().copied();
                let entry = match parent.is_some_and(|p| p.list) && !is_error_entry(kind) {
                    true => {
                        let index = u32::try_from(out.entries.len()).expect("entries counted in u32");
                        let attrs = u32::try_from(out.attrs.len()).expect("attributes counted in u32");
                        out.entries.push(TokenEntry {
                            kind,
                            parent: open.iter().rev().find_map(|o| o.entry),
                            end: index + 1,
                            extent: TokenRange::new(at, at),
                            attrs: TokenRange::new(attrs, attrs),
                            name: None,
                            digest: ContentId([0; 32]),
                        });
                        Some(index)
                    }
                    false => None,
                };
                let list = kind == SOURCE_FILE || (is_list(kind) && parent.is_some_and(|p| p.entry.is_some()));
                open.push(Open { kind, start: at, entry, list });
            }
            Step::Exit => {
                let node = open.pop().expect("the parse exits only a node it entered");
                let range = TokenRange::new(node.start, at);
                if let Some(index) = node.entry {
                    let ends = u32::try_from(out.entries.len()).expect("entries counted in u32");
                    let entry = &mut out.entries[index as usize];
                    entry.extent = range;
                    entry.end = ends;
                }
                // A node directly in an entry: its outer attribute, or its name.
                if let Some(owner) = open.last().and_then(|o| o.entry) {
                    let entry = &mut out.entries[owner as usize];
                    match node.kind {
                        ATTR => {
                            out.attrs.push(range);
                            let attrs = u32::try_from(out.attrs.len()).expect("attributes counted in u32");
                            entry.attrs.end = attrs;
                        }
                        child if is_name_child(entry.kind, child) && !range.is_empty() => {
                            entry.name.get_or_insert(range.start);
                        }
                        _ => {}
                    }
                }
            }
            Step::Token { n_input_tokens, .. } => at += u32::from(n_input_tokens),
            // A float literal read as a field's parts is still one token of the record.
            Step::FloatSplit { .. } => at += 1,
            Step::Error { .. } => out.errors.push(at),
        }
    }
    if at != count {
        return Err(TokenOutlineRefused::Steps { read: at, tokens: count });
    }
    for entry in out.entries.iter_mut() {
        entry.digest = tokens.digest(entry.extent);
    }
    Ok(out)
}
