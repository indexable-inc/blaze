//! The compiler a real lazy run gives its [`Runner`](crate::runner::Runner): codegen's lazy
//! build of the image, kept for the run, compiling each function against the front end as it
//! stands when the function is first called.
//!
//! There is one database and no fork of it (docs/v2/plan/lanes/frontend/db.md), and a run that
//! held a reader for its life would hold off every edit while the program runs. So a compile
//! takes a reader for itself alone ([`Source::reader`]) and lets it go once the function is
//! compiled, before it is placed. A write meanwhile stops the reader, and the compile starts
//! again on the next one, which [`Source::reader`] gives only once the write is done.
//!
//! What makes a compile at a later revision exact is the function's key, not the revision. A
//! lazy function's slot only ever runs code of the key the image was linked with (blaze-link
//! never puts a patch's code in a lazy slot), and a key names its code, as it does for the
//! store's templates across builds. So codegen compiles `key` at the reader's revision only
//! while the front end still describes the instance under `key`. When it describes it under
//! another (the function was edited before its first call), the compile is refused, naming the
//! function and both keys: never the new body placed into the old image. A memo hit needs no
//! reader at all, so a first compile probes the memo before it takes one.
//!
//! What a compile makes for the store (its template and side table) goes to [`Source::keep`],
//! so the next build's checks and first calls read it instead of building IR again. A memo hit
//! answered by the thread that read its ask keeps its template only once the askers are
//! answered ([`Source::keep_hit`]), off their path.

use blaze_codegen::driver::Put;
use blaze_codegen::{CompiledFunction, InstanceKey};
use blaze_stencil::tier::Tier;

use crate::runner::Compiler;

/// A function as codegen's lazy build makes it, with what the store keeps of it.
pub struct Kept {
    pub function: CompiledFunction,
    pub puts: Vec<Put>,
}

/// Why a compile against one reader gave no code.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// A write to the database stopped the reader before the compile ended (the query engine's
    /// `Cancelled::PendingWrite`): compiled again on the next reader.
    Stopped,
    /// The function can't be compiled, and why, naming it: the run's failure.
    Failed(String),
}

/// What a lazy run compiles from: codegen's lazy build of the image kept for the run
/// (`blaze_codegen::driver::Later`), the front end it reads, the memo and the store.
pub trait Source: Send + Sync + 'static {
    /// A hold on the database at one revision, for one compile.
    type Reader;

    /// The database as it stands now. Takes no lock while no write is pending; while one is,
    /// waits for it to end, so it never gives a reader a write has stopped already.
    fn reader(&self) -> Result<Self::Reader, String>;

    /// `key`'s code from the memo (`Later::memo_hit`), with no reader, no store read, no IR
    /// built and nothing encoded for the store: `None` when the memo has none. An `Err` names
    /// why the memo's template can't be made into `key`'s function, which a compile would find
    /// too.
    fn memo_hit(&self, key: InstanceKey) -> Option<Result<CompiledFunction, String>>;

    /// After `key`'s memo hit: the memo's template kept in the store under `key`'s key
    /// (`Later::memo_put`, then [`Source::keep`]), so the next build's check reads it.
    fn keep_hit(&self, key: InstanceKey);

    /// `key` compiled against `reader` (`Later::compile_kept`; with `to`, `Later::tier_up` at
    /// that tier). Refused when the front end at `reader`'s revision describes the instance
    /// under a key other than `key`.
    fn compile(&self, reader: &Self::Reader, key: InstanceKey, to: Option<Tier>) -> Result<Kept, Refusal>;

    /// Puts `puts` in the store, from any thread, without waiting on the store's writer.
    fn keep(&self, puts: Vec<Put>);
}

/// The most writes in a row one compile is stopped by before that is an error: far past any
/// burst of edits within one compile, for a [`Source::reader`] that waits out a write as it
/// must. One that doesn't would have the compile spin on stopped readers.
pub const MOST_STOPS: u32 = 64;

/// The [`Compiler`] of a lazy run over a [`Source`].
pub struct Lazily<S: Source> {
    source: S,
}

impl<S: Source> Lazily<S> {
    pub fn new(source: S) -> Lazily<S> {
        Lazily { source }
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    /// `key` compiled (at `to`, for a tier-up) on a reader of its own, again on the next one
    /// for each write that stops it, with what it made for the store kept.
    fn run(&self, key: InstanceKey, to: Option<Tier>) -> Result<CompiledFunction, String> {
        for _ in 0..MOST_STOPS {
            let reader = self.source.reader()?;
            let compiled = self.source.compile(&reader, key, to);
            drop(reader);
            match compiled {
                Ok(kept) => return Ok(self.keep(kept)),
                Err(Refusal::Stopped) => continue,
                Err(Refusal::Failed(why)) => return Err(why),
            }
        }
        // alloc: an error
        Err(format!("stopped by {MOST_STOPS} writes to the database in a row: a reader must wait out a write in progress"))
    }

    fn keep(&self, kept: Kept) -> CompiledFunction {
        if !kept.puts.is_empty() {
            self.source.keep(kept.puts);
        }
        kept.function
    }
}

impl<S: Source> Compiler for Lazily<S> {
    fn compile(&self, key: InstanceKey) -> Result<CompiledFunction, String> {
        if let Some(hit) = self.source.memo_hit(key) {
            let function = hit?;
            self.source.keep_hit(key);
            return Ok(function);
        }
        self.run(key, None)
    }

    fn hit(&self, key: InstanceKey) -> Option<Result<CompiledFunction, String>> {
        self.source.memo_hit(key)
    }

    fn hit_answered(&self, key: InstanceKey) {
        self.source.keep_hit(key);
    }

    fn tier_up(&self, key: InstanceKey, to: Tier) -> Result<CompiledFunction, String> {
        self.run(key, Some(to))
    }
}
