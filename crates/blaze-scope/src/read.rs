//! The protocol between a machine and its caller: the reads it asks, the answers it takes, the
//! expansions it asks for, and what one step returns.

use blaze_id::{ContentId, ItemKey};

use crate::scope::{BindingState, Def, ExportEntry, Namespace, TextualDef, Visibility, VisibilityClass};

/// A read of another module's records.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Read<N> {
    /// The binding of `name` in `ns` of `module`'s scope (a module or an enum).
    Name { module: ItemKey, ns: Namespace, name: N },
    /// The bindings of one visibility class of `module`'s scope: what a glob reads.
    Exports { module: ItemKey, class: VisibilityClass },
    /// The `macro_rules!` visible at `module`'s `mod` line (its `TextualEntry` record): the
    /// parent's entry and the parent's sequence up to the line.
    TextualEntry { module: ItemKey },
    /// What a `#[macro_use]` on `module`'s `mod` line carries out: its own textual sequence.
    TextualOut { module: ItemKey },
}

impl<N> Read<N> {
    /// The module the read is of.
    pub fn module(&self) -> ItemKey {
        match *self {
            Read::Name { module, .. }
            | Read::Exports { module, .. }
            | Read::TextualEntry { module }
            | Read::TextualOut { module } => module,
        }
    }
}

/// What a `Name` read answers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NameAnswer {
    /// No binding of the name: a negative read, kept in the read list like any other.
    Absent,
    Found { def: Def, vis: Visibility, state: BindingState },
}

/// The answer to one read.
#[derive(Clone, Copy, Debug)]
pub enum Answer<'a, N> {
    Name(NameAnswer),
    /// The class's bindings (any order) and the digest of the class in the module's record.
    Exports { entries: &'a [ExportEntry<N>], digest: ContentId },
    /// The textual list in order, and its digest.
    Textual { defs: &'a [TextualDef<N>], digest: ContentId },
    /// The module is being computed and is waiting, directly or not, on the asker: a cycle.
    /// The asker joins the module in a [`crate::Group`].
    InProgress,
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
