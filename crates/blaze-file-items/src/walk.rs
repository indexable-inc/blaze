//! What makes an [`Outline`]: the item grammar's sink that writes the rows as each item closes
//! ([`rows`]), and here what it reads a node by: which kinds are items, lists and named
//! entries, how a visibility, a rename and an attribute are read off their tokens, and the
//! worker's scratch the making grows in.

use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::{
    Attr, AttrKind, AttrSpan, ErrorOrigin, Item, ItemDetail, ItemFlags, ItemIndex, Range, Rename,
    SyntaxError, WrittenVis,
};
use parser::{
    Edition, LexedStr,
    SyntaxKind::{self, *},
};

use crate::flags::Scan;
use crate::Outline;
use crate::digest::{By, Compress, Feed, Keyed, Lexed, digest};

/// What an entry is before the walk closes it.
const PLACEHOLDER: Item = Item {
    detail: ItemDetail::Error,
    parent: None,
    end: ItemIndex(0),
    range: Range::new(0, 0),
    name: None,
    visibility: WrittenVis::Private,
    signature: Range::new(0, 0),
    body: None,
    attrs: AttrSpan { start: 0, end: 0 },
    disambiguator: 0,
    hash: ContentId([0; 32]),
    signature_hash: ContentId([0; 32]),
    flags: ItemFlags::NONE,
};

/// What an outline's making grows and no caller keeps, one set per worker at the most it ever
/// held (`blaze_mem::scratch`, docs/v2/plan/design/memory.md, "Scratch buffers reused per
/// worker"), like the lexer's tokens: the outline as the sink appends to it and the sink's
/// working vectors. In a bump frame two vectors that grow by turns copy each other at every
/// step, and a working vector is never freed while a kept one is above it, so the frame would
/// hold twice the outline and the working set beside it; here the frame takes the outline
/// once, at its exact size ([`Outline::copy_in`]), and the worker's set stops growing once
/// warm. The heap is `Alloc::global`'s, named here and nowhere else in the crate.
struct Buffers {
    out: Outline<'static>,
    working: Working<'static>,
}

impl Default for Buffers {
    fn default() -> Buffers {
        let alloc = Alloc::global();
        Buffers {
            out: Outline {
                items: Vec::new_in(alloc),
                attrs: Vec::new_in(alloc),
                file_attrs: AttrSpan::default(),
                errors: Vec::new_in(alloc),
                messages: Vec::new_in(alloc),
                keys: Vec::new_in(alloc),
                names: Vec::new_in(alloc),
                file_names: crate::names_digest::file_names_digest("", &[]),
                own_flags: ItemFlags::NONE,
                flags: ItemFlags::NONE,
                extensions: parser::Extensions::NONE,
            },
            working: Working::new_in(alloc),
        }
    }
}

std::thread_local! {
    /// The thread's [`Buffers`], when none is lent.
    static BUFFERS: core::cell::Cell<Option<Buffers>> = const { core::cell::Cell::new(None) };
}

impl blaze_mem::scratch::Shelved for Buffers {
    fn empty(&mut self) {
        let Buffers { out, working } = self;
        let Outline { items, attrs, file_attrs, errors, messages, keys, names, file_names, own_flags, flags, extensions } = out;
        items.clear();
        attrs.clear();
        errors.clear();
        messages.clear();
        keys.clear();
        names.clear();
        *file_names = crate::names_digest::file_names_digest("", &[]);
        *file_attrs = AttrSpan::default();
        *own_flags = ItemFlags::NONE;
        *flags = ItemFlags::NONE;
        *extensions = parser::Extensions::NONE;
        // The working vectors are cleared by the next outline, each as it starts.
        let Working { rows: _, columns: _, digest: _ } = working;
    }

    fn held(&self) -> usize {
        self.out.items.capacity() + self.working.columns.held()
    }

    fn unshelve() -> Option<Buffers> {
        BUFFERS.try_with(core::cell::Cell::take).ok().flatten()
    }

    fn shelve(self) {
        // A thread that is ending: the buffers are freed.
        let _ = BUFFERS.try_with(|shelf| shelf.set(Some(self)));
    }
}

/// The working vectors of an outline's making, which no outline keeps: the token columns the
/// grammar and its sink read, the sink's stacks, and the digests' scratch. One set serves text
/// after text, cleared and never freed (rule 11; docs/v2/plan/design/memory.md, the frame
/// reset per item).
pub(crate) struct Working<'a> {
    rows: rows::Stacks<'a>,
    columns: rows::Columns<'a>,
    digest: crate::digest::Scratch<'a>,
}

pub(crate) mod rows;
pub(crate) mod token_outline;

impl<'a> Working<'a> {
    /// Empty: nothing is allocated until the first outline.
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Working<'a> {
        Working {
            rows: rows::Stacks::new_in(alloc),
            columns: rows::Columns::new_in(alloc),
            digest: crate::digest::Scratch::new_in(alloc),
        }
    }
}

/// Whether the parser has a `{ .. }` body that holds an item or a macro call at any depth
/// (an `is_item` node) among input tokens `from..from + len`, when the tokens may say so: false
/// only when none of them can start one. Every item but a macro call starts with its keyword
/// (`fn`, `struct`, `enum`, `trait`, `impl`, `const`, `static`, `type`, `mod`, `use`,
/// `extern`, `macro`, or the contextual `union`); `macro_rules!` and every macro call have a
/// `!` right after a path, whose last token is an identifier, a path keyword or a generic
/// argument list's `>`. The grammar takes a `!` there as a call's (`path_expr`, `type_path`,
/// `path_or_macro_pat`) unless it is a joint `!=` in an expression or type; only a pattern
/// right before a joint `!=` (`let a != b`), which nothing valid has, is a call this misses.
/// A `const` right after `*` is a pointer type's, never an item's.
fn may_hold_items(input: parser::Tokens<'_>, from: usize, len: usize) -> bool {
    let mut prev = EOF;
    for at in from..from + len {
        let kind = input.kind(at);
        let item = match kind {
            FN_KW | STRUCT_KW | ENUM_KW | TRAIT_KW | IMPL_KW | STATIC_KW | TYPE_KW | MOD_KW
            | USE_KW | EXTERN_KW | MACRO_KW => true,
            CONST_KW => prev != STAR,
            IDENT => input.contextual_kind(at) == UNION_KW,
            BANG => {
                matches!(prev, IDENT | SELF_KW | SUPER_KW | CRATE_KW | SELF_TYPE_KW | R_ANGLE)
                    && !(input.is_joint(at) && input.kind(at + 1) == EQ)
            }
            _ => false,
        };
        if item {
            return true;
        }
        prev = kind;
    }
    false
}

/// How an outline reads a `fn` body: parsed with the rest, or left unparsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bodies {
    /// Every body parsed: the definition the skipping outline is tested equal to.
    Parsed,
    /// A body the grammar may leave unparsed (`parser::lazy_block_len`) is parsed only when its
    /// tokens may hold an item or a macro call ([`may_hold_items`]), and then only as far as
    /// the first statement that does (`parser::Sink::satisfied`). The whole text is lexed
    /// either way, so the lexer's findings are the same; the parser's findings inside a body
    /// left unparsed are not the outline's: they are found when the body is parsed for its own
    /// record.
    Skipped,
}

/// A token of a visibility, a rename or an attribute, recorded for reading it
/// ([`visibility_of`], [`rename_of`], [`attr_of`]).
#[derive(Clone, Copy)]
struct Tok {
    kind: SyntaxKind,
    range: Range,
}

/// Pushes an error at `range` with `message`, copied onto the end of `messages`.
fn push_error(errors: &mut AVec<'_, SyntaxError>, messages: &mut AVec<'_, u8>, range: Range, origin: ErrorOrigin, message: &str) {
    let start = u32::try_from(messages.len()).expect("messages under 4 GiB");
    messages.extend_from_slice(message.as_bytes());
    let end = u32::try_from(messages.len()).expect("messages under 4 GiB");
    errors.push(SyntaxError { range, origin, message: Range::new(start, end) });
}

/// `part`, a range of the file inside an item starting at `base`, relative to the item.
fn relative(part: Range, base: u32) -> Range {
    let at = |offset: u32| offset.checked_sub(base).expect("a part of an item is inside it");
    Range::new(at(part.start), at(part.end))
}

/// The edition as one byte of a hash, by name, so a reordering of `Edition` changes no hash.
fn edition_byte(edition: Edition) -> u8 {
    match edition {
        Edition::Edition2015 => 0,
        Edition::Edition2018 => 1,
        Edition::Edition2021 => 2,
        Edition::Edition2024 => 3,
    }
}

/// The kinds of node an item list holds as items (and a body holds as items or macro calls).
fn is_item(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        CONST
            | ENUM
            | EXTERN_BLOCK
            | EXTERN_CRATE
            | FN
            | IMPL
            | MACRO_CALL
            | MACRO_RULES
            | MACRO_DEF
            | MODULE
            | STATIC
            | STRUCT
            | TRAIT
            | TYPE_ALIAS
            | UNION
            | USE
    )
}

/// Whether a direct child supplies the outline entry's name. For `extern crate x as y;`
/// the name is the crate reference `x`: scope lowering resolves that target, then uses
/// the separate rename to determine the local binding.
fn is_name_child(entry: SyntaxKind, child: SyntaxKind) -> bool {
    matches!((entry, child), (_, NAME) | (EXTERN_CRATE, NAME_REF))
}

/// Syntax kinds the text outline classifies as errors when it makes a row for them.
fn is_error_entry(kind: SyntaxKind) -> bool {
    matches!(kind, ERROR | ATTR | DOC_COMMENT)
}

/// The kinds of entry told apart from their siblings by name: the rest, by their head.
fn is_named(syntax: SyntaxKind) -> bool {
    matches!(
        syntax,
        FN | STRUCT
            | ENUM
            | UNION
            | TRAIT
            | CONST
            | STATIC
            | TYPE_ALIAS
            | MODULE
            | EXTERN_CRATE
            | MACRO_RULES
            | MACRO_DEF
            | VARIANT
            | RECORD_FIELD
            | TUPLE_FIELD
    )
}

/// The lists whose children are items, and may hold their owner's inner attributes.
fn is_item_list(kind: SyntaxKind) -> bool {
    matches!(kind, SOURCE_FILE | ITEM_LIST | ASSOC_ITEM_LIST | EXTERN_ITEM_LIST)
}

/// The bodies whose node children are entries.
fn is_list(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        ITEM_LIST
            | ASSOC_ITEM_LIST
            | EXTERN_ITEM_LIST
            | VARIANT_LIST
            | RECORD_FIELD_LIST
            | TUPLE_FIELD_LIST
    )
}

/// The kind of an entry as one byte of its sibling key: its variant, not its payload.
fn tag_of(kind: ItemDetail) -> u8 {
    match kind {
        ItemDetail::Fn { .. } => 0,
        ItemDetail::Struct => 1,
        ItemDetail::Enum => 2,
        ItemDetail::Union => 3,
        ItemDetail::Trait => 4,
        ItemDetail::TraitAlias => 5,
        ItemDetail::Impl => 6,
        ItemDetail::Const { .. } => 7,
        ItemDetail::Static { .. } => 8,
        ItemDetail::TypeAlias => 9,
        ItemDetail::Mod { .. } => 10,
        ItemDetail::Use { .. } => 11,
        ItemDetail::ExternCrate { .. } => 12,
        ItemDetail::ExternBlock { .. } => 13,
        ItemDetail::MacroRules { .. } => 14,
        ItemDetail::MacroDef => 15,
        ItemDetail::MacroCall { .. } => 16,
        ItemDetail::Variant { .. } => 17,
        ItemDetail::Field { .. } => 18,
        ItemDetail::Error => 19,
        ItemDetail::Unhandled { .. } => 20,
    }
}

/// A visibility from its tokens: `pub`, then `( .. )` or nothing. Ranges are the file's.
fn visibility_of(tokens: &[Tok]) -> WrittenVis<Range> {
    let Some(paren) = tokens.get(1).filter(|token| token.kind == L_PAREN) else {
        return WrittenVis::Pub;
    };
    let inside = match tokens {
        [_, _, inside @ .., close] if close.kind == R_PAREN => inside,
        [_, _, inside @ ..] => inside,
        _ => &[],
    };
    let span = |path: &[Tok]| match (path.first(), path.last()) {
        (Some(first), Some(last)) => Range::new(first.range.start, last.range.end),
        _ => Range::new(paren.range.end, paren.range.end),
    };
    match inside {
        [only] if only.kind == CRATE_KW => WrittenVis::Crate,
        [only] if only.kind == SUPER_KW => WrittenVis::Super,
        [only] if only.kind == SELF_KW => WrittenVis::SelfOnly,
        [first, path @ ..] if first.kind == IN_KW => WrittenVis::In(span(path)),
        path => WrittenVis::WithoutIn(span(path)),
    }
}

/// An `extern crate`'s rename from its tokens: `as`, then a name or `_`.
fn rename_of(tokens: &[Tok]) -> Option<Rename<Range>> {
    match tokens.get(1) {
        Some(token) if token.kind == UNDERSCORE => Some(Rename::Underscore),
        Some(token) => Some(Rename::To(token.range)),
        None => None,
    }
}

/// Whether an attribute (`#[..]`, `#![..]`) or doc comment is inner, and what it is, from its
/// tokens; a `#[path]` literal's range is the file's.
fn attr_of(kind: SyntaxKind, tokens: &[Tok], text: &str) -> (bool, AttrKind) {
    if kind == DOC_COMMENT {
        return (tokens.first().is_some_and(|token| token.kind == INNER_DOC_COMMENT), AttrKind::Doc);
    }
    let is = |at: usize, kind: SyntaxKind| tokens.get(at).is_some_and(|token| token.kind == kind);
    let inner = is(1, BANG);
    // Past `#`, `!` and `[`, and an `unsafe(` around the meta.
    let mut at = if inner { 3 } else { 2 };
    if is(at, UNSAFE_KW) && is(at + 1, L_PAREN) {
        at += 2;
    }
    // A path of one segment: `a::b` names none of these.
    let Some(first) = tokens
        .get(at)
        .filter(|token| token.kind == IDENT && !is(at + 1, COLON) && !is(at + 1, COLON2))
    else {
        return (inner, AttrKind::Other);
    };
    let attr = match first.range.of(text) {
        "cfg" => AttrKind::Cfg,
        "cfg_attr" => AttrKind::CfgAttr,
        "doc" => AttrKind::Doc,
        "macro_export" => AttrKind::MacroExport,
        "path" if is(at + 1, EQ) && is(at + 2, STRING) => {
            AttrKind::Path { literal: tokens[at + 2].range }
        }
        _ => AttrKind::Other,
    };
    (inner, attr)
}
