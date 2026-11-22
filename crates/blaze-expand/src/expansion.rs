//! What one macro call makes: [`Expansion`], a portable value written as one record.

use core::fmt;

use blaze_own::{AVec, Alloc, Vec};

use blaze_id::{ContentId, Portable};

use crate::{
    cx::{ExpandCx, HostError, Place, PlaceOf},
    error::{StreamError, Table},
    tokens::{Edition, Sink, Text, TokenHygiene, TokenStream, Transparency, lex, push_text},
};

/// Where a token of an expansion came from: an index into one of the call's or the
/// definition's streams, or made by the expansion. Never an offset (rule 71): the reader maps
/// an index to a place through the stream it placed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TokenOrigin {
    /// The token at this index of the call's input ([`crate::Call::input`]).
    Input(u32),
    /// Of the attribute's arguments.
    AttrArgs(u32),
    /// Of the definition's body.
    Def(u32),
    /// Of a `macro m(params)` definition's parameters.
    Params(u32),
    /// Made by the expansion, with this transparency.
    Made(Transparency),
}

impl TokenOrigin {
    fn encode<S: Sink>(self, out: &mut S) {
        let (tag, value) = match self {
            TokenOrigin::Input(at) => (0, at),
            TokenOrigin::AttrArgs(at) => (1, at),
            TokenOrigin::Def(at) => (2, at),
            TokenOrigin::Params(at) => (3, at),
            TokenOrigin::Made(transparency) => (4, u32::from(transparency.byte())),
        };
        out.put(&[tag]);
        out.put(&value.to_be_bytes());
    }
}

/// Tokens `start..end` of a stream: the schema's one `Range`, here counting tokens.
pub use blaze_surface::Range;

/// A range's canonical bytes, in a key or a content hash.
fn encode_range<S: Sink>(range: Range, out: &mut S) {
    out.put(&range.start.to_be_bytes());
    out.put(&range.end.to_be_bytes());
}

/// What a nested macro call is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NestedKind {
    /// `path!(..)`, `path![..]` or `path! {..}`.
    FnLike,
    /// `macro_rules! name {..}`: a definition, its body a template not scanned.
    MacroRules,
    /// `macro name(..) {..}` or `macro name {..}`.
    Macro2,
}

/// How a nested call's path resolves.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PathScope {
    /// As written, by its tokens' hygiene.
    AsWritten,
    /// As `$crate::name!`: a single-name call the body of a `local_inner_macros` macro wrote.
    DefCrate,
}

/// A macro call in a stream, not expanded: listed for the caller, which resolves its path and
/// expands it as a call of its own.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NestedCall {
    pub kind: NestedKind,
    /// The path's tokens (for a definition, the `macro_rules` or `macro` keyword).
    pub path: Range,
    /// For a definition, its name's token.
    pub name: Option<u32>,
    /// The group of the arguments or the body, delimiters included; for a `macro m(..) {..}`
    /// the parameters' group is `params`.
    pub args: Range,
    pub params: Option<Range>,
    pub scope: PathScope,
}

impl NestedCall {
    /// The tokens inside the arguments' delimiters.
    pub fn inner(&self) -> Range {
        Range { start: self.args.start + 1, end: self.args.end - 1 }
    }

    fn encode<S: Sink>(self, out: &mut S) {
        let kind = match self.kind {
            NestedKind::FnLike => 0,
            NestedKind::MacroRules => 1,
            NestedKind::Macro2 => 2,
        };
        let scope = match self.scope {
            PathScope::AsWritten => 0,
            PathScope::DefCrate => 1,
        };
        out.put(&[kind, scope]);
        encode_range(self.path, out);
        out.put(&self.name.map_or(u32::MAX, |it| it).to_be_bytes());
        encode_range(self.args, out);
        match self.params {
            None => out.put(&[0]),
            Some(params) => {
                out.put(&[1]);
                encode_range(params, out);
            }
        }
    }
}

/// A position the reader fills when it places the expansion.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum HoleOf {
    /// `line!()`: the `0u32` token becomes the line of the outermost call.
    Line,
    /// `column!()`: its column.
    Column,
    /// `file!()`: the `""` token becomes the path of its file.
    File,
    /// `module_path!()`: the `""` token becomes the path of the module the call is in.
    ModulePath,
}

/// A placeholder token of the expansion and what fills it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Hole {
    pub token: u32,
    pub of: HoleOf,
}

/// What kind of outside fact was read.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ReadKind {
    /// An env var by name.
    Env,
    /// One atom of a cfg predicate (`unix`, `feature = "x"`), its subject the atom's text: a
    /// predicate records one read per atom it evaluated, in evaluation order, never its whole
    /// text (cx.rs `Recorder::cfg`; D152 and D158 read these as cfg cells).
    Cfg,
    /// An included file by the path written, as text.
    IncludeStr,
    /// An included file, as bytes.
    IncludeBytes,
    /// A line inside eager input.
    Line,
    /// A column inside eager input.
    Column,
    /// The file's path inside eager input.
    File,
    /// The module's path inside eager input.
    ModulePath,
    /// A read a proc macro's run made, its subject the sandbox's own name for it.
    ProcMacro,
}

/// What the read was told.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ReadAnswer {
    /// Nothing there: no such variable or file.
    Absent,
    /// The digest of the value.
    Digest(ContentId),
    Bool(bool),
    Number(u32),
}

/// One outside read.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Read {
    pub kind: ReadKind,
    /// What was asked: a name, a path or a predicate's text, a range of the reads' text.
    pub subject: Text,
    pub answer: ReadAnswer,
}

/// An expansion's reads, in the order they were made, with the text of their subjects.
#[derive(Clone, Debug)]
pub struct Reads<'a> {
    list: AVec<'a, Read>,
    text: AVec<'a, u8>,
}

impl<'a> Reads<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Reads<'a> {
        Reads { list: Vec::new_in(alloc), text: Vec::new_in(alloc) }
    }

    pub fn push(
        &mut self,
        kind: ReadKind,
        subject: &str,
        answer: ReadAnswer,
    ) -> Result<(), StreamError> {
        u32::try_from(self.list.len() + 1)
            .map_err(|_| StreamError::TooLarge { table: Table::Reads })?;
        let subject = push_text(&mut self.text, subject, Table::Reads)?;
        self.list.push(Read { kind, subject, answer });
        Ok(())
    }

    /// Appends `other`'s reads.
    pub fn extend(&mut self, other: &Reads<'_>) -> Result<(), StreamError> {
        for read in other.list() {
            self.push(read.kind, other.subject(read), read.answer)?;
        }
        Ok(())
    }

    pub fn list(&self) -> &[Read] {
        &self.list
    }

    pub fn subject(&self, read: &Read) -> &str {
        read.subject.of(&self.text)
    }

    /// The reads' canonical bytes.
    pub fn encode<S: Sink>(&self, out: &mut S) {
        out.put(&crate::tokens::index(self.list.len()).to_be_bytes());
        for read in &self.list {
            let kind = match read.kind {
                ReadKind::Env => 0,
                ReadKind::Cfg => 1,
                ReadKind::IncludeStr => 2,
                ReadKind::IncludeBytes => 3,
                ReadKind::Line => 4,
                ReadKind::Column => 5,
                ReadKind::File => 6,
                ReadKind::ModulePath => 7,
                ReadKind::ProcMacro => 8,
            };
            out.put(&[kind]);
            let subject = self.subject(read);
            out.put(&crate::tokens::index(subject.len()).to_be_bytes());
            out.put(subject.as_bytes());
            match read.answer {
                ReadAnswer::Absent => out.put(&[0]),
                ReadAnswer::Digest(digest) => {
                    out.put(&[1]);
                    out.put(&digest.0);
                }
                ReadAnswer::Bool(value) => out.put(&[2, u8::from(value)]),
                ReadAnswer::Number(value) => {
                    out.put(&[3]);
                    out.put(&value.to_be_bytes());
                }
            }
        }
    }
}

impl ReadAnswer {
    /// What a read of the environment variable whose value is `value` records.
    pub(crate) fn of_env(value: Option<&str>) -> ReadAnswer {
        match value {
            Some(value) => ReadAnswer::Digest(ContentId::of("blaze v2 expand env value 1", value.as_bytes())),
            None => ReadAnswer::Absent,
        }
    }

    /// What a read of a place answered `place` records.
    pub(crate) fn of_place(place: Place<'_>) -> ReadAnswer {
        match place {
            Place::Number(value) => ReadAnswer::Number(value),
            Place::Text(value) => ReadAnswer::Digest(ContentId::of("blaze v2 expand place 1", value.as_bytes())),
        }
    }
}

/// What asking a read list again found ([`Reads::hold`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Held {
    /// Every read is answered today as it was.
    Holds,
    /// The read at `read` is answered otherwise today.
    Moved { read: usize },
    /// The read at `read` cannot be asked again by its subject (an included file is named by
    /// where it is in the root, not by the path the call wrote; a proc macro's read is its
    /// sandbox's): the expansion is not known unchanged, and is not kept ([`Reads::askable`]).
    NotAskable { read: usize, kind: ReadKind },
}

impl Reads<'_> {
    /// Whether every read can be asked again by [`Reads::hold`]: else the first that cannot.
    pub fn askable(&self) -> Result<(), (usize, ReadKind)> {
        for (read, it) in self.list.iter().enumerate() {
            match it.kind {
                ReadKind::Env | ReadKind::Cfg | ReadKind::Line | ReadKind::Column | ReadKind::File | ReadKind::ModulePath => {}
                kind @ (ReadKind::IncludeStr | ReadKind::IncludeBytes | ReadKind::ProcMacro) => return Err((read, kind)),
            }
        }
        Ok(())
    }

    /// Asks `cx` each read again, in the order they were made, and stops at the first that is
    /// answered otherwise: a kept expansion is today's only when this says [`Held::Holds`]. A
    /// cfg read's subject is its predicate's text, lexed again in `edition` (the call's).
    pub fn hold(&self, cx: &impl ExpandCx, edition: Edition, alloc: Alloc<'_>) -> Result<Held, HostError> {
        for (read, it) in self.list.iter().enumerate() {
            let subject = self.subject(it);
            let place = |what: PlaceOf| cx.place(what).map(ReadAnswer::of_place);
            let today = match it.kind {
                ReadKind::Env => ReadAnswer::of_env(cx.env(subject)?),
                ReadKind::Cfg => match lex(subject, edition, alloc) {
                    Ok(predicate) => ReadAnswer::Bool(cx.cfg(&predicate)?),
                    Err(_) => return Ok(Held::NotAskable { read, kind: it.kind }),
                },
                ReadKind::Line => place(PlaceOf::Line)?,
                ReadKind::Column => place(PlaceOf::Column)?,
                ReadKind::File => place(PlaceOf::File)?,
                ReadKind::ModulePath => place(PlaceOf::ModulePath)?,
                kind @ (ReadKind::IncludeStr | ReadKind::IncludeBytes | ReadKind::ProcMacro) => {
                    return Ok(Held::NotAskable { read, kind });
                }
            };
            if today != it.answer {
                return Ok(Held::Moved { read });
            }
        }
        Ok(Held::Holds)
    }
}

impl<'b> PartialEq<Reads<'b>> for Reads<'_> {
    fn eq(&self, other: &Reads<'b>) -> bool {
        self.list[..] == other.list[..] && self.text[..] == other.text[..]
    }
}

/// Why an include failed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum IncludeFailure {
    Missing,
    /// `include_str!` or `include!` of a file that is not UTF-8.
    NotUtf8,
    /// `include!` of a file that does not lex.
    DoesNotLex,
    /// A path that leaves the unit's source root (decision D60: [`crate::include_target`]).
    Outside,
}

impl IncludeFailure {
    fn byte(self) -> u8 {
        match self {
            IncludeFailure::Missing => 0,
            IncludeFailure::NotUtf8 => 1,
            IncludeFailure::DoesNotLex => 2,
            IncludeFailure::Outside => 3,
        }
    }
}

/// A macro call's own error, a value of its expansion: the record keeps it, and a reader
/// reports it where it places the call. An expansion with an error ([`Diagnostic::is_error`])
/// has no tokens.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Diagnostic {
    /// No rule of the `rules` rules matched. `closest` is the rule that got furthest (its index
    /// in definition order; the first of equals; `None` where the matcher kept no tracker),
    /// `token` the token it stopped at (`None` at the end of the input), `expected` what that
    /// rule wanted there, both as rustc words them; `at` where.
    NoRuleMatched { rules: u32, closest: Option<u32>, token: Option<Text>, expected: Text, at: Option<TokenOrigin> },
    /// A rule matched and its transcription failed (a binding used at the wrong depth).
    Binding { message: Text, at: Option<TokenOrigin> },
    /// A rule left input unmatched.
    LeftoverTokens { at: Option<TokenOrigin> },
    /// The matcher's own bound was hit.
    LimitExceeded,
    /// An unexpected token in a rule.
    UnexpectedToken { at: Option<TokenOrigin> },
    /// The definition's rules do not parse.
    BadDefinition { message: Text },
    /// `compile_error!(message)`.
    CompileError { message: Text },
    /// `env!(name)` of a variable not set; `message` its second argument.
    EnvNotDefined { name: Text, message: Option<Text> },
    /// `include_str!` or `include_bytes!` of `path` failed.
    Include { path: Text, why: IncludeFailure },
    /// A builtin derive refused the item.
    Derive { message: Text },
    /// A builtin macro's arguments are not what it takes.
    Builtin { message: Text, at: Option<TokenOrigin> },
    /// A proc macro's own diagnostic, `proc_macro::Diagnostic::emit`: its level, its message,
    /// and the token its first span names.
    ProcMacro { level: ProcLevel, message: Text, at: Option<TokenOrigin> },
    /// The proc macro panicked; its panic message.
    ProcMacroPanicked { message: Text },
}

impl Diagnostic {
    /// Whether the call failed: an expansion with an error has no tokens; a proc macro's
    /// warning, note or help sits beside its tokens.
    pub fn is_error(&self) -> bool {
        !matches!(self, Diagnostic::ProcMacro { level: ProcLevel::Warning | ProcLevel::Note | ProcLevel::Help, .. })
    }
}

/// A proc macro diagnostic's level, as `proc_macro::Level` has them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ProcLevel {
    Error,
    Warning,
    Note,
    Help,
}

impl ProcLevel {
    pub(crate) fn byte(self) -> u8 {
        match self {
            ProcLevel::Error => 0,
            ProcLevel::Warning => 1,
            ProcLevel::Note => 2,
            ProcLevel::Help => 3,
        }
    }

    pub(crate) fn from_byte(byte: u8) -> Option<ProcLevel> {
        Some(match byte {
            0 => ProcLevel::Error,
            1 => ProcLevel::Warning,
            2 => ProcLevel::Note,
            3 => ProcLevel::Help,
            _ => return None,
        })
    }
}

/// One macro call's expansion.
#[derive(Clone, Debug)]
pub struct Expansion<'a> {
    /// The tokens, position-independent.
    pub tokens: TokenStream<'a>,
    /// Each token's origin, a column parallel to `tokens`.
    pub origins: AVec<'a, TokenOrigin>,
    /// The macro calls in `tokens`, not expanded.
    pub nested: AVec<'a, NestedCall>,
    /// The placeholders the reader fills.
    pub holes: AVec<'a, Hole>,
    pub reads: Reads<'a>,
    pub diagnostics: AVec<'a, Diagnostic>,
    /// The text the diagnostics' [`Text`]s are ranges of.
    pub notes: AVec<'a, u8>,
    /// The rule that matched, for a declarative macro.
    pub arm: Option<u32>,
}

impl<'a> Expansion<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Expansion<'a> {
        Expansion {
            tokens: TokenStream::new_in(alloc),
            origins: Vec::new_in(alloc),
            nested: Vec::new_in(alloc),
            holes: Vec::new_in(alloc),
            reads: Reads::new_in(alloc),
            diagnostics: Vec::new_in(alloc),
            notes: Vec::new_in(alloc),
            arm: None,
        }
    }

    pub(crate) fn note(&mut self, text: &str) -> Result<Text, StreamError> {
        push_text(&mut self.notes, text, Table::Notes)
    }

    /// A note of what `value` displays, written in place.
    pub(crate) fn note_display(&mut self, value: &impl fmt::Display) -> Result<Text, StreamError> {
        struct To<'v, 'a>(&'v mut AVec<'a, u8>);
        impl fmt::Write for To<'_, '_> {
            fn write_str(&mut self, text: &str) -> fmt::Result {
                self.0.extend_from_slice(text.as_bytes());
                Ok(())
            }
        }
        let start = self.notes.len();
        let mut to = To(&mut self.notes);
        fmt::Write::write_fmt(&mut to, format_args!("{value}"))
            .expect("writing to a buffer does not fail");
        let too_large = StreamError::TooLarge { table: Table::Notes };
        let at = u32::try_from(start).map_err(|_| too_large)?;
        let len = u32::try_from(self.notes.len() - start).map_err(|_| too_large)?;
        at.checked_add(len).ok_or(too_large)?;
        Ok(Text { at, len })
    }

    /// A note's text.
    pub fn note_text(&self, text: Text) -> &str {
        text.of(&self.notes)
    }

    /// What resolves the names of token `at`: `$crate`'s crate, def-site and local hygiene.
    pub fn hygiene(&self, at: usize) -> TokenHygiene {
        self.tokens.hygiene(self.tokens.tokens()[at].ctx)
    }

    /// The expansion's canonical bytes: equal expansions, and only they, give equal bytes.
    pub fn encode<S: Sink>(&self, out: &mut S) {
        out.put(b"blaze v2 expansion 1");
        self.tokens.encode(out);
        for origin in self.origins.iter() {
            origin.encode(out);
        }
        out.put(&crate::tokens::index(self.nested.len()).to_be_bytes());
        for nested in self.nested.iter() {
            nested.encode(out);
        }
        out.put(&crate::tokens::index(self.holes.len()).to_be_bytes());
        for hole in self.holes.iter() {
            let of = match hole.of {
                HoleOf::Line => 0,
                HoleOf::Column => 1,
                HoleOf::File => 2,
                HoleOf::ModulePath => 3,
            };
            out.put(&hole.token.to_be_bytes());
            out.put(&[of]);
        }
        self.reads.encode(out);
        out.put(&crate::tokens::index(self.diagnostics.len()).to_be_bytes());
        for diagnostic in self.diagnostics.iter() {
            self.encode_diagnostic(*diagnostic, out);
        }
        out.put(&self.arm.map_or(u32::MAX, |it| it).to_be_bytes());
    }

    fn encode_diagnostic<S: Sink>(&self, diagnostic: Diagnostic, out: &mut S) {
        let text = |out: &mut S, text: Text| {
            let text = self.note_text(text);
            out.put(&crate::tokens::index(text.len()).to_be_bytes());
            out.put(text.as_bytes());
        };
        let origin = |out: &mut S, at: Option<TokenOrigin>| match at {
            None => out.put(&[0]),
            Some(origin) => {
                out.put(&[1]);
                origin.encode(out);
            }
        };
        match diagnostic {
            Diagnostic::NoRuleMatched { rules, closest, token, expected, at } => {
                out.put(&[0]);
                out.put(&rules.to_be_bytes());
                match closest {
                    None => out.put(&[0]),
                    Some(arm) => {
                        out.put(&[1]);
                        out.put(&arm.to_be_bytes());
                    }
                }
                match token {
                    None => out.put(&[0]),
                    Some(token) => {
                        out.put(&[1]);
                        text(out, token);
                    }
                }
                text(out, expected);
                origin(out, at);
            }
            Diagnostic::Binding { message, at } => {
                out.put(&[1]);
                text(out, message);
                origin(out, at);
            }
            Diagnostic::LeftoverTokens { at } => {
                out.put(&[2]);
                origin(out, at);
            }
            Diagnostic::LimitExceeded => out.put(&[3]),
            Diagnostic::UnexpectedToken { at } => {
                out.put(&[4]);
                origin(out, at);
            }
            Diagnostic::BadDefinition { message } => {
                out.put(&[5]);
                text(out, message);
            }
            Diagnostic::CompileError { message } => {
                out.put(&[6]);
                text(out, message);
            }
            Diagnostic::EnvNotDefined { name, message } => {
                out.put(&[7]);
                text(out, name);
                match message {
                    None => out.put(&[0]),
                    Some(message) => {
                        out.put(&[1]);
                        text(out, message);
                    }
                }
            }
            Diagnostic::Include { path, why } => {
                out.put(&[8, why.byte()]);
                text(out, path);
            }
            Diagnostic::Derive { message } => {
                out.put(&[9]);
                text(out, message);
            }
            Diagnostic::Builtin { message, at } => {
                out.put(&[10]);
                text(out, message);
                origin(out, at);
            }
            Diagnostic::ProcMacro { level, message, at } => {
                out.put(&[11, level.byte()]);
                text(out, message);
                origin(out, at);
            }
            Diagnostic::ProcMacroPanicked { message } => {
                out.put(&[12]);
                text(out, message);
            }
        }
    }
}

impl<'b> PartialEq<Expansion<'b>> for Expansion<'_> {
    fn eq(&self, other: &Expansion<'b>) -> bool {
        self.tokens == other.tokens
            && self.origins[..] == other.origins[..]
            && self.nested[..] == other.nested[..]
            && self.holes[..] == other.holes[..]
            && self.reads == other.reads
            && self.diagnostics[..] == other.diagnostics[..]
            && self.notes[..] == other.notes[..]
            && self.arm == other.arm
    }
}

/// What [`crate::expand`] gives.
#[derive(Debug)]
pub enum Expanded<'a> {
    Expansion(Expansion<'a>),
    /// An eager macro whose input has macro calls: expand each (as [`crate::Inside::EagerInput`]),
    /// seal each answer, and call again with them in [`crate::Eager::answers`], in this order.
    /// The ranges are of the call's input.
    NeedsExpanded(AVec<'a, NestedCall>),
}

// SAFETY: indices of the expansion's own streams and a closed enum.
unsafe impl Portable for TokenOrigin {}
// SAFETY: closed enums.
unsafe impl Portable for NestedKind {}
// SAFETY: closed enums.
unsafe impl Portable for PathScope {}
// SAFETY: ranges and indices of the expansion's own tokens and closed enums.
unsafe impl Portable for NestedCall {}
// SAFETY: a closed enum.
unsafe impl Portable for HoleOf {}
// SAFETY: an index of the expansion's own tokens and a closed enum.
unsafe impl Portable for Hole {}
// SAFETY: a closed enum.
unsafe impl Portable for ReadKind {}
// SAFETY: a content digest, a bool or a number.
unsafe impl Portable for ReadAnswer {}
// SAFETY: every field is `Portable`.
unsafe impl Portable for Read {}
// SAFETY: a list of `Read`s and UTF-8 bytes.
unsafe impl Portable for Reads<'_> {}
// SAFETY: a closed enum.
unsafe impl Portable for IncludeFailure {}
// SAFETY: counts, ranges of the expansion's notes and `TokenOrigin`s.
unsafe impl Portable for Diagnostic {}
// SAFETY: every field is `Portable`: a token stream, columns of the types above, UTF-8
// bytes and an index. No span, interned symbol or arena pointer is in it.
unsafe impl Portable for Expansion<'_> {}
