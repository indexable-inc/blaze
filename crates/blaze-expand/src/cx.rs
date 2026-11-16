//! Everything outside the call's tokens and the definition: asked for through [`ExpandCx`],
//! each answer recorded in the expansion as a [`crate::Read`] so the record is valid exactly
//! while those answers stand.

use core::alloc::Allocator;
use core::fmt;

use alloc::vec::Vec;

use crate::{
    def::{MacroDef, ProcKind},
    expansion::{Origin, Reads},
    tokens::TokenStream,
};

/// The outside of an expansion. The expansion asks it for nothing it does not record.
pub trait ExpandCx {
    /// The environment variable `name` as the crate is built: `None` when it is not set.
    fn env(&self, name: &str) -> Result<Option<&str>, HostError>;

    /// The outcome of the cfg `predicate` for the crate the call is in.
    fn cfg<A: Allocator>(&self, predicate: &TokenStream<A>) -> Result<bool, HostError>;

    /// The file `path` names, as written in the call, from the file of the call.
    fn include(&self, path: &str, how: IncludeAs) -> Result<Included<'_>, HostError>;

    /// The call's position, asked only for a call in eager input ([`crate::Inside::EagerInput`]).
    fn place(&self, what: PlaceOf) -> Result<Place<'_>, HostError>;

    /// Runs the proc macro `def` on `call`, in its sandbox. Its output tokens come with their
    /// origins, one per token: an index of the input or attribute arguments it passed on, or
    /// made at `Span::call_site` ([`crate::Transparency::Transparent`]), `mixed_site`
    /// (`SemiOpaque`) or `def_site` (`Opaque`).
    fn run_proc_macro<A: Allocator + Clone>(
        &self,
        def: &MacroDef<A>,
        call: ProcCall<'_, A>,
        alloc: A,
    ) -> Result<Produced<A>, HostError>;
}

/// How an included file is read.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum IncludeAs {
    Str,
    Bytes,
}

/// A file read for `include_str!` or `include_bytes!`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Included<'a> {
    Bytes(&'a [u8]),
    /// No file at that path.
    Missing,
}

/// A position a call in eager input asks for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PlaceOf {
    /// The line of the outermost call, from 1.
    Line,
    /// Its column, from 1, in characters.
    Column,
    /// The path of its file, as `file!()` prints it.
    File,
    /// The path of its module, as `module_path!()` prints it.
    ModulePath,
}

/// The answer to a [`PlaceOf`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Place<'a> {
    Number(u32),
    Text(&'a str),
}

/// A proc macro's call.
pub struct ProcCall<'a, A: Allocator> {
    pub kind: ProcKind,
    /// The fn-like call's arguments, or the item.
    pub input: &'a TokenStream<A>,
    /// An attribute's arguments.
    pub attr_args: Option<&'a TokenStream<A>>,
}

/// What a proc macro gave back: its tokens, each one's origin, and the outside reads its run
/// made (env vars, files), recorded by the sandbox.
pub struct Produced<A: Allocator> {
    pub tokens: TokenStream<A>,
    pub origins: Vec<Origin, A>,
    pub reads: Reads<A>,
}

/// Which outside question failed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum HostAsk {
    Env,
    Cfg,
    Include,
    Place,
    ProcMacro,
}

/// Why it failed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum HostWhy {
    /// The answer could not be read (an I/O error, a value that is not Unicode).
    Unreadable,
    /// The context refuses the question (a path outside the crate, a sandbox refusal).
    Refused,
    /// The proc macro panicked or its runner failed.
    Failed,
    /// The answer did not come within its deadline.
    TimedOut,
}

/// An outside question the context could not answer: it stops the expansion, and is never an
/// answer of the record.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct HostError {
    pub ask: HostAsk,
    pub why: HostWhy,
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the expansion's {:?} read failed: {:?}", self.ask, self.why)
    }
}

impl core::error::Error for HostError {}
