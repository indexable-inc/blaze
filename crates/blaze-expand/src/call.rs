//! One macro call: its tokens, its kind, its call-site hygiene and, for an eager macro
//! re-entered, the answers to the calls in its input.

use std::alloc::Allocator;

use crate::{
    error::{ExpandError, Stream},
    tokens::{Ctx, Edition, Sink, TokenStream},
};

/// The three kinds of macro call.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CallShape {
    FnLike,
    Attr,
    Derive,
}

/// A call's tokens by its kind.
pub enum CallKind<'a, A: Allocator> {
    /// `m!(args)`: the tokens inside the delimiters.
    FnLike { args: &'a TokenStream<A> },
    /// `#[m]` (`args` `None`) or `#[m(args)]` on `item`: the item without this attribute.
    Attr { args: Option<&'a TokenStream<A>>, item: &'a TokenStream<A> },
    /// `#[derive(m)]` on `item`.
    Derive { item: &'a TokenStream<A> },
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
pub struct Eager<'a, A: Allocator> {
    pub inside: Inside,
    /// For an eager macro re-entered after [`crate::Expanded::NeedsExpanded`]: the tokens of
    /// each nested call it listed, in its order, each sealed ([`TokenStream::seal`]).
    pub answers: &'a [&'a TokenStream<A>],
}

impl<A: Allocator> Eager<'_, A> {
    /// A call where it is written, with no answers.
    pub fn plain() -> Self {
        Eager { inside: Inside::Plain, answers: &[] }
    }
}

/// One macro call.
pub struct Call<'a, A: Allocator> {
    pub kind: CallKind<'a, A>,
    pub site: CallSite,
    pub eager: Eager<'a, A>,
}

impl<'a, A: Allocator> Call<'a, A> {
    /// `m!(args)` where it is written.
    pub fn fn_like(args: &'a TokenStream<A>, site: CallSite) -> Self {
        Call { kind: CallKind::FnLike { args }, site, eager: Eager::plain() }
    }

    pub fn shape(&self) -> CallShape {
        match self.kind {
            CallKind::FnLike { .. } => CallShape::FnLike,
            CallKind::Attr { .. } => CallShape::Attr,
            CallKind::Derive { .. } => CallShape::Derive,
        }
    }

    /// The tokens the macro is given: the fn-like call's arguments, or the item.
    pub fn input(&self) -> &'a TokenStream<A> {
        match self.kind {
            CallKind::FnLike { args } => args,
            CallKind::Attr { item, .. } | CallKind::Derive { item } => item,
        }
    }

    /// An attribute's arguments.
    pub fn attr_args(&self) -> Option<&'a TokenStream<A>> {
        match self.kind {
            CallKind::Attr { args, .. } => args,
            CallKind::FnLike { .. } | CallKind::Derive { .. } => None,
        }
    }

    /// Every stream of the call, with its name.
    pub(crate) fn streams(&self) -> impl Iterator<Item = (Stream, &'a TokenStream<A>)> + '_ {
        let answers = self.eager.answers.iter().enumerate().map(|(at, answer)| {
            (Stream::Answer(crate::tokens::index(at)), *answer)
        });
        core::iter::once((Stream::Input, self.input()))
            .chain(self.attr_args().map(|args| (Stream::AttrArgs, args)))
            .chain(answers)
    }

    /// Refuses a call whose streams do not pair their delimiters or carry an expansion's own
    /// marks: either would be read wrong.
    pub(crate) fn check(&self) -> Result<(), ExpandError> {
        for (stream, tokens) in self.streams() {
            if let Some(token) = tokens.unbalanced() {
                return Err(ExpandError::Unbalanced { stream, token });
            }
            if tokens.has_own_marks() {
                return Err(ExpandError::Unsealed { stream });
            }
        }
        Ok(())
    }

    /// The call's canonical bytes: its kind, call site, eager facts and every stream.
    pub(crate) fn encode<S: Sink>(&self, out: &mut S) {
        let shape = match self.shape() {
            CallShape::FnLike => 0,
            CallShape::Attr => 1,
            CallShape::Derive => 2,
        };
        let inside = match self.eager.inside {
            Inside::Plain => 0,
            Inside::EagerInput => 1,
        };
        let CallSite { edition, panic_edition, ctx } = self.site;
        out.put(&[shape, edition.byte(), panic_edition.byte(), inside]);
        out.put(&ctx.raw().to_be_bytes());
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
