//! blaze-names-demand's [`blaze_names_demand::driver::Source`] over real inputs (decision D134):
//! the demand machine asks for one module's outline, one macro call's expansion or one crate's
//! root, and this crate answers from the crate's texts, sans-IO.
//!
//! This first piece is the bridge every answer needs: a call's [`Writes`] from blaze-expand's
//! [`MacroWrites`] (what a definition can write, crate `blaze_expand::writes`), and the [`Texts`]
//! a driver reads files through. Its tests hold the soundness property of the summaries against
//! real expansions (tests/writes_sound.rs). The `Source` impl itself is [`CrateSource`]
//! (src/source.rs), judged against blaze-scope's whole-crate names (tests/demand_fixture.rs).
//!
//! # The soundness rule, as this crate keeps it
//!
//! A call summarised `Only { names }` writes no binding outside `names` in the module it is
//! expanded in, no glob, and no call that is not folded into the summary; `exports` says it may
//! write a `#[macro_export]` macro, bound at the crate root. Every doubt is [`Writes::Any`].

#![no_std]

use blaze_expand::{CallWrites, MacroWrites, TokenStream, Written};
use blaze_id::CrateItemKey;
use blaze_names_demand::types::Writes;
use blaze_own::{AVec, Bump, Vec};

mod error;
mod source;

pub use error::SourceError;
pub use source::{CrateFacts, CrateSource, CrateSourceOpts, MOST_FILES, MOST_HELD, SourceCounts, Summaries};

/// How many resolvers deep a callee may be asked (rule 18): a call's macro found by an ask
/// whose modules have calls whose macros are found by an ask ... Each layer is a fresh
/// `Resolver` over the same source, so the chain is bounded here, not by the machine.
pub const MOST_CALLEE_DEPTH: usize = 4;

/// The texts of a crate's files, as the driver holds them (read from disk, a VCS, a test's
/// table). `None` for a file that does not exist; the caller says what that means.
pub trait Texts {
    /// The text of `path`, relative to `krate`'s root directory (`src/lib.rs`, `src/a/b.rs`).
    fn read(&self, krate: CrateItemKey, path: &str) -> Option<&str>;
}

/// The demand machine's summary of one call of the definition `writes` summarises, its input
/// `input` (`None` when the input is not at hand: a summary that needs it is then `Any`), its
/// names in `alloc`.
pub fn call_writes<'w>(writes: &MacroWrites<'w, '_>, input: Option<&'w TokenStream<'w>>, alloc: Bump<'w>) -> Writes<'w> {
    call_writes_keeping(writes, input, &|_: &Written<'w>| true, alloc)
}

/// [`call_writes`] with only the definition's literal names `keep` holds. The soundness test's
/// planted control (a summary that forgets one way of writing a name) is this with a filter;
/// [`call_writes`] keeps all.
pub fn call_writes_keeping<'w>(
    writes: &MacroWrites<'w, '_>,
    input: Option<&'w TokenStream<'w>>,
    keep: &impl Fn(&Written<'w>) -> bool,
    alloc: Bump<'w>,
) -> Writes<'w> {
    let mut names: AVec<'w, &'w str> = Vec::new_in(alloc.alloc());
    match writes.for_call_keeping(input, keep, &mut names) {
        CallWrites::Any(_) => Writes::Any,
        CallWrites::Only { exports } => Writes::Only { names: alloc.alloc_slice_copy(&names), exports },
    }
}
