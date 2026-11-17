//! The struct, enum or union a builtin derive is of, read from the item's tokens: its name,
//! generic parameters, `where` predicates, variants and fields, and the `T::Assoc` paths of its
//! field types, each as indices into the item's stream. Nothing is parsed into a tree and
//! nothing is copied: the derive writer copies the tokens it names.
//!
//! The reader knows only what a derive needs. A comma ends a parameter, a predicate, a field or
//! a variant only outside every group and every `<..>` (a `->`'s `>` closes none); in a
//! variant's discriminant or a field's default, after a lone `=`, `<` and `>` are operators and
//! count for nothing.

use core::ops::Range;

use blaze_own::{AVec, Alloc, Vec};

use crate::tokens::{Delim, Raw, Spacing, Token, TokenKind, TokenStream};

/// What a derive is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AdtKind {
    Struct,
    /// `default_variant` is the first variant marked `#[default]`, by index.
    Enum {
        default_variant: Option<usize>,
    },
    Union,
}

/// How a struct's or a variant's fields are written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FieldsKind {
    Record,
    Tuple,
    Unit,
}

/// One field: its name token (a record field's) and its position in its variant, and the
/// tokens of its type.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Field {
    pub(crate) name: Option<usize>,
    pub(crate) index: usize,
    pub(crate) ty: Range<usize>,
    /// The field declares a default value (`a: u8 = 1`, RFC 3681): `derive(Default)` takes it.
    pub(crate) defaulted: bool,
}

/// A struct's fields or an enum's variant: `fields` indexes [`Adt::fields`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Variant {
    /// The variant's name token; the type's for a struct.
    pub(crate) name: usize,
    pub(crate) kind: FieldsKind,
    pub(crate) fields: Range<usize>,
}

impl Variant {
    pub(crate) fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// What a generic parameter is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum ParamKind {
    Lifetime,
    /// A type parameter and its bounds, if it writes any.
    Type {
        bounds: Option<Range<usize>>,
    },
    /// A const parameter and its type.
    Const {
        ty: Range<usize>,
    },
}

/// One generic parameter: its declaration without attributes (a lifetime's with its bounds),
/// its name (a lifetime's `'` and name, two tokens) and its kind.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Param {
    pub(crate) decl: Range<usize>,
    pub(crate) name: Range<usize>,
    pub(crate) kind: ParamKind,
}

/// The item a derive is of, by token indices of its stream.
pub(crate) struct Adt<'a> {
    pub(crate) name: usize,
    pub(crate) kind: AdtKind,
    /// One for a struct, none for a union.
    pub(crate) variants: AVec<'a, Variant>,
    /// The fields of every variant, a variant's together; a union's.
    pub(crate) fields: AVec<'a, Field>,
    pub(crate) params: AVec<'a, Param>,
    /// Each `where` predicate, without its comma.
    pub(crate) predicates: AVec<'a, Range<usize>>,
    /// Each `T::Assoc` path type of a field, `T` a type or const parameter: what rustc's
    /// derives bound by the trait too (`find_type_parameters`), in the order written.
    pub(crate) assoc: AVec<'a, Range<usize>>,
    /// `#[repr(packed)]` (`packed(N)` too) or `#[blaze::bits]`: the fields may have no
    /// address of their own, so they are copied out to be used.
    pub(crate) packed: bool,
}

impl Adt<'_> {
    pub(crate) fn is_enum(&self) -> bool {
        matches!(self.kind, AdtKind::Enum { .. })
    }

    pub(crate) fn packed_struct(&self) -> bool {
        self.packed && self.kind == AdtKind::Struct
    }

    pub(crate) fn is_empty_enum(&self) -> bool {
        self.is_enum() && self.variants.is_empty()
    }
}

/// Why the item is not one a derive can be of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NotAnAdt {
    /// The item is not a struct, an enum or a union.
    WrongKind,
    /// The item has no name after its keyword.
    NoName,
    /// The item's tokens are not those of a struct, an enum or a union.
    Malformed,
}

impl NotAnAdt {
    pub(crate) fn message(self) -> &'static str {
        match self {
            NotAnAdt::WrongKind => "expected struct, enum or union",
            NotAnAdt::NoName => "missing name",
            NotAnAdt::Malformed => "the item does not read as a struct, an enum or a union",
        }
    }
}

/// Whether a lone `=` ends the counting of `<` and `>` in a piece: in an expression (a
/// discriminant, a field's default) they are operators.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Angles {
    Always,
    UntilEq,
}

/// Reads the derive's item, `stream`, in `alloc`.
pub(crate) fn read<'a>(
    stream: &TokenStream<'_>,
    alloc: Alloc<'a>,
) -> Result<Adt<'a>, NotAnAdt> {
    let r = Reader { stream, tokens: stream.tokens() };
    let end = r.tokens.len();
    let mut packed = false;
    let mut at = r.skip_attrs(0, end, |inner| packed |= r.is_packed(inner))?;
    at = r.skip_vis(at)?;
    let kind = if r.word(at, "struct") {
        AdtKind::Struct
    } else if r.word(at, "enum") {
        AdtKind::Enum { default_variant: None }
    } else if r.word(at, "union") {
        AdtKind::Union
    } else {
        return Err(NotAnAdt::WrongKind);
    };
    let name = at + 1;
    if !r.ident(name) {
        return Err(NotAnAdt::NoName);
    }
    let mut adt = Adt {
        name,
        kind,
        variants: Vec::new_in(alloc),
        fields: Vec::new_in(alloc),
        params: Vec::new_in(alloc),
        predicates: Vec::new_in(alloc),
        assoc: Vec::new_in(alloc),
        packed,
    };
    at = name + 1;
    if r.punct(at, '<') {
        let close = r.generics_end(at)?;
        r.split(at + 1..close, Angles::Always, |piece| r.param(piece, &mut adt.params))?;
        at = close + 1;
    }
    if r.word(at, "where") {
        at = r.where_clause(at, &mut adt.predicates)?;
    }
    match kind {
        AdtKind::Struct => {
            let fields = if r.open(at, Delim::Brace) {
                let past = r.past_group(at)?;
                r.fields(at + 1..past - 1, FieldsKind::Record, &mut adt.fields)?;
                FieldsKind::Record
            } else if r.open(at, Delim::Paren) {
                let past = r.past_group(at)?;
                r.fields(at + 1..past - 1, FieldsKind::Tuple, &mut adt.fields)?;
                if r.word(past, "where") {
                    r.where_clause(past, &mut adt.predicates)?;
                }
                FieldsKind::Tuple
            } else if r.punct(at, ';') {
                FieldsKind::Unit
            } else {
                return Err(NotAnAdt::Malformed);
            };
            adt.variants.push(Variant { name, kind: fields, fields: 0..adt.fields.len() });
        }
        AdtKind::Enum { .. } => {
            if !r.open(at, Delim::Brace) {
                return Err(NotAnAdt::Malformed);
            }
            let past = r.past_group(at)?;
            r.variants(at + 1..past - 1, &mut adt)?;
        }
        AdtKind::Union => {
            if !r.open(at, Delim::Brace) {
                return Err(NotAnAdt::Malformed);
            }
            let past = r.past_group(at)?;
            r.fields(at + 1..past - 1, FieldsKind::Record, &mut adt.fields)?;
        }
    }
    r.assoc_types(&mut adt)?;
    Ok(adt)
}

struct Reader<'s> {
    stream: &'s TokenStream<'s>,
    tokens: &'s [Token],
}

impl<'s> Reader<'s> {
    fn kind(&self, at: usize) -> Option<TokenKind> {
        self.tokens.get(at).map(|it| it.kind)
    }

    fn text(&self, at: usize) -> &'s str {
        self.stream.text_at(at)
    }

    fn punct(&self, at: usize, want: char) -> bool {
        matches!(self.kind(at), Some(TokenKind::Punct { char, .. }) if char == want)
    }

    fn joint_punct(&self, at: usize, want: char) -> bool {
        matches!(
            self.kind(at),
            Some(TokenKind::Punct { char, spacing: Spacing::Joint }) if char == want
        )
    }

    /// `::` at `at`.
    fn path_sep(&self, at: usize) -> bool {
        self.joint_punct(at, ':') && self.punct(at + 1, ':')
    }

    fn ident(&self, at: usize) -> bool {
        matches!(self.kind(at), Some(TokenKind::Ident { .. }))
    }

    /// The keyword or plain name `word` at `at` (not `r#word`).
    fn word(&self, at: usize, word: &str) -> bool {
        matches!(self.kind(at), Some(TokenKind::Ident { raw: Raw::No, .. }))
            && self.text(at) == word
    }

    fn open(&self, at: usize, delim: Delim) -> bool {
        self.kind(at) == Some(TokenKind::Open(delim))
    }

    /// The index past the group opened at `at`.
    fn past_group(&self, at: usize) -> Result<usize, NotAnAdt> {
        self.stream.close_of(at).map(|close| close + 1).ok_or(NotAnAdt::Malformed)
    }

    /// The index past the token tree at `at`: a group whole, else the token.
    fn past(&self, at: usize) -> Result<usize, NotAnAdt> {
        match self.kind(at) {
            Some(TokenKind::Open(_)) => self.past_group(at),
            Some(TokenKind::Close(_)) | None => Err(NotAnAdt::Malformed),
            Some(_) => Ok(at + 1),
        }
    }

    /// Whether the `>` at `at` ends a `->`.
    fn arrow_end(&self, at: usize) -> bool {
        at > 0 && self.joint_punct(at - 1, '-')
    }

    /// Whether the `=` at `at` stands alone: not of `==`, `=>`, `<=`, `!=`, `+=` and the like.
    fn lone_eq(&self, at: usize) -> bool {
        let glued_after =
            self.joint_punct(at, '=') && (self.punct(at + 1, '=') || self.punct(at + 1, '>'));
        let glued_before = at > 0
            && matches!(
                self.kind(at - 1),
                Some(TokenKind::Punct { char, spacing: Spacing::Joint }) if "<>!=+-*/%^&|".contains(char)
            );
        !glued_after && !glued_before
    }

    /// The outer attributes from `at`, each one's inside handed to `on`; the index past them.
    fn skip_attrs(
        &self,
        mut at: usize,
        end: usize,
        mut on: impl FnMut(Range<usize>),
    ) -> Result<usize, NotAnAdt> {
        while at < end && self.punct(at, '#') && self.open(at + 1, Delim::Bracket) {
            let past = self.past_group(at + 1)?;
            on(at + 2..past - 1);
            at = past;
        }
        Ok(at)
    }

    /// The index past a visibility at `at`: `pub`, and `(crate)`, `(self)`, `(super)` or
    /// `(in path)` after it (a tuple field's `pub (A, B)` is a type after `pub`).
    fn skip_vis(&self, at: usize) -> Result<usize, NotAnAdt> {
        if !self.word(at, "pub") {
            return Ok(at);
        }
        let restricted = self.open(at + 1, Delim::Paren)
            && ["crate", "self", "super", "in"].iter().any(|it| self.word(at + 2, it));
        if restricted { self.past_group(at + 1) } else { Ok(at + 1) }
    }

    /// The index of the `>` closing the `<` at `open`.
    fn generics_end(&self, open: usize) -> Result<usize, NotAnAdt> {
        let mut depth = 1u32;
        let mut at = open + 1;
        while at < self.tokens.len() {
            if self.punct(at, '<') {
                depth += 1;
            } else if self.punct(at, '>') && !self.arrow_end(at) {
                depth -= 1;
                if depth == 0 {
                    return Ok(at);
                }
            }
            at = self.past(at)?;
        }
        Err(NotAnAdt::Malformed)
    }

    /// The first lone `=` of `range` outside every group and `<..>`, or its end.
    fn top_eq(&self, range: Range<usize>) -> Result<usize, NotAnAdt> {
        let mut depth = 0u32;
        let mut at = range.start;
        while at < range.end {
            if self.punct(at, '<') {
                depth += 1;
            } else if self.punct(at, '>') && !self.arrow_end(at) {
                depth = depth.checked_sub(1).ok_or(NotAnAdt::Malformed)?;
            } else if depth == 0 && self.punct(at, '=') && self.lone_eq(at) {
                return Ok(at);
            }
            at = self.past(at)?;
        }
        Ok(range.end)
    }

    /// Each comma-separated piece of `range`, to `piece`; a trailing comma's empty piece is not
    /// handed on.
    fn split(
        &self,
        range: Range<usize>,
        angles: Angles,
        mut piece: impl FnMut(Range<usize>) -> Result<(), NotAnAdt>,
    ) -> Result<(), NotAnAdt> {
        let (mut start, mut at, mut depth, mut counting) = (range.start, range.start, 0u32, true);
        while at < range.end {
            if depth == 0 && self.punct(at, ',') {
                piece(start..at)?;
                (start, counting) = (at + 1, true);
            } else if counting && self.punct(at, '<') {
                depth += 1;
            } else if counting && self.punct(at, '>') && !self.arrow_end(at) {
                depth = depth.checked_sub(1).ok_or(NotAnAdt::Malformed)?;
            } else if angles == Angles::UntilEq
                && depth == 0
                && self.punct(at, '=')
                && self.lone_eq(at)
            {
                counting = false;
            }
            at = self.past(at)?;
        }
        if start < range.end {
            piece(start..range.end)?;
        }
        Ok(())
    }

    /// One generic parameter.
    fn param(
        &self,
        piece: Range<usize>,
        params: &mut AVec<'_, Param>,
    ) -> Result<(), NotAnAdt> {
        let at = self.skip_attrs(piece.start, piece.end, |_| {})?;
        if at >= piece.end {
            return Err(NotAnAdt::Malformed);
        }
        let param = if self.punct(at, '\'') {
            Param { decl: at..piece.end, name: at..at + 2, kind: ParamKind::Lifetime }
        } else if self.word(at, "const") {
            let name = at + 1;
            if !self.ident(name) || !self.punct(name + 1, ':') {
                return Err(NotAnAdt::Malformed);
            }
            let ty = name + 2..self.top_eq(name + 2..piece.end)?;
            Param { decl: at..piece.end, name: name..name + 1, kind: ParamKind::Const { ty } }
        } else {
            if !self.ident(at) {
                return Err(NotAnAdt::Malformed);
            }
            let bounds = if self.punct(at + 1, ':') {
                let end = self.top_eq(at + 2..piece.end)?;
                (end > at + 2).then_some(at + 2..end)
            } else {
                None
            };
            Param { decl: at..piece.end, name: at..at + 1, kind: ParamKind::Type { bounds } }
        };
        params.push(param);
        Ok(())
    }

    /// The `where` clause at `at`: its predicates, and the index of what ends it (a body's
    /// `{`, a `;`, or the item's end).
    fn where_clause(
        &self,
        at: usize,
        predicates: &mut AVec<'_, Range<usize>>,
    ) -> Result<usize, NotAnAdt> {
        let (mut stop, mut depth) = (at + 1, 0u32);
        while stop < self.tokens.len() {
            if depth == 0 && (self.open(stop, Delim::Brace) || self.punct(stop, ';')) {
                break;
            }
            if self.punct(stop, '<') {
                depth += 1;
            } else if self.punct(stop, '>') && !self.arrow_end(stop) {
                depth = depth.checked_sub(1).ok_or(NotAnAdt::Malformed)?;
            }
            stop = self.past(stop)?;
        }
        self.split(at + 1..stop, Angles::Always, |piece| {
            predicates.push(piece);
            Ok(())
        })?;
        Ok(stop)
    }

    /// The fields inside a record's braces or a tuple's parentheses.
    fn fields(
        &self,
        range: Range<usize>,
        kind: FieldsKind,
        fields: &mut AVec<'_, Field>,
    ) -> Result<(), NotAnAdt> {
        let first = fields.len();
        self.split(range, Angles::UntilEq, |piece| {
            let mut at = self.skip_attrs(piece.start, piece.end, |_| {})?;
            at = self.skip_vis(at)?;
            if self.word(at, "unsafe") {
                at += 1;
            }
            let (name, ty) = match kind {
                FieldsKind::Record => {
                    if !self.ident(at) || !self.punct(at + 1, ':') {
                        return Err(NotAnAdt::Malformed);
                    }
                    (Some(at), at + 2)
                }
                FieldsKind::Tuple | FieldsKind::Unit => (None, at),
            };
            if ty >= piece.end {
                return Err(NotAnAdt::Malformed);
            }
            let ty = ty..self.top_eq(ty..piece.end)?;
            // Past the type, a lone `=` and the default's expression (RFC 3681).
            let defaulted = ty.end < piece.end;
            fields.push(Field { name, index: fields.len() - first, ty, defaulted });
            Ok(())
        })
    }

    /// An enum's variants, inside its braces.
    fn variants(
        &self,
        range: Range<usize>,
        adt: &mut Adt<'_>,
    ) -> Result<(), NotAnAdt> {
        self.split(range, Angles::UntilEq, |piece| {
            let mut default = false;
            let mut at = self.skip_attrs(piece.start, piece.end, |inner| {
                default |= inner.len() == 1 && self.word(inner.start, "default");
            })?;
            at = self.skip_vis(at)?;
            if !self.ident(at) {
                return Err(NotAnAdt::Malformed);
            }
            let start = adt.fields.len();
            let kind = if at + 1 < piece.end && self.open(at + 1, Delim::Brace) {
                let past = self.past_group(at + 1)?;
                self.fields(at + 2..past - 1, FieldsKind::Record, &mut adt.fields)?;
                FieldsKind::Record
            } else if at + 1 < piece.end && self.open(at + 1, Delim::Paren) {
                let past = self.past_group(at + 1)?;
                self.fields(at + 2..past - 1, FieldsKind::Tuple, &mut adt.fields)?;
                FieldsKind::Tuple
            } else {
                FieldsKind::Unit
            };
            if default
                && let AdtKind::Enum { default_variant } = &mut adt.kind
                && default_variant.is_none()
            {
                *default_variant = Some(adt.variants.len());
            }
            adt.variants.push(Variant { name: at, kind, fields: start..adt.fields.len() });
            Ok(())
        })
    }

    /// `repr(.. packed ..)` or `blaze::bits`, an attribute's inside.
    fn is_packed(&self, inner: Range<usize>) -> bool {
        let at = inner.start;
        if self.word(at, "repr") && self.open(at + 1, Delim::Paren) {
            return (at + 2..inner.end).any(|it| self.word(it, "packed"));
        }
        self.word(at, "blaze") && self.path_sep(at + 1) && self.word(at + 3, "bits")
    }

    /// Each `T::Assoc` path type of the fields' types, `T` a parameter: `T` not after `::`,
    /// `as` or `'`, one segment after it with its generic arguments, and no `::` after that.
    fn assoc_types(&self, adt: &mut Adt<'_>) -> Result<(), NotAnAdt> {
        if adt.params.iter().all(|it| it.kind == ParamKind::Lifetime) {
            return Ok(());
        }
        for field in 0..adt.fields.len() {
            let ty = adt.fields[field].ty.clone();
            let mut at = ty.start;
            while at < ty.end {
                match self.assoc_at(at, ty.end, &adt.params)? {
                    Some(past) => {
                        adt.assoc.push(at..past);
                        at = past;
                    }
                    None => at += 1,
                }
            }
        }
        Ok(())
    }

    fn assoc_at(&self, at: usize, end: usize, params: &[Param]) -> Result<Option<usize>, NotAnAdt> {
        let is_param = self.ident(at)
            && params.iter().any(|it| {
                it.kind != ParamKind::Lifetime && self.text(it.name.start) == self.text(at)
            });
        let after_path = at >= 2 && self.path_sep(at - 2);
        let after = at >= 1 && (self.word(at - 1, "as") || self.punct(at - 1, '\''));
        if !is_param || after_path || after || !self.path_sep(at + 1) || !self.ident(at + 3) {
            return Ok(None);
        }
        let mut past = at + 4;
        if self.punct(past, '<') {
            past = self.generics_end(past)? + 1;
        }
        if past > end || self.path_sep(past) {
            return Ok(None);
        }
        Ok(Some(past))
    }
}
