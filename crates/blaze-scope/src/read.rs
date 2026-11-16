//! The protocol between a machine and its caller: the reads it asks, the answers it takes, the
//! expansions it asks for, and what one step returns.

use blaze_id::{ContentId, CrateItemKey, ItemKey};

use blaze_surface::{Binding, BindingState, Def, GlobEdge, Namespace, Origin, TextualDef, Visibility};

/// A read of another module's records.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Read<N> {
    /// The binding of `name` in `ns` of `module`'s scope (a module or an enum): the module's
    /// own (declared, or bound by a named import), never one reached through a glob.
    Name { module: ItemKey, ns: Namespace, name: N },
    /// `module`'s glob edges and who it is: what a name absent from its own bindings is
    /// looked up through (an enum has none).
    Globs { module: ItemKey },
    /// One level of the `macro_rules!` visible at `module`'s `mod` line: the parent's OWN
    /// sequence up to the line, read from the parent's record alone (it waits on the parent
    /// only). The resolver walks the chain, one read per level, live at its group's ancestors.
    TextualEntry { module: ItemKey },
    /// What a `#[macro_use]` on `module`'s `mod` line carries out: its own textual sequence.
    TextualOut { module: ItemKey },
    /// The helper attributes a derive macro declares (`#[proc_macro_derive(D, attributes(h))]`),
    /// read from the record of the macro's definition.
    DeriveHelpers { derive: ItemKey },
}

impl<N> Read<N> {
    /// The module the read is of, or for `DeriveHelpers` the derive macro.
    pub fn module(&self) -> ItemKey {
        match *self {
            Read::Name { module, .. }
            | Read::Globs { module }
            | Read::TextualEntry { module }
            | Read::TextualOut { module } => module,
            Read::DeriveHelpers { derive } => derive,
        }
    }
}

/// What a `Name` read answers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NameAnswer {
    /// No binding of the name: a negative read, kept in the read list like any other.
    Absent,
    /// `origin` says whether the binding is an `extern crate` item: a crate root's such entry
    /// is in the extern prelude (the Reference, "Extern prelude"), also one a macro wrote.
    Found { def: Def, vis: Visibility, state: BindingState, origin: Origin },
}

impl From<Option<Binding>> for NameAnswer {
    /// What a scope's binding of the name answers: `NameAnswer::from(view.get(ns, name))` over
    /// a `ModuleScope` record, or over a working form's `get(..).copied()`.
    fn from(binding: Option<Binding>) -> NameAnswer {
        match binding {
            Some(b) => NameAnswer::Found { def: b.def, vis: b.vis, state: b.state, origin: b.origin },
            None => NameAnswer::Absent,
        }
    }
}

/// The answer to one read.
#[derive(Clone, Copy, Debug)]
pub enum Answer<'a, N> {
    Name(NameAnswer),
    /// The module's glob edges (by source), its crate, its chain of modules from itself up to
    /// its crate's root (itself first: what visibility is checked against at its edges), and
    /// the digest of the three ([`blaze_surface::globs_digest`]).
    Globs {
        edges: &'a [GlobEdge],
        krate: CrateItemKey,
        chain: &'a [ItemKey],
        digest: ContentId,
    },
    /// The textual list in order, and its digest.
    Textual { defs: &'a [TextualDef<N>], digest: ContentId },
    /// A derive's helper attribute names (any order), and their digest.
    Helpers { names: &'a [N], digest: ContentId },
    /// The module is being computed and is waiting, directly or not, on the asker: a cycle.
    /// The asker joins the module in a [`crate::Group`]. `module` is the module the answerer
    /// waits on for this answer: for a `TextualEntry`, the parent (one level; the resolver
    /// walks the chain itself). A `Step::Cycle` names exactly these modules, so the answerer is
    /// the one home of "what this read waits on".
    InProgress { module: ItemKey },
}

/// A read and its answer.
#[derive(Clone, Copy, Debug)]
pub struct Reply<'a, N> {
    pub read: Read<N>,
    pub answer: Answer<'a, N>,
}

/// A macro call whose path the machine has resolved and whose expansion it needs: the caller
/// expands `call` with the macro `def` and makes [`crate::ScopeFacts::expansion`] answer it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ExpandRequest {
    pub call: ItemKey,
    pub def: Def,
}

/// What one step of a machine returns.
///
/// - `Ask`: the reads it needs and has no answer to, sorted. The caller answers any of them,
///   in any order, at the next step; an unanswered one is asked again.
/// - `Expand`: the calls whose expansion it needs, sorted, once nothing is left to ask.
/// - `Cycle`: the modules it waits on that answered [`Answer::InProgress`], sorted, once
///   nothing is left to ask or expand: it cannot finish alone. The caller puts it and those
///   modules' machines in one [`crate::Group`].
/// - `Done`: the result.
#[derive(Debug)]
pub enum Step<'s, N, D> {
    Ask(&'s [Read<N>]),
    Expand(&'s [ExpandRequest]),
    Cycle(&'s [ItemKey]),
    Done(D),
}
