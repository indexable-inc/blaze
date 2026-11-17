//! How a reader of `ModuleScope` records finds a name (docs/v2/plan/design/bottom.md, "Globs
//! are edges, not copies"): who is asking ([`Asker`]) and what each visibility lets it name
//! ([`SeenFrom`]), and a name through glob edges ([`through`]). The one home of both, for the
//! resolver and for every reader of the records.
//!
//! A source's value for one name is its own binding if it has one (an explicit binding
//! shadows every glob), else the union over its edges of each edge's source's value, kept
//! only where the importer may name it and narrowed by the edge's visibility. A cycle of globs
//! passes on only what some member binds: the values are computed together, from none, over
//! the sources the name reaches, until nothing changes (the least fixpoint). One item by two
//! routes is one binding, as visible as the widest; two items are ambiguous (the error is the
//! user's, where the name is used). The merge is commutative, so the result does not depend on
//! the order of the edges or of the walk.

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::Alloc;

use crate::kind::{AmbiguityKind, BindingState, Def, Visibility};
use crate::module_scope::Globs;
use crate::through_scratch::{ThroughScratch, through_in};

/// Who is asking: a module, its crate and its chain of modules from itself up to its crate's
/// root (itself first). Visibility is a question about this chain.
#[derive(Clone, Copy, Debug)]
pub struct Asker<'q> {
    pub krate: CrateItemKey,
    pub chain: &'q [ItemKey],
}

/// What a visibility means to an [`Asker`]. Called as methods (`vis.visible_from(q)`) and by
/// path (`Visibility::narrower(a, b, q)`).
pub trait SeenFrom: Sized {
    /// Whether a module with the chain `from` may name a binding of this visibility.
    fn visible_from(self, from: Asker<'_>) -> bool;

    /// The narrower of `self` and `b`, both seen from a module with the chain `from`: what a
    /// re-export of a `b` binding through a `self` import may be named by. A visibility `from`
    /// cannot see is the narrower one, so the result is never wider than what `from` sees.
    fn narrower(self, b: Self, from: Asker<'_>) -> Self;

    /// The wider of two visibilities that `from` both sees.
    fn wider(self, b: Self, from: Asker<'_>) -> Self;
}

impl SeenFrom for Visibility {
    fn visible_from(self, from: Asker<'_>) -> bool {
        match self {
            Visibility::Public => true,
            Visibility::Crate(krate) => krate == from.krate,
            Visibility::Restricted(module) => from.chain.contains(&module),
        }
    }

    fn narrower(self, b: Visibility, from: Asker<'_>) -> Visibility {
        narrower(self, b, from)
    }

    fn wider(self, b: Visibility, from: Asker<'_>) -> Visibility {
        if narrower(self, b, from) == self { b } else { self }
    }
}

fn narrower(a: Visibility, b: Visibility, from: Asker<'_>) -> Visibility {
    match (a, b) {
        (Visibility::Public, other) | (other, Visibility::Public) => other,
        (Visibility::Crate(_), other @ Visibility::Restricted(_))
        | (other @ Visibility::Restricted(_), Visibility::Crate(_)) => other,
        (Visibility::Crate(x), Visibility::Crate(y)) => {
            if x == from.krate {
                Visibility::Crate(y)
            } else {
                Visibility::Crate(x)
            }
        }
        (Visibility::Restricted(x), Visibility::Restricted(y)) => {
            // The deeper of two ancestors is the narrower; one `from` cannot see is narrowest.
            match (from.chain.iter().position(|&m| m == x), from.chain.iter().position(|&m| m == y)) {
                (Some(i), Some(j)) => {
                    if i <= j {
                        a
                    } else {
                        b
                    }
                }
                (None, _) => a,
                (_, None) => b,
            }
        }
    }
}

/// The most sources one lookup reaches: past it the lookup stops with a typed error.
pub const MAX_REACHED: usize = 1 << 16;

/// A binding as a source passes it on: what it names, who may name it there, and its state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Passed {
    pub def: Def,
    pub vis: Visibility,
    pub state: BindingState,
}

/// What a source is to a lookup through it.
#[derive(Clone, Copy, Debug)]
pub enum Node<'w> {
    /// Its whole value for the name is known (its own binding, an enum's variant, a value
    /// computed elsewhere): no edge of it is followed.
    Value(Option<Passed>),
    /// It binds nothing of its own under the name: its glob edges, and who it is (its crate
    /// and its chain of modules up to its crate's root, itself first).
    Edges { edges: Globs<'w>, krate: CrateItemKey, chain: &'w [ItemKey] },
}

/// The sources a lookup reads, one name at a time.
pub trait Sources<'w> {
    /// Why a source cannot be read now (an answer not given yet, an undetermined binding).
    type Stop;

    /// Whether a source that cannot be read for `stop` is an open node: it gives no value and
    /// no edge, and the lookup stops only if nothing else reaches the asked source (a glob
    /// binding found is not held by another glob still undecided: the stall rule, bottom.md
    /// section 6). When false (the default), such a source stops the lookup. A source whose
    /// record is not given yet is never open: its value is a fact still to arrive.
    fn open(stop: &Self::Stop) -> bool {
        let _ = stop;
        false
    }

    fn node(&mut self, source: Def) -> Result<Node<'w>, Self::Stop>;
}

/// Why a lookup through globs did not finish.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Through<S> {
    /// A source could not be read now.
    Stop(S),
    /// It reached more than [`MAX_REACHED`] sources, or did not settle in its bound.
    Unbounded,
}

/// `source`'s value for the name `sources` reads, through its glob edges: [`through_in`] in a
/// scratch made for this one call. A caller that looks up many names keeps a
/// [`ThroughScratch`] and calls `through_in` itself (blaze-scope's `Sink` does).
pub fn through<'w, W: Sources<'w>>(
    sources: &mut W,
    source: Def,
    alloc: Alloc<'_>,
) -> Result<Option<Passed>, Through<W::Stop>> {
    let mut scratch = ThroughScratch::new_in(alloc);
    through_in(sources, source, &mut scratch)
}

/// Two bindings of one name reaching one module: one item stays one binding, as visible as
/// the wider; two items are ambiguous, kept under the smaller key so the order they came in
/// moves nothing.
pub fn merge(acc: Option<Passed>, p: Passed, asker: Asker<'_>) -> Passed {
    let Some(a) = acc else { return p };
    let state = |x: BindingState, y: BindingState| match (x, y) {
        (BindingState::Ambiguous(k), _) | (_, BindingState::Ambiguous(k)) => BindingState::Ambiguous(k),
        (BindingState::Determined, BindingState::Determined) => BindingState::Determined,
    };
    if a.def == p.def {
        Passed { def: a.def, vis: Visibility::wider(a.vis, p.vis, asker), state: state(a.state, p.state) }
    } else {
        let keep = if p.def < a.def { p } else { a };
        Passed { state: BindingState::Ambiguous(AmbiguityKind::GlobVsGlob), ..keep }
    }
}
