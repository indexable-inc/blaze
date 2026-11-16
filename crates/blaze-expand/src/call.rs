//! One macro call: its tokens, its kind, its call-site hygiene and, for an eager macro
//! re-entered, the answers to the calls in its input.

use crate::{
    error::{ExpandError, Stream},
    tokens::{Ctx, Edition, Sink, TokenStream},
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

    /// Refuses a call whose streams do not pair their delimiters or carry an expansion's own
    /// marks: either would be read wrong. The proof is what code that pairs delimiters takes.
    pub(crate) fn check(&self) -> Result<Checked, ExpandError> {
        for (stream, tokens) in self.streams() {
            if let Some(token) = tokens.unbalanced() {
                return Err(ExpandError::Unbalanced { stream, token });
            }
            if tokens.has_own_marks() {
                return Err(ExpandError::Unsealed { stream });
            }
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
