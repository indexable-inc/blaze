//! The rows of a tree's columns, each a `#[repr(C)]` row of `u32` words, so that a tree is one
//! block of words ([`super::TopSubtree`]) read as it lies, and the wire writes and reads the
//! columns as their bytes.
//!
//! `zerocopy`'s derives check the layouts (`repr(C)`, no padding, every bit pattern valid where
//! a row is read from bytes) and emit the `unsafe impl`s of their traits. They are another
//! crate's macro, which `unsafe_code` does not report, so the module needs no allow and has no
//! unsafe of its own.

use span::{EditionedFileId, ErasedFileAstId, Span, SpanAnchor, SyntaxContext, TextRange};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// One token tree, 8 bytes. `head` holds the tag in its low two bits and above it:
/// - a subtree: its delimiter kind (2 bits), then its ordinal (the number of subtrees before it,
///   the top one being 0); `data` is its length, the number of token trees in it;
/// - a punct: its spacing (2 bits); `data` is its character;
/// - an ident: whether it is raw (1 bit); `data` is its symbol's index;
/// - a literal: its kind (4 bits), the hashes of a raw one (8 bits) and its suffix's length
///   (the 18 bits left, below [`super::MAX_SUFFIX`]); `data` is its symbol's index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub(super) struct Node {
    pub(super) head: u32,
    pub(super) data: u32,
}

impl Node {
    #[inline]
    pub(super) fn from_row([head, data]: [u32; 2]) -> Node {
        Node { head, data }
    }

    #[inline]
    pub(super) fn row(self) -> [u32; 2] {
        [self.head, self.data]
    }
}

/// A span, 8 bytes: where it starts, and its length in the low 16 bits of `len_part` with its
/// part's index in the high 16. A length or index of `0xFFFF` or more makes it wide: then
/// `len_part` is [`super::WIDE`] and `start` the index of a [`WideSpan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub(super) struct PackedSpan {
    pub(super) start: u32,
    pub(super) len_part: u32,
}

impl PackedSpan {
    #[inline]
    pub(super) fn from_row([start, len_part]: [u32; 2]) -> PackedSpan {
        PackedSpan { start, len_part }
    }
}

/// A span too large to pack, 12 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub(super) struct WideSpan {
    pub(super) start: u32,
    pub(super) len: u32,
    pub(super) part: u32,
}

impl WideSpan {
    #[inline]
    pub(super) fn from_row([start, len, part]: [u32; 3]) -> WideSpan {
        WideSpan { start, len, part }
    }
}

/// The parts of a span that tokens near each other share: all but the range, 12 bytes. The
/// words are the raw ids, in the order the proc-macro wire carries them
/// ([`super::WirePart`]): the file, the AST id and the context. Equal parts have equal words.
///
/// Not `FromBytes`: a part is made from a span's anchor and context ([`Self::new`]), or from
/// the words of one ([`Self::from_row`]), so its context's word is always one
/// [`SyntaxContext::into_u32`] gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, IntoBytes, Immutable)]
#[repr(C)]
pub(super) struct CompressedSpanPart {
    file_id: u32,
    ast_id: u32,
    ctx: u32,
}

const _: () = assert!(size_of::<CompressedSpanPart>() == 12);

impl CompressedSpanPart {
    #[inline]
    pub(super) fn new(anchor: SpanAnchor, ctx: SyntaxContext) -> Self {
        CompressedSpanPart {
            file_id: anchor.file_id.as_u32(),
            ast_id: anchor.ast_id.into_raw(),
            ctx: ctx.into_u32(),
        }
    }

    #[inline]
    pub(super) fn from_span(span: &Span) -> Self {
        Self::new(span.anchor, span.ctx)
    }

    /// The part whose [`Self::row`] is `row`: the words of a tree's parts column, which are
    /// only ever written from parts.
    #[inline]
    pub(super) fn from_row([file_id, ast_id, ctx]: [u32; 3]) -> Self {
        CompressedSpanPart { file_id, ast_id, ctx }
    }

    #[inline]
    pub(super) fn row(self) -> [u32; 3] {
        [self.file_id, self.ast_id, self.ctx]
    }

    #[inline]
    pub(super) fn anchor(self) -> SpanAnchor {
        SpanAnchor {
            file_id: EditionedFileId::from_raw(self.file_id),
            ast_id: ErasedFileAstId::from_raw(self.ast_id),
        }
    }

    #[inline]
    pub(super) fn ctx(self) -> SyntaxContext {
        // `self.ctx` is what `SyntaxContext::into_u32` gave for a context of this process: `new`
        // writes it so, and `from_row` reads the words of a part (the type's doc).
        SyntaxContext::from_u32(self.ctx)
    }

    #[inline]
    pub(super) fn recombine(self, range: TextRange) -> Span {
        Span { range, anchor: self.anchor(), ctx: self.ctx() }
    }

    /// The part with its AST id changed by `f`.
    #[inline]
    pub(super) fn with_ast_id(mut self, f: &mut impl FnMut(&mut ErasedFileAstId)) -> Self {
        let mut ast_id = ErasedFileAstId::from_raw(self.ast_id);
        f(&mut ast_id);
        self.ast_id = ast_id.into_raw();
        self
    }
}

/// A column's rows as the words they are, for one copy into a tree's block or onto the wire.
#[inline]
pub(super) fn words<T: IntoBytes + Immutable>(column: &[T]) -> &[u32] {
    <[u32]>::ref_from_bytes(column.as_bytes()).expect("a column's rows are whole aligned words")
}
