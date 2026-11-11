//! Facts: what an analysis says of one item, for lints and other analyses to ask for, in the
//! style of Go's `go/analysis` (Requires and Facts) and of blaze's own effect summaries.
//!
//! An extension declares a fact with a type ([`Fact`]) and computes it by a function of one
//! body ([`Analysis::run`]) that may ask for the facts it requires, and for its own, of the
//! items the body calls ([`crate::Cx::fact`]). The host does the rest: it orders the items so
//! callees come first, runs a cycle of calls to its fixpoint from [`Fact::bottom`], and keeps
//! each value under the item's code identity, the extension's and the values it asked for, so
//! an analysis is incremental with no work by its author.

use crate::{Body, Bytes, Cx};

/// The type of a fact.
///
/// A fact is kept as bytes: the same bytes for the same value, on every run and machine (no
/// address, no order of a hash table), since they are compared to end a fixpoint and hashed
/// into the keys of what read them.
pub trait Fact: Sized {
    /// The fact's name within its extension.
    const NAME: &'static str;

    /// The least value: what an item in a cycle of calls has before it is first computed.
    /// [`Analysis::run`] must be monotone from it, or the cycle's fixpoint is refused at its
    /// bound.
    fn bottom() -> Self;

    fn encode(&self, out: &mut Bytes<'_>);

    /// The value `bytes` are, or `None` for bytes [`Fact::encode`] never writes.
    fn decode(bytes: &[u8]) -> Option<Self>;
}

/// How a fact is computed.
pub trait Analysis {
    type Fact: Fact;

    /// The other facts [`Analysis::run`] asks for, by name. Its own fact, of the body's
    /// callees, needs no entry.
    const REQUIRES: &'static [&'static str];

    /// The fact of `body`'s item.
    fn run(cx: &mut Cx<'_>, body: &Body<'_, '_>) -> Self::Fact;
}

/// A fact's function, its type erased: writes the fact of the body into `out`.
pub type FactFn = for<'a, 'b, 'db> fn(&mut Cx<'a>, &Body<'b, 'db>, &mut Bytes<'a>);

/// One fact of an extension, as its [`crate::Extension`] lists it: `FactDecl::of::<A>()`.
#[derive(Clone, Copy)]
pub struct FactDecl {
    pub name: &'static str,
    pub requires: &'static [&'static str],
    /// Writes [`Fact::bottom`].
    pub bottom: for<'a> fn(&mut Bytes<'a>),
    pub run: FactFn,
}

impl FactDecl {
    /// The declaration of the fact `A` computes.
    pub const fn of<A: Analysis>() -> FactDecl {
        FactDecl {
            name: <A::Fact as Fact>::NAME,
            requires: A::REQUIRES,
            bottom: bottom::<A>,
            run: run::<A>,
        }
    }
}

fn bottom<A: Analysis>(out: &mut Bytes<'_>) {
    <A::Fact as Fact>::bottom().encode(out);
}

fn run<'a, A: Analysis>(cx: &mut Cx<'a>, body: &Body<'_, '_>, out: &mut Bytes<'a>) {
    A::run(cx, body).encode(out);
}
