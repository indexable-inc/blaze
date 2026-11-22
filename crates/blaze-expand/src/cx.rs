//! Everything outside the call's tokens and the definition: asked for through [`ExpandCx`],
//! each answer recorded in the expansion as a [`crate::Read`] so the record is valid exactly
//! while those answers stand. A builtin asks through a [`Recorder`], never the context itself:
//! each of its methods pushes the read before it hands the answer back, so no builtin can hold
//! an answer its read list lacks.

use core::fmt;

use blaze_id::ContentId;
use blaze_own::{AVec, Alloc};

use crate::{
    builtin::stringified,
    cfg::{CfgPredicateError, cfg_predicate},
    def::{MacroDefinition, ProcKind},
    error::ExpandError,
    expansion::{ProcLevel, ReadAnswer, ReadKind, Reads, TokenOrigin},
    literal::{Buf, escape_str},
    tokens::{Ctx, Edition, LitKind, Spacing, Text, TokenStream},
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

    /// [`Self::place`] for the call written at byte `call` of the text this host was made for
    /// (item-relative, rule 71): a host made for a whole body, which holds many calls, is told
    /// which one by its caller (blaze-lower by_name.rs `BodyCx`, the outermost WRITTEN call of
    /// the eager input: core `line!`, "the first macro invocation leading up to the
    /// expansion"). A host made for one call (blaze-scope's item host) already knows it: the
    /// default is its `place`.
    fn place_of_call(&self, what: PlaceOf, call: u32) -> Result<Place<'_>, HostError> {
        let _ = call;
        self.place(what)
    }

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

/// The one way a builtin reaches the outside: the context and the read list the answers go
/// to, held together. Each method asks the context, pushes its read (the subject written out
/// here, never by the caller) and only then returns the answer; the context is a private field,
/// so a builtin given a `&mut Recorder` cannot ask it anything unrecorded.
pub(crate) struct Recorder<'x, 'a, C: ExpandCx> {
    cx: &'x C,
    reads: &'x mut Reads<'a>,
    /// The call's edition: an atom's read is lexed again in it ([`Reads::hold`]).
    edition: Edition,
    /// Where an atom's tokens and a subject's text are built.
    scratch: Alloc<'a>,
}

/// A [`Recorder`]'s parts, each one required.
pub(crate) struct RecorderOpts<'x, 'a, C: ExpandCx> {
    pub(crate) cx: &'x C,
    pub(crate) reads: &'x mut Reads<'a>,
    pub(crate) edition: Edition,
    pub(crate) scratch: Alloc<'a>,
}

impl<'x, 'a, C: ExpandCx> Recorder<'x, 'a, C> {
    pub(crate) fn new(opts: RecorderOpts<'x, 'a, C>) -> Recorder<'x, 'a, C> {
        let RecorderOpts { cx, reads, edition, scratch } = opts;
        Recorder { cx, reads, edition, scratch }
    }

    /// The env var `name`, recorded under its name with its value's digest (or as absent).
    pub(crate) fn env(&mut self, name: &str) -> Result<Option<&'x str>, ExpandError> {
        let value = self.cx.env(name)?;
        self.reads.push(ReadKind::Env, name, ReadAnswer::of_env(value))?;
        Ok(value)
    }

    /// The file `written` names, read `how`, recorded under the root-relative path a host with
    /// a [`ReadRoot`] answers (D60), else the path as written, with the bytes' digest (or as
    /// absent). A rooted answer whose path is not root-relative is refused, never recorded: it
    /// would put the machine's folder or a spelling into the record. [`Included::Outside`] is
    /// decided by the written path and the calling file's place alone, so it is no read.
    pub(crate) fn include(&mut self, written: &str, how: IncludeAs) -> Result<Included<'x>, ExpandError> {
        let included = self.cx.include(written, how)?;
        // `include!` reads the file as text, the same fact `include_str!` reads.
        let kind = match how {
            IncludeAs::Tokens | IncludeAs::Str => ReadKind::IncludeStr,
            IncludeAs::Bytes => ReadKind::IncludeBytes,
        };
        let (subject, found) = match included {
            Included::Bytes(bytes) => (written, Some(bytes)),
            Included::Missing => (written, None),
            Included::At { path, bytes } => (rooted(path)?, Some(bytes)),
            Included::MissingAt { path } => (rooted(path)?, None),
            Included::Outside => return Ok(included),
        };
        let answer = match found {
            Some(bytes) => ReadAnswer::Digest(ContentId::of("blaze v2 expand included file 1", bytes)),
            None => ReadAnswer::Absent,
        };
        self.reads.push(kind, subject, answer)?;
        Ok(included)
    }

    /// The call's position `what` (asked only inside eager input), recorded with no subject.
    pub(crate) fn place(&mut self, what: PlaceOf) -> Result<Place<'x>, ExpandError> {
        let place = self.cx.place(what)?;
        let kind = match what {
            PlaceOf::Line => ReadKind::Line,
            PlaceOf::Column => ReadKind::Column,
            PlaceOf::File => ReadKind::File,
            PlaceOf::ModulePath => ReadKind::ModulePath,
        };
        self.reads.push(kind, "", ReadAnswer::of_place(place))?;
        Ok(place)
    }

    /// The outcome of the cfg `predicate`, evaluated here ([`cfg_predicate`]: every atom, in
    /// source order, no short cut) with each atom asked of the context as a predicate of its
    /// own (`unix`, `feature = "x"`) and recorded, one read per atom in evaluation order, its
    /// subject the atom's text and its answer the atom's outcome. The predicate's text is no
    /// subject: `all(unix, feature = "x")` and `all(unix, any(feature = "x"))` read the same
    /// two facts, and a changed set reruns exactly the calls whose atoms moved. A predicate
    /// that is none is [`CfgPredicateError::Malformed`] or `NotTakesOne`, its token an index of
    /// `predicate`; a failed ask is [`CfgPredicateError::Atom`].
    pub(crate) fn cfg(&mut self, predicate: &TokenStream<'_>) -> Result<bool, CfgPredicateError<ExpandError>> {
        let Recorder { cx, reads, edition, scratch } = self;
        let (cx, root, scratch) = (*cx, Ctx::root(*edition), *scratch);
        cfg_predicate(predicate, scratch, &mut |name, value| -> Result<bool, ExpandError> {
            let mut atom = TokenStream::new_in(scratch);
            atom.ident(name, root)?;
            if let Some(value) = value {
                atom.punct('=', Spacing::Alone, root)?;
                let mut text = Buf::new_in(scratch);
                escape_str(value, &mut text);
                atom.literal(LitKind::Str, text.as_str(), 0, root)?;
            }
            let outcome = cx.cfg(&atom)?;
            let mut subject = Buf::new_in(scratch);
            stringified(&atom, scratch, &mut subject)?;
            reads.push(ReadKind::Cfg, subject.as_str(), ReadAnswer::Bool(outcome))?;
            Ok(outcome)
        })
    }
}

/// `path` when it is root-relative ([`is_root_relative`]); a host's rooted answer that is not
/// one is unreadable.
fn rooted(path: &str) -> Result<&str, ExpandError> {
    if is_root_relative(path) {
        Ok(path)
    } else {
        Err(HostError { ask: HostAsk::Include, why: HostWhy::Unreadable }.into())
    }
}

/// How an included file is read, which the host is told so it can tell a module file from
/// data: `include!`'s file is Rust whose items or expression join the crate (its host reads it
/// as a module file, whose surface the names read); `include_str!`'s and `include_bytes!`'s are
/// data (bytes whose digest a reader's read list holds, never a module).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum IncludeAs {
    /// `include!`: the file's tokens.
    Tokens,
    /// `include_str!`: a string literal of the file's text.
    Str,
    /// `include_bytes!`: a byte string literal of the file's bytes.
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

impl PlaceOf {
    /// The line or the column (`self` is [`PlaceOf::Line`] or [`PlaceOf::Column`]) of byte `at`
    /// of `text`, the ONE count a host answers them by: the line from 1, the column from 1 in
    /// characters past the line's start (core `line!`, `column!`: "1-based ... counted in
    /// UTF-8 characters"). `None`: `at` is past `text` or inside a character, or `self` is no
    /// number.
    pub fn counted(self, text: &str, at: usize) -> Option<u32> {
        let before = text.get(..at)?;
        let number = match self {
            PlaceOf::Line => before.bytes().filter(|b| *b == b'\n').count() + 1,
            PlaceOf::Column => before[before.rfind('\n').map_or(0, |n| n + 1)..].chars().count() + 1,
            PlaceOf::File | PlaceOf::ModulePath => return None,
        };
        u32::try_from(number).ok()
    }
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
    /// The file holding the call, as `file!()` shows it there ([`PlaceOf::File`], D96: a
    /// member's workspace-relative path, a dependency's `<name>-<version>/<path>`), or why the
    /// host does not answer it. What `Span::file` answers for a token written in that file
    /// (blaze-proc-macro server.rs). Asked of the host before the run, but no read until the
    /// macro asks it: the run records it then, and a kept answer that read it is taken for a
    /// call only where it is answered the same (blaze-proc-macro table.rs).
    pub file: Result<&'a str, HostError>,
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
