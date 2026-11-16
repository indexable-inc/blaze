//! Everything outside the call's tokens and the definition: asked for through [`ExpandCx`],
//! each answer recorded in the expansion as a [`crate::Read`] so the record is valid exactly
//! while those answers stand.

use core::fmt;

use blaze_own::{AVec, Alloc};

use crate::{
    def::{MacroDefinition, ProcKind},
    expansion::{ProcLevel, Reads, TokenOrigin},
    tokens::{Text, TokenStream},
};
#[cfg(doc)]
use crate::expansion::Diagnostic;

/// The outside of an expansion. The expansion asks it for nothing it does not record.
pub trait ExpandCx {
    /// The environment variable `name` as the crate is built: `None` when it is not set.
    fn env(&self, name: &str) -> Result<Option<&str>, HostError>;

    /// The outcome of the cfg `predicate` for the crate the call is in.
    fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, HostError>;

    /// The file `path` names, as written in the call, from the file of the call.
    fn include(&self, path: &str, how: IncludeAs) -> Result<Included<'_>, HostError>;

    /// The call's position, asked only for a call in eager input ([`crate::Inside::EagerInput`]).
    fn place(&self, what: PlaceOf) -> Result<Place<'_>, HostError>;

    /// Runs the proc macro `def` on `call`, in its sandbox. Its output tokens come with their
    /// origins, one per token: an index of the input or attribute arguments it passed on, or
    /// made at `Span::call_site` ([`crate::Transparency::Transparent`]), `mixed_site`
    /// (`SemiOpaque`) or `def_site` (`Opaque`).
    fn run_proc_macro<'a>(
        &self,
        def: &MacroDefinition<'_>,
        call: ProcCall<'_>,
        alloc: Alloc<'a>,
    ) -> Result<Produced<'a>, HostError>;
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
    /// The file's bytes, from a host with no [`ReadRoot`] (a test's files, a build script's
    /// output record): the read list names it by the path as written.
    Bytes(&'a [u8]),
    /// No file at that path, from a host with no [`ReadRoot`].
    Missing,
    /// The file at `path` under the unit's [`ReadRoot`], and its bytes. `path` is the
    /// segments [`include_target`] gives joined by `/` (D60, docs/v2/plan/decisions.json: "an
    /// include is bounded by the unit's source root and its digest goes in the read list"), so
    /// the read list names the file by where it is in the root, never by the machine's folder
    /// or the call's spelling: two calls naming one file from two folders read one subject, and
    /// a changed file finds its readers by its root-relative path. A path that is not
    /// root-relative ([`is_root_relative`]) is refused, never recorded.
    At { path: &'a str, bytes: &'a [u8] },
    /// No file at the root-relative `path` under the unit's [`ReadRoot`] (as [`Included::At`]):
    /// recorded as absent under that path, so creating the file reruns the reader.
    MissingAt { path: &'a str },
    /// The path leaves the unit's [`ReadRoot`] (decision D60: [`include_target`]); a build
    /// script's output folder is answered from its record before a host is asked. Decided by
    /// the written path and the calling file's place alone, so it is no read.
    Outside,
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
pub struct ProcCall<'a> {
    pub kind: ProcKind,
    /// The fn-like call's arguments, or the item.
    pub input: &'a TokenStream<'a>,
    /// An attribute's arguments.
    pub attr_args: Option<&'a TokenStream<'a>>,
}

/// What a proc macro gave back: its tokens, each one's origin, and the outside reads its run
/// made (env vars, files), recorded by the sandbox.
pub struct Produced<'a> {
    pub tokens: TokenStream<'a>,
    pub origins: AVec<'a, TokenOrigin>,
    pub reads: Reads<'a>,
    /// The macro's diagnostics; each `Text` is a range of `text`.
    pub diagnostics: AVec<'a, ProcDiagnostic>,
    pub text: AVec<'a, u8>,
}

/// A diagnostic a proc macro's run gives: only the two kinds a proc macro can make, so a host
/// cannot hand back another kind's (a type, not a check). Each becomes a [`Diagnostic`] of the
/// expansion, its text copied into the expansion's notes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ProcDiagnostic {
    /// `proc_macro::Diagnostic::emit`: [`Diagnostic::ProcMacro`].
    Emitted { level: ProcLevel, message: Text, at: Option<TokenOrigin> },
    /// The macro panicked: [`Diagnostic::ProcMacroPanicked`].
    Panicked { message: Text },
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
    /// A limit stopped the proc macro's run.
    Limit(ProcLimit),
}

/// Which limit stopped a proc macro's run.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ProcLimit {
    Fuel,
    Memory,
    Output,
    Paths,
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

/// Where a unit's sources came from: what bounds the files its includes may read (decision
/// D60, docs/v2/plan/decisions.json).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SourceOrigin {
    /// A workspace member inside a version-control checkout: it reads under the checkout's root.
    Checkout,
    /// A workspace member with no checkout around it: it reads under the workspace's root.
    Workspace,
    /// A git source: it reads under the whole fetched checkout.
    GitCheckout,
    /// A registry source (or any unit fetched as a package alone): it reads under its package.
    Package,
}

/// The tree a unit's `include!`, `include_str!` and `include_bytes!` read in: a value the host
/// is given per unit, never found from the machine by the expander (D60). Paths under it are
/// `/`-separated and written without `.` or `..`, so no key holds where the tree is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ReadRoot<'s> {
    pub origin: SourceOrigin,
    /// The unit's package folder under the root; empty when the package is the root.
    pub package: &'s str,
}

impl ReadRoot<'_> {
    /// The folder under the root no read may leave: the package for
    /// [`SourceOrigin::Package`], the whole root otherwise.
    fn bound(&self) -> &str {
        match self.origin {
            SourceOrigin::Package => self.package,
            SourceOrigin::Checkout | SourceOrigin::Workspace | SourceOrigin::GitCheckout => "",
        }
    }
}

/// An include path that leaves its unit's [`ReadRoot`] ([`Included::Outside`]): refused by
/// decision D60, by the written path and the calling file's place alone.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct OutsideRoot {
    pub origin: SourceOrigin,
}

impl fmt::Display for OutsideRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bound = match self.origin {
            SourceOrigin::Checkout => "the workspace's checkout",
            SourceOrigin::Workspace => "the workspace's root",
            SourceOrigin::GitCheckout => "its fetched git checkout",
            SourceOrigin::Package => "its fetched package",
        };
        write!(f, "the path leaves {bound} (decision D60: includes read only under the unit's source root)")
    }
}

impl core::error::Error for OutsideRoot {}

/// Whether `path` is a root-relative file path as [`include_target`]'s segments joined by `/`
/// make one: not empty, no leading or doubled `/`, no `.` or `..` segment, no `\`. A path a
/// host answers in [`Included::At`] or [`Included::MissingAt`] must be one, or a record would
/// hold the machine's folder or a spelling of the path.
pub fn is_root_relative(path: &str) -> bool {
    !path.is_empty() && !path.contains('\\') && path.split('/').all(|segment| !matches!(segment, "" | "." | ".."))
}

/// The file `include!`, `include_str!` or `include_bytes!` reads when the call is written in a
/// file of the folder `folder` (under the package of `root`) and names `written`, as segments
/// under the root. The rule is the pinned library's own (core `macros/mod.rs`, `include_str!`:
/// "The file is located relative to the current file (similarly to how modules are found)");
/// it may leave the package (D60) but never the root's bound ([`ReadRoot::bound`]), and an
/// absolute path names the machine, so it is outside. `.` and `..` are taken lexically, never
/// through the file system. The segments borrow `root`, `folder` and `written`; nothing is read.
pub fn include_target<'s, 'a>(
    root: ReadRoot<'s>,
    folder: &'s str,
    written: &'s str,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, &'s str>, OutsideRoot> {
    let outside = OutsideRoot { origin: root.origin };
    if written.starts_with('/') {
        return Err(outside);
    }
    let mut at = AVec::new_in(alloc);
    for segment in root.package.split('/').chain(folder.split('/')).chain(written.split('/')) {
        match segment {
            "" | "." => {}
            ".." => {
                if at.pop().is_none() {
                    return Err(outside);
                }
            }
            name => at.push(name),
        }
    }
    let mut bound = root.bound().split('/').filter(|s| !s.is_empty());
    let mut ours = at.iter();
    loop {
        match (bound.next(), ours.next()) {
            (None, _) => return Ok(at),
            (Some(want), Some(have)) if want == *have => {}
            _ => return Err(outside),
        }
    }
}
