//! An [`Expand`] whose calls' paths are resolved by one [`MacroNames`]: a real build's is
//! [`crate::scoped::Scoped`] (the names library's `Names::macro_definition` in the body's
//! module, any crate). [`Lists`], the stand-in by last name over blaze-expand's builtins and
//! the caller's `macro_rules!` definitions, stays only for tests and census runs that have no
//! names. It never guesses past its resolver (a path it names no macro for is
//! [`Expansion::Unsupported`] and a [`MacroFinding`]). Sans-IO: the definitions and the host
//! ([`ExpandCx`]: the environment, cfg, included files, positions) are the caller's.
//!
//! An eager builtin (`concat!`, `env!`, `include_str!` and their kin) whose input has macro
//! calls is answered as blaze-expand asks ([`blaze_expand::Expanded::NeedsExpanded`]): each
//! nested call expanded as an eager input, sealed by its key, and the call made again with
//! the answers, to [`EAGER_DEPTH`] deep.

use blaze_expand::{
    BuiltinDerive, BuiltinFn, Call, CallSite, Ctx, DefAt, DefSite, DeriveFacts, Eager, ExpandCx, Expanded, Export, Inside, LocalInner,
    LexError, MacroDefinition, Mark, NestedKind, Prepared, ProcKind, Reads, StreamError, TokenKind, TokenStream, call_key, expand, expand_prepared, lex, prepare,
};
use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, Vec};
use blaze_surface::Edition;

use crate::driver::{BodyCall, BodyOut, CallInput, Expand, Expansion, LocalMacro};

/// How deep eager inputs nest (`concat!(concat!(..))`) before the call is refused.
pub const EAGER_DEPTH: u32 = 8;

/// The definition a macro call's path names.
pub enum Resolved<'d, 'a> {
    /// A definition: a builtin, a proc macro (its expansion reaches the [`ExpandCx`]'s
    /// `run_proc_macro`), or a `macro_rules!` or `macro` prepared at each call.
    Def(MacroDefinition<'a>),
    /// A `macro_rules!` or `macro` the caller prepared once.
    Prepared(&'d Prepared<'d>),
}

/// What a call's path names for `kind` (a function-like call, an attribute or a derive), as
/// the caller resolves it: [`crate::scoped::Scoped`] (blaze-scope's `Names::macro_definition`
/// in the body's module), or [`Lists`]. An error is a path that names no macro: the call is
/// [`Expansion::Unsupported`] and a body call's [`MacroFinding`], never looked up another way.
pub trait MacroNames<'d> {
    fn resolve<'a>(&self, path: &str, at: PathAt, kind: ProcKind, alloc: Alloc<'a>) -> Result<Resolved<'d, 'a>, NoMacro<'a>>
    where
        'd: 'a;

    /// Where a `macro_rules!` the body itself writes is defined: the body's crate, which its
    /// `$crate` names. `None` when the resolver does not know it: a call of such a macro is
    /// then [`NoMacro::LocalWithoutCrate`], a finding, never resolved another way.
    fn local_site(&self) -> Option<DefSite> {
        None
    }
}

/// Why a call's path names no macro.
#[derive(Clone, Copy, Debug)]
pub enum NoMacro<'a> {
    /// No macro of its last name in [`Lists`] (the stand-in with no names).
    NotListed,
    /// `$crate` written where no expansion's mark says which crate it is (the body's own text).
    UnmarkedDollarCrate,
    /// The path as written is not a path (an empty segment, `$crate` past the first segment).
    NotAPath,
    /// A `macro_rules!` of the body names it, and the resolver gives no crate to define it in.
    LocalWithoutCrate,
    /// The names library's answer: the path names nothing usable, a procedural macro the
    /// caller has no runner for, or a source that makes no definition of this shape.
    Names(blaze_scope::resolve::MacroDefError<'a>),
}

impl core::fmt::Display for NoMacro<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NoMacro::NotListed => f.write_str("no macro of this name in the stand-in lists"),
            NoMacro::UnmarkedDollarCrate => f.write_str("`$crate` outside an expansion"),
            NoMacro::NotAPath => f.write_str("not a path"),
            NoMacro::LocalWithoutCrate => f.write_str("a `macro_rules!` of the body, and the resolver names no crate for it"),
            NoMacro::Names(blaze_scope::resolve::MacroDefError::Path(e)) => write!(f, "{e}"),
            NoMacro::Names(blaze_scope::resolve::MacroDefError::NoSource { krate, .. }) => {
                write!(f, "a macro of crate {krate} with no source (procedural, or not read)")
            }
            NoMacro::Names(blaze_scope::resolve::MacroDefError::Refused(note)) => write!(f, "its source makes no definition: {note:?}"),
        }
    }
}

/// A body macro call that was not expanded, and is no error of the call itself: the call's
/// range (of the body's text: past the item's length, of an expansion appended to it), its
/// path as written and why.
#[derive(Debug)]
pub struct MacroFinding<'a> {
    pub at: blaze_surface::Range,
    pub path: AString<'a>,
    pub why: NotExpanded<'a>,
}

/// Why a body call has no expansion while the call itself is not in error.
#[derive(Debug)]
pub enum NotExpanded<'a> {
    /// Its path names no macro.
    NoMacro(NoMacro<'a>),
    /// It expanded, and its reads could not join the body's read list: an expansion whose
    /// reads are not kept would be a record valid past a change to what it read, so the call
    /// is not taken.
    ReadList(ReadLimit),
    /// Its path named a macro and expanding it failed: the expansion's own typed error.
    Failed(Failure<'a>),
}

/// Why a call whose path named a macro has no expansion: the call's own error or the
/// expander's stop, typed, so a body's finding names it (never a bare "failed").
#[derive(Debug)]
pub enum Failure<'a> {
    /// The expansion's first error ([`blaze_expand::Diagnostic::is_error`]), and the text that
    /// error names (its message, its expected token, its variable or path) from the
    /// expansion's notes.
    Diagnostic { diagnostic: blaze_expand::Diagnostic, text: AString<'a> },
    /// The expander stopped ([`blaze_expand::ExpandError`]).
    Expand(blaze_expand::ExpandError),
    /// The call's input, or a `macro_rules!` of the body, does not lex.
    Lex(LexError),
    /// The call's input could not be copied out of its stream.
    Stream(StreamError),
    /// The call's input has no delimiters around it.
    NoDelimiters,
    /// The call's input is in an expansion this body has not made.
    NoExpansion { expansion: u32 },
    /// An eager macro's input holds a call that is not function-like.
    EagerNotFnLike { kind: NestedKind },
    /// An eager macro asked for its input's calls again, or past [`EAGER_DEPTH`] eager inputs.
    EagerAgain { depth: u32 },
}

impl core::fmt::Display for Failure<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Failure::Diagnostic { diagnostic, text } => write!(f, "its expansion's error {}: `{text}`", diagnostic_name(diagnostic)),
            Failure::Expand(e) => write!(f, "the expander stopped: {e:?}"),
            Failure::Lex(e) => write!(f, "its input does not lex: {e:?}"),
            Failure::Stream(e) => write!(f, "its input could not be copied: {e}"),
            Failure::NoDelimiters => f.write_str("its input has no delimiters"),
            Failure::NoExpansion { expansion } => write!(f, "its input is in expansion {expansion}, which this body has not made"),
            Failure::EagerNotFnLike { kind } => write!(f, "an eager input holds a {kind:?} call"),
            Failure::EagerAgain { depth } => write!(f, "an eager macro asked for its input's calls again (eager depth {depth}, bound {EAGER_DEPTH})"),
        }
    }
}

/// A diagnostic's variant, as a name.
fn diagnostic_name(d: &blaze_expand::Diagnostic) -> &'static str {
    use blaze_expand::Diagnostic as D;
    match d {
        D::NoRuleMatched { .. } => "NoRuleMatched",
        D::Binding { .. } => "Binding",
        D::LeftoverTokens { .. } => "LeftoverTokens",
        D::LimitExceeded => "LimitExceeded",
        D::UnexpectedToken { .. } => "UnexpectedToken",
        D::BadDefinition { .. } => "BadDefinition",
        D::CompileError { .. } => "CompileError",
        D::EnvNotDefined { .. } => "EnvNotDefined",
        D::Include { why: blaze_expand::IncludeFailure::Outside, .. } => "Include (the path leaves the unit's source root: decision D60)",
        D::Include { .. } => "Include",
        D::Derive { .. } => "Derive",
        D::Builtin { .. } => "Builtin",
        D::ProcMacro { .. } => "ProcMacro",
        D::ProcMacroPanicked { .. } => "ProcMacroPanicked",
    }
}

/// The text a diagnostic names, from its expansion's `notes`.
fn diagnostic_text<'a>(d: &blaze_expand::Diagnostic, notes: &[u8], alloc: Alloc<'a>) -> AString<'a> {
    use blaze_expand::Diagnostic as D;
    let t = match *d {
        D::NoRuleMatched { expected, .. } => expected,
        D::Binding { message, .. }
        | D::BadDefinition { message }
        | D::CompileError { message }
        | D::Derive { message }
        | D::Builtin { message, .. }
        | D::ProcMacro { message, .. }
        | D::ProcMacroPanicked { message } => message,
        D::EnvNotDefined { name, .. } => name,
        D::Include { path, .. } => path,
        D::LeftoverTokens { .. } | D::LimitExceeded | D::UnexpectedToken { .. } => blaze_expand::Text::EMPTY,
    };
    let mut out = AString::new_in(alloc);
    // The expander writes its notes as whole `str`s; a range off them is named as such.
    match notes.get(t.at as usize..(t.at as usize).saturating_add(t.len as usize)).map(core::str::from_utf8) {
        Some(Ok(text)) => out.push_str(text),
        _ => out.push_str("(a range off the expansion's notes)"),
    }
    out
}

/// The bound a body's read list met.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadLimit {
    /// The list would pass `cap` reads ([`BODY_READ_CAP`], or [`ByName::with_read_cap`]'s).
    Cap(u32),
    /// The list's table would pass its encoding's bound.
    Table(StreamError),
}

/// The most reads one body's list holds ([`ByName::reads`]): far past any real body (a body
/// of tempo reads at most a few), and a bound on the record.
pub const BODY_READ_CAP: u32 = 1 << 16;

impl core::fmt::Display for NotExpanded<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NotExpanded::NoMacro(why) => write!(f, "{why}"),
            NotExpanded::ReadList(ReadLimit::Cap(cap)) => write!(f, "its reads would pass the body's read list's cap of {cap}"),
            NotExpanded::ReadList(ReadLimit::Table(e)) => write!(f, "its reads would pass the body's read list's bound: {e}"),
            NotExpanded::Failed(why) => write!(f, "{why}"),
        }
    }
}

/// Where a call's path is written: in the body's own text (`mark: None`, resolved in the body's
/// module), or in an expansion, whose tokens carry the mark of the macro that wrote them: its
/// crate is what `$crate` names, and an opaque mark (`macro`) resolves at its module.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PathAt {
    pub mark: Option<Mark>,
}

impl PathAt {
    /// The mark on the first token of the path that ends just before `open` (the call's `!`
    /// is at `open - 1`) in `stream`.
    fn before(stream: &TokenStream<'_>, open: u32) -> PathAt {
        let tokens = stream.tokens();
        let mut at = (open as usize).saturating_sub(1);
        let mut first = None;
        while at > 0 {
            at -= 1;
            match tokens[at].kind {
                TokenKind::Ident { .. } => first = Some(at),
                TokenKind::Punct { char: ':', .. } => {}
                _ => break,
            }
        }
        PathAt { mark: first.and_then(|it| stream.hygiene(tokens[it].ctx).mark) }
    }

    /// The first token of `start..end` (a nested call's path) in `stream`.
    fn of(stream: &TokenStream<'_>, start: u32) -> PathAt {
        let mark = stream.tokens().get(start as usize).and_then(|t| stream.hygiene(t.ctx).mark);
        PathAt { mark }
    }
}

/// The stand-in resolution by a path's last name: blaze-expand's builtins, then `rules` (the
/// last of a name winning), then `then` when given (a proc-macro resolver).
pub struct Lists<'d> {
    pub rules: &'d [(&'d str, Prepared<'d>)],
    /// Where a builtin is defined: `$crate` of its expansion names this crate.
    pub builtin_at: DefAt,
    pub then: Option<&'d dyn MacroNames<'d>>,
}

impl<'d> MacroNames<'d> for Lists<'d> {
    fn resolve<'a>(&self, path: &str, at: PathAt, kind: ProcKind, alloc: Alloc<'a>) -> Result<Resolved<'d, 'a>, NoMacro<'a>>
    where
        'd: 'a,
    {
        let name = last_name(path).ok_or(NoMacro::NotAPath)?;
        let rules = || self.rules.iter().rev().find(|(n, _)| *n == name).map(|(_, p)| Resolved::Prepared(p));
        let found = match kind {
            ProcKind::FnLike => match (0..=u8::MAX).map_while(BuiltinFn::from_byte).find(|it| it.name() == name) {
                Some(which) => Some(Resolved::Def(MacroDefinition::builtin_fn(which, self.builtin_at, alloc))),
                None => rules(),
            },
            ProcKind::Attr => rules(),
            ProcKind::Derive => (0..=u8::MAX)
                .map_while(BuiltinDerive::from_byte)
                .find(|it| it.name() == name)
                .map(|which| Resolved::Def(MacroDefinition::builtin_derive(which, self.builtin_at, alloc))),
        };
        match (found, self.then) {
            (Some(found), _) => Ok(found),
            (None, Some(then)) => then.resolve(path, at, kind, alloc),
            (None, None) => Err(NoMacro::NotListed),
        }
    }

    /// The stand-in has one crate: the builtins'.
    fn local_site(&self) -> Option<DefSite> {
        Some(self.builtin_at.site)
    }
}

/// Where [`ByName`] resolves: its own [`Lists`], or the caller's resolver.
enum Names<'d> {
    Lists(Lists<'d>),
    Caller(&'d dyn MacroNames<'d>),
}

/// The expander: see the module's doc.
pub struct ByName<'d, 'a, C> {
    names: Names<'d>,
    cx: C,
    /// This body's expansions, by [`CallInput::Expanded`]'s `expansion`: each one's key and
    /// tokens, while the body is lowered.
    expansions: AVec<'a, (ContentId, TokenStream<'a>)>,
    /// This body's calls whose path named no macro.
    findings: AVec<'a, MacroFinding<'a>>,
    /// This body's calls whose expansion joined reads to `reads`, each with its span
    /// ([`crate::call_reads::CallReads`]): where a proc macro's read is placed (D111).
    call_reads: AVec<'a, crate::call_reads::CallReads<'a>>,
    /// This body's read list: every outside read its expansions made (an `env!` name and
    /// value, an `include_str!`/`include_bytes!`/`include!` path and content digest), in call
    /// order, eager inputs' included. The body's record keeps it: a changed answer reruns
    /// exactly the bodies that read it.
    reads: Reads<'a>,
    /// The most reads `reads` takes ([`BODY_READ_CAP`] unless [`ByName::with_read_cap`]).
    read_cap: u32,
    alloc: Alloc<'a>,
}

/// Why a call has no expansion.
enum Refused<'a> {
    /// No macro of the name.
    Unknown(NoMacro<'a>),
    /// Its reads could not join the body's read list.
    ReadList(ReadLimit),
    /// The expansion is an error, or eager inputs nest too deep.
    Failed(Failure<'a>),
}

impl<'d: 'a, 'a, C: ExpandCx> ByName<'d, 'a, C> {
    /// Resolving by last name over blaze-expand's builtins and `rules` ([`Lists`]).
    pub fn new(rules: &'d [(&'d str, Prepared<'d>)], builtin_at: DefAt, cx: C, alloc: Alloc<'a>) -> Self {
        let names = Names::Lists(Lists { rules, builtin_at, then: None });
        Self::over(names, cx, alloc)
    }

    /// Resolving every call's path through `names` alone.
    pub fn with_names(names: &'d dyn MacroNames<'d>, cx: C, alloc: Alloc<'a>) -> Self {
        Self::over(Names::Caller(names), cx, alloc)
    }

    fn over(names: Names<'d>, cx: C, alloc: Alloc<'a>) -> Self {
        ByName {
            names,
            cx,
            expansions: Vec::new_in(alloc),
            findings: Vec::new_in(alloc),
            call_reads: Vec::new_in(alloc),
            reads: Reads::new_in(alloc),
            read_cap: BODY_READ_CAP,
            alloc,
        }
    }

    /// The same expander with a body's read list bounded at `cap` reads: a call whose reads
    /// would pass it is a [`NotExpanded::ReadList`] finding.
    #[must_use]
    pub fn with_read_cap(mut self, cap: u32) -> Self {
        self.read_cap = cap;
        self
    }

    /// Joins an expansion's reads to the body's list, or names the bound it would pass.
    fn join_reads(&mut self, reads: &Reads<'_>) -> Result<(), ReadLimit> {
        if self.reads.list().len() + reads.list().len() > self.read_cap as usize {
            return Err(ReadLimit::Cap(self.read_cap));
        }
        self.reads.extend(reads).map_err(ReadLimit::Table)
    }

    /// The body calls of the body lowered last whose path named no macro, in source order.
    pub fn findings(&self) -> &[MacroFinding<'a>] {
        &self.findings
    }

    /// The read list of the body lowered last ([`ByName::reads`]'s field doc).
    pub fn reads(&self) -> &Reads<'a> {
        &self.reads
    }

    /// The calls of the body lowered last whose expansion joined reads, in call order, each
    /// with its span of [`ByName::reads`] (blaze-ladder front2 `body_left` files D111 from them).
    pub fn call_reads(&self) -> &[crate::call_reads::CallReads<'a>] {
        &self.call_reads
    }

    /// Keeps the span of the read list the call at `at` (written `path`) joined since `before`
    /// reads, when it joined any.
    fn note_reads(&mut self, at: Option<blaze_surface::Range>, path: &str, shape: blaze_surface::CallShape, before: usize) {
        let after = self.reads.list().len();
        if after > before {
            let mut written = AString::new_in(self.alloc);
            written.push_str(path);
            self.call_reads.push(crate::call_reads::CallReads { at, path: written, shape, reads: before..after });
        }
    }

    fn resolve(&self, path: &str, at: PathAt, kind: ProcKind) -> Result<Resolved<'d, 'a>, NoMacro<'a>> {
        match &self.names {
            Names::Lists(lists) => lists.resolve(path, at, kind, self.alloc),
            Names::Caller(names) => names.resolve(path, at, kind, self.alloc),
        }
    }

    /// The `macro_rules!` of the body a single-name `path` names (the last written of the
    /// name in textual scope), as a definition in the body's crate at `edition`.
    fn local(&self, path: &str, locals: &[LocalMacro<'_>], edition: Edition) -> Option<Result<MacroDefinition<'a>, Refused<'a>>> {
        let name = path.trim();
        let written = locals.iter().rev().find(|l| l.name == name)?;
        let site = match &self.names {
            Names::Lists(lists) => lists.local_site(),
            Names::Caller(names) => names.local_site(),
        };
        let Some(site) = site else { return Some(Err(Refused::Unknown(NoMacro::LocalWithoutCrate))) };
        let at = DefAt { site, edition, export: Export::Local, local_inner: LocalInner::No, transparency: None };
        Some(lex(written.rules, edition, self.alloc).map(|body| MacroDefinition::rules(body, at)).map_err(|e| Refused::Failed(Failure::Lex(e))))
    }

    /// Where the path of the call whose input is `input` is written.
    fn path_at(&self, input: CallInput<'_>) -> PathAt {
        match input {
            CallInput::Text(_) => PathAt::default(),
            CallInput::Expanded { expansion, start, .. } => {
                self.expansions.get(expansion as usize).map_or(PathAt::default(), |(_, tokens)| PathAt::before(tokens, start))
            }
        }
    }

    /// The tokens inside `call`'s delimiters.
    fn input(&self, input: CallInput<'_>, edition: Edition) -> Result<TokenStream<'a>, Refused<'a>> {
        match input {
            CallInput::Text(text) => {
                let lexed = lex(text, edition, self.alloc).map_err(|e| Refused::Failed(Failure::Lex(e)))?;
                let len = lexed.len();
                if len < 2 {
                    return Err(Refused::Failed(Failure::NoDelimiters));
                }
                lexed.copy_in(1..len - 1, self.alloc).map_err(|e| Refused::Failed(Failure::Stream(e)))
            }
            CallInput::Expanded { expansion, start, end } => {
                let (key, tokens) =
                    self.expansions.get(expansion as usize).ok_or(Refused::Failed(Failure::NoExpansion { expansion }))?;
                if end < start + 2 {
                    return Err(Refused::Failed(Failure::NoDelimiters));
                }
                let mut copy = tokens
                    .copy_in(start as usize + 1..end as usize - 1, self.alloc)
                    .map_err(|e| Refused::Failed(Failure::Stream(e)))?;
                copy.seal(*key);
                Ok(copy)
            }
        }
    }

    /// Expands `name!(input)`: its key and its expansion's tokens.
    fn call(
        &mut self,
        path: &str,
        at: PathAt,
        input: &TokenStream<'_>,
        edition: Edition,
        inside: Inside,
        depth: u32,
        locals: &[LocalMacro<'_>],
        mut cfg: Option<&mut (dyn crate::Cfg + '_)>,
    ) -> Result<(ContentId, TokenStream<'a>), Refused<'a>> {
        // `panic!` takes the edition of the code that writes it: a path a macro wrote carries
        // its definition's mark, whose edition is the defining crate's (the token was read in
        // it). Gap: rustc's `use_panic_2021` skips macros under
        // `#[allow_internal_unstable(edition_panic)]` (core's `debug_assert!`) to their caller;
        // `DefAt` does not record that flag yet, so such a macro's own edition is taken.
        let panic_edition = at.mark.map_or(edition, |mark| mark.edition);
        let site = CallSite { edition, panic_edition, ctx: Ctx::root(edition) };
        let resolved = match self.local(path, locals, edition) {
            Some(def) => Resolved::Def(def?),
            None => self.resolve(path, at, ProcKind::FnLike).map_err(Refused::Unknown)?,
        };
        let (def, prepared) = match &resolved {
            Resolved::Def(def) => (def, None),
            Resolved::Prepared(prepared) => (prepared.def(), Some(*prepared)),
        };
        let mut answers: AVec<'a, TokenStream<'a>> = Vec::new_in(self.alloc);
        loop {
            let refs: AVec<'_, &TokenStream<'_>> = {
                let mut refs = Vec::with_capacity_in(answers.len(), self.alloc);
                refs.extend(answers.iter());
                refs
            };
            let call = Call {
                kind: blaze_expand::CallKind::FnLike { args: input },
                site,
                eager: Eager { inside, answers: &refs },
            };
            let key = call_key(def, &call).0;
            let result = match cfg.as_deref_mut() {
                Some(cfg) => {
                    let cx = BodyCx { host: &self.cx, cfg: core::cell::RefCell::new(cfg), alloc: self.alloc };
                    expand_with(def, prepared, &call, &cx, self.alloc)
                }
                None => expand_with(def, prepared, &call, &self.cx, self.alloc),
            };
            match result {
                // An expansion's reads join the body's list whether it succeeded or ended in
                // the call's error: a missing `include_str!` file or an unset `env!` is an
                // answer the error depends on (creating the file reruns the body).
                Ok(Expanded::Expansion(exp)) => {
                    self.join_reads(&exp.reads).map_err(Refused::ReadList)?;
                    if let Some(diagnostic) = exp.diagnostics.iter().find(|d| d.is_error()) {
                        let text = diagnostic_text(diagnostic, &exp.notes, self.alloc);
                        return Err(Refused::Failed(Failure::Diagnostic { diagnostic: *diagnostic, text }));
                    }
                    return Ok((key, exp.tokens));
                }
                Ok(Expanded::NeedsExpanded(nested)) if answers.is_empty() && depth < EAGER_DEPTH => {
                    for n in nested.iter() {
                        if n.kind != NestedKind::FnLike {
                            return Err(Refused::Failed(Failure::EagerNotFnLike { kind: n.kind }));
                        }
                        let path = path_text(input, n.path.start, n.path.end, self.alloc);
                        let inner = n.inner();
                        let args = input
                            .copy_in(inner.start as usize..inner.end as usize, self.alloc)
                            .map_err(|e| Refused::Failed(Failure::Stream(e)))?;
                        let at = PathAt::of(input, n.path.start);
                        let (key, mut tokens) =
                            self.call(&path, at, &args, edition, Inside::EagerInput, depth + 1, locals, cfg.as_deref_mut())?;
                        tokens.seal(key);
                        answers.push(tokens);
                    }
                }
                Ok(Expanded::NeedsExpanded(_)) => return Err(Refused::Failed(Failure::EagerAgain { depth })),
                Err(e) => return Err(Refused::Failed(Failure::Expand(e))),
            }
        }
    }
}

/// [`expand_prepared`] of a definition prepared once, else [`expand`].
fn expand_with<'a>(
    def: &MacroDefinition<'_>,
    prepared: Option<&Prepared<'_>>,
    call: &Call<'_>,
    cx: &impl ExpandCx,
    alloc: Alloc<'a>,
) -> Result<Expanded<'a>, blaze_expand::ExpandError> {
    match prepared {
        Some(prepared) => expand_prepared(prepared, call, cx, alloc),
        None => expand(def, call, cx, alloc),
    }
}

/// The host of one body call: its `cfg!` answered by the set the body is lowered under
/// ([`crate::cfg_outcome`]), the one answer its `#[cfg]`s get too; every other read is the
/// caller's host's. A host answering `cfg!` on its own could disagree with the body's
/// `#[cfg]`s, or refuse what the body's set answers.
struct BodyCx<'x, 'c, 'm, C> {
    host: &'x C,
    cfg: core::cell::RefCell<&'x mut (dyn crate::Cfg + 'c)>,
    /// The predicate's scratch.
    alloc: Alloc<'m>,
}

impl<C: ExpandCx> ExpandCx for BodyCx<'_, '_, '_, C> {
    fn env(&self, name: &str) -> Result<Option<&str>, blaze_expand::HostError> {
        self.host.env(name)
    }

    fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, blaze_expand::HostError> {
        crate::cfg_outcome(predicate, &mut **self.cfg.borrow_mut(), self.alloc)
    }

    fn include(&self, path: &str, how: blaze_expand::IncludeAs) -> Result<blaze_expand::Included<'_>, blaze_expand::HostError> {
        self.host.include(path, how)
    }

    fn place(&self, what: blaze_expand::PlaceOf) -> Result<blaze_expand::Place<'_>, blaze_expand::HostError> {
        self.host.place(what)
    }

    fn run_proc_macro<'b>(
        &self,
        def: &MacroDefinition<'_>,
        call: blaze_expand::ProcCall<'_>,
        alloc: Alloc<'b>,
    ) -> Result<blaze_expand::Produced<'b>, blaze_expand::HostError> {
        self.host.run_proc_macro(def, call, alloc)
    }
}

/// The path `stream`'s tokens `start..end` write, as source text (`a::b`).
fn path_text<'a>(stream: &TokenStream<'_>, start: u32, end: u32, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::new_in(alloc);
    for at in start..end {
        match stream.tokens()[at as usize].kind {
            TokenKind::Ident { .. } => out.push_str(stream.text_at(at as usize)),
            _ => out.push(':'),
        }
    }
    out
}

/// The last name of a path as written (`a::b`, or in an expansion `a : : b`).
fn last_name(path: &str) -> Option<&str> {
    path.rsplit(|c: char| !(c.is_alphanumeric() || c == '_')).find(|it| !it.is_empty())
}

impl<'d: 'a, 'a, C: ExpandCx> Expand for ByName<'d, 'a, C> {
    fn expand(&mut self, path: &str, input: &str, edition: Edition, cfg: &mut dyn crate::Cfg, out: &mut BodyOut<'_>) -> Expansion {
        let result = self
            .input(CallInput::Text(input), edition)
            .and_then(|input| self.call(path, PathAt::default(), &input, edition, Inside::Plain, 0, &[], Some(cfg)));
        match result {
            Ok((key, tokens)) => {
                out.key = key;
                out.words = blaze_expand::body_words(&tokens, key, *out.words.words.allocator());
                Expansion::Expanded
            }
            Err(Refused::Unknown(_)) => Expansion::Unsupported,
            // A text call has no range to report at: the caller sees the failure.
            Err(Refused::ReadList(_) | Refused::Failed(_)) => Expansion::Failed,
        }
    }

    fn expand_body(&mut self, call: BodyCall<'_>, cfg: &mut dyn crate::Cfg, out: &mut BodyOut<'_>) -> Expansion {
        let at = self.path_at(call.input);
        let before = self.reads.list().len();
        let result = self
            .input(call.input, call.edition)
            .and_then(|input| self.call(call.path, at, &input, call.edition, Inside::Plain, 0, call.locals, Some(cfg)));
        // Its reads (an eager input's nested calls' included) joined on success and on the
        // call's error alike (`call`): placed at this call.
        self.note_reads(Some(call.range), call.path, blaze_surface::CallShape::FnLike, before);
        match result {
            Ok((key, tokens)) => {
                out.key = key;
                out.words = blaze_expand::body_words(&tokens, key, *out.words.words.allocator());
                self.expansions.push((key, tokens));
                Expansion::Expanded
            }
            Err(Refused::Unknown(why)) => {
                let mut path = AString::new_in(self.alloc);
                path.push_str(call.path);
                self.findings.push(MacroFinding { at: call.range, path, why: NotExpanded::NoMacro(why) });
                Expansion::Unsupported
            }
            Err(Refused::ReadList(limit)) => {
                let mut path = AString::new_in(self.alloc);
                path.push_str(call.path);
                self.findings.push(MacroFinding { at: call.range, path, why: NotExpanded::ReadList(limit) });
                Expansion::Failed
            }
            Err(Refused::Failed(why)) => {
                let mut path = AString::new_in(self.alloc);
                path.push_str(call.path);
                self.findings.push(MacroFinding { at: call.range, path, why: NotExpanded::Failed(why) });
                Expansion::Failed
            }
        }
    }

    fn begin_body(&mut self) {
        self.expansions.clear();
        self.findings.clear();
        self.call_reads.clear();
        self.reads = Reads::new_in(self.alloc);
    }

    /// An attribute macro (a `macro_rules!` `attr(..)` arm, as `#[bstd::main]`, or a proc
    /// macro); a path that names none is no attribute macro (`#[inline]`, `#[allow(..)]`).
    fn attr(&mut self, path: &str, args: Option<&TokenStream<'_>>, item: &TokenStream<'_>, edition: Edition, out: &mut BodyOut<'_>) -> Expansion {
        let Ok(resolved) = self.resolve(path, PathAt::default(), ProcKind::Attr) else { return Expansion::Unsupported };
        let def;
        let own;
        let prepared = match resolved {
            Resolved::Prepared(prepared) => prepared,
            Resolved::Def(d) => {
                def = d;
                let Ok(p) = prepare(&def) else { return Expansion::Failed };
                own = p;
                &own
            }
        };
        let site = CallSite { edition, panic_edition: edition, ctx: Ctx::root(edition) };
        let call = Call::attr(args, item, site);
        let key = call_key(prepared.def(), &call).0;
        let before = self.reads.list().len();
        let outcome = match expand_prepared(prepared, &call, &self.cx, self.alloc) {
            // Reads join the list on an error too (see `call`).
            Ok(Expanded::Expansion(exp)) => {
                if self.join_reads(&exp.reads).is_err() || exp.diagnostics.iter().any(blaze_expand::Diagnostic::is_error) {
                    Expansion::Failed
                } else {
                    out.key = key;
                    out.words = blaze_expand::body_words(&exp.tokens, key, *out.words.words.allocator());
                    Expansion::Expanded
                }
            }
            _ => Expansion::Failed,
        };
        self.note_reads(None, path, blaze_surface::CallShape::Attr, before);
        outcome
    }

    /// A derive: the language's (`Clone`, `Copy`, `Debug`, `Default`, `PartialEq`, `Eq`,
    /// `PartialOrd`, `Ord`, `Hash`, `ConstParamTy`, written by blaze-expand's `derive` as the
    /// Reference specifies them), or a proc macro's.
    fn derive(&mut self, path: &str, item: &TokenStream<'_>, facts: DeriveFacts, edition: Edition, out: &mut BodyOut<'_>) -> Expansion {
        let def = match self.resolve(path, PathAt::default(), ProcKind::Derive) {
            Ok(Resolved::Def(def)) => def,
            // A `macro_rules!` is no derive.
            Ok(Resolved::Prepared(_)) => return Expansion::Failed,
            Err(_) => return Expansion::Unsupported,
        };
        let site = CallSite { edition, panic_edition: edition, ctx: Ctx::root(edition) };
        let call = Call::derive(item, facts, site);
        let key = call_key(&def, &call).0;
        let before = self.reads.list().len();
        let outcome = match expand(&def, &call, &self.cx, self.alloc) {
            // Reads join the list on an error too (see `call`).
            Ok(Expanded::Expansion(exp)) => {
                if self.join_reads(&exp.reads).is_err() || exp.diagnostics.iter().any(blaze_expand::Diagnostic::is_error) {
                    Expansion::Failed
                } else {
                    out.key = key;
                    out.words = blaze_expand::body_words(&exp.tokens, key, *out.words.words.allocator());
                    Expansion::Expanded
                }
            }
            _ => Expansion::Failed,
        };
        self.note_reads(None, path, blaze_surface::CallShape::Derive, before);
        outcome
    }
}
