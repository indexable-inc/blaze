//! The part of `rustc_span` rustfmt reads: byte positions, spans, symbols and identifiers, the
//! edition, and the source map that turns spans back into text and lines.
//!
//! Follows `compiler/rustc_span/src/lib.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0) in
//! what rustfmt can observe: `Span::new` puts its ends in order, a span's text is the source
//! map's (BOM removed, `\r\n` made `\n`), and files are laid out one after another with a gap of
//! one position, so a position names one file. There are no expansions: every span is written
//! in a file, so its context is the root.

pub mod edition;
pub mod source_map;
pub mod symbol;

use std::cmp;
use std::fmt;
use std::ops::{Add, AddAssign, Sub};
use std::path::{Path, PathBuf};

pub use source_map::{SourceFile, SourceMap};
pub use symbol::{Ident, Symbol, kw, sym};

/// A byte offset into the source map's address space.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct BytePos(pub u32);

/// A character offset, counted in chars.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct CharPos(pub usize);

pub trait Pos {
    fn from_usize(n: usize) -> Self;
    fn to_usize(&self) -> usize;
    fn from_u32(n: u32) -> Self;
    fn to_u32(&self) -> u32;
}

impl Pos for BytePos {
    fn from_usize(n: usize) -> BytePos {
        BytePos(n as u32)
    }
    fn to_usize(&self) -> usize {
        self.0 as usize
    }
    fn from_u32(n: u32) -> BytePos {
        BytePos(n)
    }
    fn to_u32(&self) -> u32 {
        self.0
    }
}

impl Pos for CharPos {
    fn from_usize(n: usize) -> CharPos {
        CharPos(n)
    }
    fn to_usize(&self) -> usize {
        self.0
    }
    fn from_u32(n: u32) -> CharPos {
        CharPos(n as usize)
    }
    fn to_u32(&self) -> u32 {
        self.0 as u32
    }
}

// rustc's positions wrap in a release build, which is what rustfmt ships as; a span rustfmt
// computes past a file's start must come out as rustc's would.
impl Add for BytePos {
    type Output = BytePos;
    fn add(self, rhs: BytePos) -> BytePos {
        BytePos(self.0.wrapping_add(rhs.0))
    }
}

impl Sub for BytePos {
    type Output = BytePos;
    fn sub(self, rhs: BytePos) -> BytePos {
        BytePos(self.0.wrapping_sub(rhs.0))
    }
}

impl AddAssign for BytePos {
    fn add_assign(&mut self, rhs: BytePos) {
        *self = *self + rhs;
    }
}

impl Add for CharPos {
    type Output = CharPos;
    fn add(self, rhs: CharPos) -> CharPos {
        CharPos(self.0.wrapping_add(rhs.0))
    }
}

impl Sub for CharPos {
    type Output = CharPos;
    fn sub(self, rhs: CharPos) -> CharPos {
        CharPos(self.0.wrapping_sub(rhs.0))
    }
}

/// The context of a span: always the root, since nothing is expanded.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct SyntaxContext;

impl SyntaxContext {
    pub const fn root() -> SyntaxContext {
        SyntaxContext
    }
}

/// The root expansion, the only one there is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LocalExpnId;

impl LocalExpnId {
    pub const ROOT: LocalExpnId = LocalExpnId;
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Span {
    lo: BytePos,
    hi: BytePos,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SpanData {
    pub lo: BytePos,
    pub hi: BytePos,
    pub ctxt: SyntaxContext,
    pub parent: Option<LocalDefId>,
}

impl SpanData {
    pub fn span(&self) -> Span {
        Span::new(self.lo, self.hi, self.ctxt, None)
    }
}

pub const DUMMY_SP: Span = Span {
    lo: BytePos(0),
    hi: BytePos(0),
};

impl Span {
    pub fn new(
        mut lo: BytePos,
        mut hi: BytePos,
        _ctxt: SyntaxContext,
        _parent: Option<LocalDefId>,
    ) -> Span {
        if lo > hi {
            std::mem::swap(&mut lo, &mut hi);
        }
        Span { lo, hi }
    }

    pub fn with_root_ctxt(lo: BytePos, hi: BytePos) -> Span {
        Span::new(lo, hi, SyntaxContext, None)
    }

    pub fn lo(self) -> BytePos {
        self.lo
    }

    pub fn hi(self) -> BytePos {
        self.hi
    }

    pub fn data(self) -> SpanData {
        SpanData {
            lo: self.lo,
            hi: self.hi,
            ctxt: SyntaxContext,
            parent: None,
        }
    }

    pub fn ctxt(self) -> SyntaxContext {
        SyntaxContext
    }

    pub fn with_lo(self, lo: BytePos) -> Span {
        Span::new(lo, self.hi, SyntaxContext, None)
    }

    pub fn with_hi(self, hi: BytePos) -> Span {
        Span::new(self.lo, hi, SyntaxContext, None)
    }

    pub fn with_ctxt(self, _ctxt: SyntaxContext) -> Span {
        self
    }

    pub fn is_dummy(self) -> bool {
        self.lo.0 == 0 && self.hi.0 == 0
    }

    pub fn is_empty(self) -> bool {
        self.lo == self.hi
    }

    pub fn from_expansion(self) -> bool {
        false
    }

    pub fn source_callsite(self) -> Span {
        self
    }

    pub fn eq_ctxt(self, _other: Span) -> bool {
        true
    }

    pub fn shrink_to_lo(self) -> Span {
        Span::new(self.lo, self.lo, SyntaxContext, None)
    }

    pub fn shrink_to_hi(self) -> Span {
        Span::new(self.hi, self.hi, SyntaxContext, None)
    }

    pub fn contains(self, other: Span) -> bool {
        self.lo <= other.lo && other.hi <= self.hi
    }

    pub fn overlaps(self, other: Span) -> bool {
        self.lo < other.hi && other.lo < self.hi
    }

    /// From the start of `self` to the end of `end`, whichever order they are in.
    pub fn to(self, end: Span) -> Span {
        Span::new(
            cmp::min(self.lo, end.lo),
            cmp::max(self.hi, end.hi),
            SyntaxContext,
            None,
        )
    }

    /// From the end of `self` to the start of `end`.
    pub fn between(self, end: Span) -> Span {
        Span::new(self.hi, end.lo, SyntaxContext, None)
    }

    /// From the start of `self` to the start of `end`.
    pub fn until(self, end: Span) -> Span {
        Span::new(self.lo, end.lo, SyntaxContext, None)
    }

    pub fn substitute_dummy(self, other: Span) -> Span {
        if self.is_dummy() { other } else { self }
    }
}

/// Stands for rustc's `LocalDefId` in `Span::new`'s signature; spans here have no parent.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LocalDefId;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

pub fn respan<T>(span: Span, node: T) -> Spanned<T> {
    Spanned { node, span }
}

/// The id of an attribute; rustfmt only carries it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct AttrId(pub u32);

/// A file's name as the source map records it.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum FileName {
    Real(RealFileName),
    Custom(String),
}

impl FileName {
    pub fn prefer_local(&self) -> impl fmt::Display + '_ {
        FileNameDisplay(self)
    }
}

impl fmt::Display for FileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        FileNameDisplay(self).fmt(f)
    }
}

struct FileNameDisplay<'a>(&'a FileName);

impl fmt::Display for FileNameDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            FileName::Real(real) => write!(f, "{}", real.local_path.display()),
            FileName::Custom(s) => write!(f, "<{s}>"),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct RealFileName {
    local_path: PathBuf,
}

impl RealFileName {
    pub fn from_path(path: impl Into<PathBuf>) -> RealFileName {
        RealFileName {
            local_path: path.into(),
        }
    }

    pub fn local_path(&self) -> Option<&Path> {
        Some(&self.local_path)
    }

    pub fn into_local_path(self) -> Option<PathBuf> {
        Some(self.local_path)
    }
}

/// Runs `f` with the edition the session's spans are written in, as rustc's session globals
/// hold it; keywords that came with an edition are reserved only from it on.
pub fn create_session_if_not_set_then<R>(edition: edition::Edition, f: impl FnOnce(&()) -> R) -> R {
    let old = edition::set_session_edition(edition);
    let result = f(&());
    edition::set_session_edition(old);
    result
}
