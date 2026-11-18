//! NAMES ON DEMAND (decision D134; docs/v2/plan/design/bottom.md, "work is at item grain"; lane
//! LZ1, 2026-11-18). A name is resolved when it is asked, and only what its answer needs is made.
//!
//! What this replaces: a crate's names made WHOLE in one task (blaze-scope's `CrateScope`: every
//! macro call of the crate expanded, every module's scope resolved). Hello world's first run made
//! the names of all 78 toolchain crates (5.68 s of 6.1 s, 2.0 GB of frame for `core`, 13,764 macro
//! calls expanded) to resolve about a dozen paths.
//!
//! # The pieces
//!
//! - [`types`]: what is asked ([`Ask`]: from which module, a path, a namespace), what is answered
//!   ([`Answer`]), what the machine needs from its driver ([`Need`]: ONE module's outline, ONE macro
//!   call's expansion, ONE crate's root) and what the driver gives back ([`Given`]); a cell's key
//!   ([`CellKey`]) and its reads ([`Read`]).
//! - [`resolver`]: the machine. [`Resolver::ask`], then [`Resolver::step`] until it answers;
//!   between steps the driver gives what [`Resolver::needs`] lists. Pure, deterministic (every
//!   table ordered), bounded (every table and loop has a cap with a typed refusal), holding only
//!   what it was handed; its cells stay for the request, so a name is resolved once a command.
//! - [`driver`]: the seam the engine implements over the existing records ([`driver::Source`]),
//!   and the one loop that answers an ask through it ([`driver::resolve`]).
//!
//! # The semantics (what both this machine and the whole-world oracle compute)
//!
//! A CELL is (module, namespace, name): the module's binding of that name. Its value is the set
//! of definitions (each with the visibility it is bound at) from:
//! 1. EXPLICIT entries: items, `extern crate`, and single imports `use p::x as name` whose target
//!    resolves in that namespace, written in the module or by a macro call expanded in it. Two
//!    explicit entries are a duplicate (rustc E0428/E0252).
//! 2. GLOBS, only when no explicit entry binds the name (RFC 1560: explicit items and
//!    macro-expanded items shadow globs): the union over the module's `use p::*` of the source
//!    module's binding, each definition kept only when visible from the importing module, at the
//!    narrower of the glob's and the binding's visibility. Two definitions are ambiguous (rustc
//!    E0659), reported only when the name is used. In the macro namespace a name written by an
//!    expansion and also given by a glob is ambiguous (rustc's `GlobVsExpanded`).
//! 3. A cycle of globs gives a name nothing unless something outside the cycle gives it: a LEAST
//!    fixpoint. Because a single import that resolves shadows the globs of its module, the
//!    fixpoint is STRATIFIED: a round holds fixed which imports shadow, solves the least fixpoint
//!    from empty, and the next round shadows by the imports that resolved; rounds repeat until
//!    that set stops growing (a set that shrinks is refused: [`Error::Unstable`]).
//! 4. A first segment (`use a::b`, or a bare name asked lexically) is looked up in the module,
//!    then the extern prelude, then the standard prelude's module (neither prelude under
//!    `#![no_implicit_prelude]`); in a `use` path a local name and an extern crate that differ
//!    are ambiguous (Rust 2018's uniform paths).
//!
//! # THE SOUNDNESS RULE FOR UNEXPANDED MACRO CALLS
//!
//! 1. A module is KNOWN for a name when every item-position macro call written in it (by its text
//!    or by an expansion in it) either is expanded or carries a summary ([`Writes`]) that does not
//!    name that name; only then is its cell evaluated.
//! 2. A summary is [`Writes::Any`] unless the driver proves from the macro's definition and the
//!    call's tokens that the expansion can bind only the listed names (every transcribed item and
//!    import name a literal of the definition or an identifier of the call's input) and writes no
//!    glob import and no call that is not itself so summarised: anything else is `Any`.
//! 3. An expansion is checked against its call's summary when it is given: a name outside it, a
//!    glob, or a nested call with a wider summary is refused ([`Error::SummaryBroken`]): a
//!    summary is a claim checked once, never trusted silently.
//! 4. A `#[macro_export]` macro binds at the crate root, so the root is known for a macro-namespace
//!    name only when every module the crate root lists ([`CrateRoot::export_modules`]: those with
//!    an exported macro or a call that may export one) is known for it in the export sense.
//! 5. When in doubt the summary is `Any` and the call is expanded: the machine never guesses.

#![no_std]
#![feature(allocator_api)]
#![forbid(unsafe_code)]

// The planted bugs' thread-local switch, in tests only (crate::plant).
#[cfg(test)]
extern crate std;

pub mod driver;
mod plant;
pub mod resolver;
pub mod types;

#[cfg(test)]
mod sim_tests;

pub use resolver::{Counts, Resolver, Step};
pub use types::{
    Ambiguity, Answer, Anchor, Ask, CallRef, CellKey, CrateRoot, Def, Entry, Error, Extern, Given, Limit,
    ModRef, Need, Ns, Outline, Path, Read, Spaces, Vis, Writes,
};
