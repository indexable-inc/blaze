//! One macro call: its tokens, its kind, its call-site hygiene and, for an eager macro
//! re-entered, the answers to the calls in its input.

use crate::{
    error::{ExpandError, Stream, StreamError},
    origin_column::TextMark,
    tokens::{Ctx, CtxNode, Delim, Edition, Sink, TokenKind, TokenStream},
};

/// The three kinds of macro call: the schema's one `CallShape`.
pub use blaze_surface::CallShape;

/// A call's tokens by its kind. A stream borrowed for `'a` is in an allocator that lives at
/// least that long (a stream is covariant in its allocator's lifetime).
pub enum CallKind<'a> {
    /// `m!(args)`: the tokens inside the delimiters.
    FnLike { args: &'a TokenStream<'a> },
    /// `#[m]` (`args` `None`) or `#[m(args)]` on `item`: the item without this attribute.
    Attr { args: Option<&'a TokenStream<'a>>, item: &'a TokenStream<'a> },
    /// `#[derive(m)]` or `#[derive_const(m)]` on `item`. `item` is the item as the derive sees
    /// it: `#[cfg]` and `#[cfg_attr]` applied, the `#[derive]` attribute that names this macro
    /// removed, and every other attribute kept (helper attributes included), in source order.
    /// The caller builds it.
    Derive { item: &'a TokenStream<'a>, facts: DeriveFacts },
}

/// What a derive's output depends on besides the item: they are in the call's key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DeriveFacts {
    /// Whether the item's derives include the builtin `Copy` (rustc's `has_derive_copy`):
    /// `Clone` of a type with no type parameter is then `*self`.
    pub copy: AlsoCopy,
    /// `#[derive_const]`: the impl is `impl const`, its parameters bounded `[const]`.
    pub constness: Constness,
}

impl DeriveFacts {
    /// A plain `#[derive]` of an item that does not also derive `Copy`.
    pub const PLAIN: DeriveFacts =
        DeriveFacts { copy: AlsoCopy::No, constness: Constness::NotConst };
}

/// Whether the item also derives the builtin `Copy`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AlsoCopy {
    No,
    Yes,
}

/// `#[derive]` or `#[derive_const]`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Constness {
    NotConst,
    Const,
}

/// The hygiene of the call site: what tokens the expansion makes resolve against.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CallSite {
    /// The edition of the call's path.
    pub edition: Edition,
    /// The edition `panic!` and `unreachable!` pick their 2015 or 2021 form by: the call's,
    /// or for a call a macro made under `#[allow_internal_unstable(edition_panic)]`, the
    /// edition of the first call outside such macros (rustc's `use_panic_2021`).
    pub panic_edition: Edition,
    /// The context of the call's path, a context of the call's input stream
    /// ([`Call::input`]): the parent of the call-site and mixed-site tokens the expansion
    /// makes.
    pub ctx: Ctx,
}

impl CallSite {
    /// A call site with no enclosing expansion context, in `edition`.
    pub const fn root(edition: Edition) -> CallSite {
        CallSite { edition, panic_edition: edition, ctx: Ctx::root(edition) }
    }

    /// The call site of the path in `range` of `from`, imported into `input` (so `ctx` is a
    /// context of `input`, as [`CallSite::ctx`] requires). `edition` and `panic_edition` come
    /// from the path token (`from.edition_of`), falling back to `fallback_edition` when `range`
    /// holds no visible token.
    pub fn from_path(
        from: &TokenStream<'_>,
        range: core::ops::Range<usize>,
        input: &mut TokenStream<'_>,
        fallback_edition: Edition,
    ) -> Result<CallSite, StreamError> {
        let Some(path_ctx) = path_ctx_in(from, range) else {
            return Ok(CallSite::root(fallback_edition));
        };
        let edition = from.edition_of(path_ctx);
        let ctx = input.import_ctx(from, path_ctx)?;
        Ok(CallSite { edition, panic_edition: edition, ctx })
    }

    /// The call site of a function-like macro call whose opening delimiter is at `open` of
    /// `from` (`path!(...)`, walking backward from the `!` before `open` across `::` and
    /// invisible groups), imported into `input`.
    pub fn before_open(
        from: &TokenStream<'_>,
        open: usize,
        input: &mut TokenStream<'_>,
        fallback_edition: Edition,
    ) -> Result<CallSite, StreamError> {
        let Some(path_ctx) = path_ctx_before_open(from, open) else {
            return Ok(CallSite::root(fallback_edition));
        };
        let edition = from.edition_of(path_ctx);
        let ctx = input.import_ctx(from, path_ctx)?;
        Ok(CallSite { edition, panic_edition: edition, ctx })
    }

    /// The call site of a token carrying a rendered [`TextMark`] (e.g. a call written in an
    /// expansion's text), imported into `input`.
    pub fn from_text_mark(
        mark: Option<TextMark>,
        input: &mut TokenStream<'_>,
        fallback_edition: Edition,
    ) -> Result<CallSite, StreamError> {
        let Some(mark) = mark else {
            return Ok(CallSite::root(fallback_edition));
        };
        let ctx = input.context(CtxNode {
            parent: Ctx::root(fallback_edition),
            mark: mark.as_outer_mark(),
        })?;
        Ok(CallSite { edition: mark.edition, panic_edition: mark.edition, ctx })
    }
}

/// The context of the first identifier (or else first visible token) of the path in `range` of
/// `stream`.
pub(crate) fn path_ctx_in(stream: &TokenStream<'_>, range: core::ops::Range<usize>) -> Option<Ctx> {
    let slice = stream.tokens().get(range)?;
    if let Some(ident) = slice.iter().find(|t| matches!(t.kind, TokenKind::Ident { .. })) {
        return Some(ident.ctx);
    }
    slice
        .iter()
        .find(|t| !matches!(t.kind, TokenKind::Open(Delim::Invisible) | TokenKind::Close(Delim::Invisible)))
        .map(|t| t.ctx)
}

/// The context of the first identifier of the macro path before the opening delimiter at `open`
/// in `stream` (`path!(...)`, skipping invisible groups around `$p:path` fragments).
fn path_ctx_before_open(stream: &TokenStream<'_>, open: usize) -> Option<Ctx> {
    let tokens = stream.tokens();
    let invisible = |i: usize| {
        matches!(tokens.get(i).map(|t| t.kind), Some(TokenKind::Open(Delim::Invisible) | TokenKind::Close(Delim::Invisible)))
    };
    let prev = |mut i: usize| -> Option<usize> {
        while i > 0 {
            i -= 1;
            if !invisible(i) {
                return Some(i);
            }
        }
        None
    };
    let colon = |i: usize| matches!(tokens.get(i).map(|t| t.kind), Some(TokenKind::Punct { char: ':', .. }));
    let bang = prev(open.min(tokens.len()))?;
    if !matches!(tokens.get(bang).map(|t| t.kind), Some(TokenKind::Punct { char: '!', .. })) {
        return None;
    }
    let mut at = bang;
    let mut first = None;
    while let Some(word) = prev(at) {
        if !matches!(tokens.get(word).map(|t| t.kind), Some(TokenKind::Ident { .. })) {
            break;
        }
        first = Some(word);
        match prev(word).and_then(|second| Some((second, prev(second)?))) {
            Some((second, colon_one)) if colon(second) && colon(colon_one) => at = colon_one,
            _ => break,
        }
    }
    first.and_then(|i| tokens.get(i)).map(|t| t.ctx)
}

/// Whether the call is itself inside an eager macro's input.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Inside {
    /// Expanded where it is written: `line!()` and its kin give a [`crate::Hole`].
    Plain,
    /// Expanded for an eager macro's input (`concat!(line!())`): positions are read from the
    /// context and recorded.
    EagerInput,
}

/// The eager facts of a call.
pub struct Eager<'a> {
    pub inside: Inside,
    /// For an eager macro re-entered after [`crate::Expanded::NeedsExpanded`]: the tokens of
    /// each nested call it listed, in its order, each sealed ([`TokenStream::seal`]).
    pub answers: &'a [&'a TokenStream<'a>],
}

impl Eager<'_> {
    /// A call where it is written, with no answers.
    pub fn plain() -> Self {
        Eager { inside: Inside::Plain, answers: &[] }
    }
}

/// One macro call.
pub struct Call<'a> {
    pub kind: CallKind<'a>,
    pub site: CallSite,
    pub eager: Eager<'a>,
}

impl<'a> Call<'a> {
    /// `m!(args)` where it is written.
    pub fn fn_like(args: &'a TokenStream<'a>, site: CallSite) -> Self {
        Call { kind: CallKind::FnLike { args }, site, eager: Eager::plain() }
    }

    /// `#[m]` or `#[m(args)]` on `item` (the item without the attribute), as
    /// [`crate::ItemAttrs::attr_call`] gives them.
    pub fn attr(args: Option<&'a TokenStream<'a>>, item: &'a TokenStream<'a>, site: CallSite) -> Self {
        Call { kind: CallKind::Attr { args, item }, site, eager: Eager::plain() }
    }

    /// `#[derive(m)]` on `item` (the item as a derive sees it), as [`crate::Derives`] gives it.
    pub fn derive(item: &'a TokenStream<'a>, facts: DeriveFacts, site: CallSite) -> Self {
        Call { kind: CallKind::Derive { item, facts }, site, eager: Eager::plain() }
    }

    pub fn shape(&self) -> CallShape {
        match self.kind {
            CallKind::FnLike { .. } => CallShape::FnLike,
            CallKind::Attr { .. } => CallShape::Attr,
            CallKind::Derive { .. } => CallShape::Derive,
        }
    }

    /// The tokens the macro is given: the fn-like call's arguments, or the item.
    pub fn input(&self) -> &'a TokenStream<'a> {
        match self.kind {
            CallKind::FnLike { args } => args,
            CallKind::Attr { item, .. } | CallKind::Derive { item, .. } => item,
        }
    }

    /// An attribute's arguments.
    pub fn attr_args(&self) -> Option<&'a TokenStream<'a>> {
        match self.kind {
            CallKind::Attr { args, .. } => args,
            CallKind::FnLike { .. } | CallKind::Derive { .. } => None,
        }
    }

    /// Every stream of the call, with its name.
    pub(crate) fn streams(&self) -> impl Iterator<Item = (Stream, &'a TokenStream<'a>)> + '_ {
        let answers = self
            .eager
            .answers
            .iter()
            .enumerate()
            .map(|(at, answer)| (Stream::Answer(crate::tokens::index(at)), *answer));
        core::iter::once((Stream::Input, self.input()))
            .chain(self.attr_args().map(|args| (Stream::AttrArgs, args)))
            .chain(answers)
    }

    /// Refuses a call whose streams do not pair their delimiters, carry an expansion's own
    /// marks, or name a call-site context that is not of [`Call::input`]: any of these would be
    /// read wrong. The proof is what code that pairs delimiters takes.
    pub(crate) fn check(&self) -> Result<Checked, ExpandError> {
        for (stream, tokens) in self.streams() {
            if let Some(token) = tokens.unbalanced() {
                return Err(ExpandError::Unbalanced { stream, token });
            }
            if tokens.has_own_marks() {
                return Err(ExpandError::Unsealed { stream });
            }
        }
        if let Some(at) = self.site.ctx.node()
            && usize::try_from(at).is_ok_and(|at| at >= self.input().contexts().len())
        {
            return Err(ExpandError::Stream {
                stream: Some(Stream::Input),
                error: StreamError::NoSuchContext { ctx: self.site.ctx.raw() },
            });
        }
        Ok(Checked(()))
    }
}

/// The proof that a call's streams pair their delimiters and carry no mark of their own
/// expansion ([`Call::check`]), made once a call.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Checked(());

impl Call<'_> {

    /// The call's canonical bytes: its kind, call site, eager facts and every stream.
    pub(crate) fn encode<S: Sink>(&self, out: &mut S) {
        let shape = self.shape().byte();
        let inside = match self.eager.inside {
            Inside::Plain => 0,
            Inside::EagerInput => 1,
        };
        let CallSite { edition, panic_edition, ctx } = self.site;
        out.put(&[shape, edition.byte(), panic_edition.byte(), inside]);
        out.put(&ctx.raw().to_be_bytes());
        match self.kind {
            CallKind::Derive { facts, .. } => {
                let copy = u8::from(facts.copy == AlsoCopy::Yes);
                let constness = u8::from(facts.constness == Constness::Const);
                out.put(&[copy, constness]);
            }
            CallKind::FnLike { .. } | CallKind::Attr { .. } => {}
        }
        self.input().encode(out);
        match self.attr_args() {
            None => out.put(&[0]),
            Some(args) => {
                out.put(&[1]);
                args.encode(out);
            }
        }
        out.put(&crate::tokens::index(self.eager.answers.len()).to_be_bytes());
        for answer in self.eager.answers {
            answer.encode(out);
        }
    }
}
