//! The NASM dialect: its preprocessor, and the statements it hands on.
//!
//! NASM source is read a line at a time, as NASM itself reads it. Each line
//! goes through the preprocessor first ([`pp`]): macro parameters inside a
//! macro body, then the `%` directives, then single-line macros, then a
//! multi-line macro call. What is left is an ordinary line, which the lexer
//! reads with the NASM rules and [`stmt`] assembles: labels with or without a
//! colon, `times`, the `db` family, `equ`, the bracketed primitive directives
//! and instructions.
//!
//! A line the preprocessor did not change is lexed where it is, so its
//! diagnostics point into the file it was written in. A changed one is
//! lexed from its expansion, which becomes a file of its own in the source
//! map, named after the line it came from. A multi-line macro's body and a
//! `%rep` block are expanded the same way, and read back through the
//! preprocessor line by line, since `%rotate` and `%if` inside them can only
//! be decided as each line is reached.
//!
//! Much of what looks like directive syntax in NASM source — `section`,
//! `global`, `struc`, `align` — is macros in NASM's standard macro set,
//! wrapping primitive directives written in brackets. rsasm defines the same
//! macros, in [`stdmac`], so they behave the same way, `__SECT__` included.
//!
//! Every behaviour here was checked against NASM 2.16.03; see
//! `tools/nasm-diff`.

pub(crate) mod pp;
pub(crate) mod stdmac;
pub(crate) mod stmt;
pub(crate) mod token;

use crate::intern::Name;
use crate::section::SectionId;
use crate::source::Span;
use crate::symbol::SymbolId;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use token::Tok;

/// A single-line macro: `%define name(params) body`.
#[derive(Clone, Debug)]
pub(crate) struct SMacro {
    /// `None` for a macro written without parentheses.
    pub params: Option<Vec<String>>,
    /// The last parameter takes the rest of the arguments, commas included.
    pub greedy: bool,
    pub body: Vec<Tok>,
}

/// A multi-line macro: `%macro name min-max+ defaults` up to `%endmacro`.
#[derive(Debug)]
pub(crate) struct MMacro {
    pub name: String,
    pub min: usize,
    pub max: usize,
    /// `+`: the last parameter takes the rest of the line.
    pub greedy: bool,
    pub defaults: Vec<Vec<Tok>>,
    /// `%rmacro`: may be called from its own expansion.
    pub recursive: bool,
    /// The body mentions `%00`, so a label on the call is its parameter
    /// rather than a line of its own.
    pub captures_label: bool,
    /// The body's lines, as written.
    pub body: String,
}

/// One multi-line macro expansion in progress.
#[derive(Debug)]
pub(crate) struct Frame {
    pub mac: Rc<MMacro>,
    /// The name the call used, for `%?`.
    pub invoked: String,
    pub params: Vec<Vec<Tok>>,
    /// Set by `%rotate`, modulo the parameter count.
    pub rotate: usize,
    pub label: Vec<Tok>,
    /// Numbers this expansion's `%%` names.
    pub unique: u64,
    /// How many `%rep` blocks were open when the expansion began, so
    /// `%exitrep` only ends one of its own.
    pub reps: usize,
}

/// A `%push` context.
#[derive(Debug)]
pub(crate) struct Context {
    pub name: String,
    pub id: u64,
    pub smacros: HashMap<String, Vec<SMacro>>,
}

/// The state of one `%if`, named after NASM's.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Cond {
    /// In the branch being assembled, before any `%else`.
    IfTrue,
    /// Not yet in a true branch; a later `%elif` or `%else` may be.
    IfFalse,
    ElseTrue,
    ElseFalse,
    /// A branch has been taken already.
    Done,
    /// Inside a conditional that is not assembled at all.
    Never,
}

impl Cond {
    pub fn emitting(self) -> bool {
        matches!(self, Cond::IfTrue | Cond::ElseTrue)
    }
}

/// What a `%exitmacro` or `%exitrep` is unwinding.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Unwind {
    Macro,
    Rep,
}

/// Where `absolute` has put the location counter.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Absolute {
    /// The address given to `absolute`, which is `$$`.
    pub base: i64,
    /// The current address, which is `$`.
    pub here: i64,
}

/// Everything the NASM dialect keeps between lines.
#[derive(Default)]
pub(crate) struct State {
    pub smacros: HashMap<String, Vec<SMacro>>,
    /// `%idefine` macros, by lowercased name.
    pub ismacros: HashMap<String, Vec<SMacro>>,
    /// `%defalias` names, and what they stand for.
    pub aliases: HashMap<String, String>,
    /// `%macro` definitions by name, and `%imacro` ones by lowercased name.
    pub mmacros: HashMap<String, Vec<Rc<MMacro>>>,
    pub immacros: HashMap<String, Vec<Rc<MMacro>>>,
    pub contexts: Vec<Context>,
    pub next_context: u64,
    pub conds: Vec<Cond>,
    pub frames: Vec<Frame>,
    /// `%rep` blocks being expanded.
    pub reps: usize,
    pub next_unique: u64,
    pub unwind: Option<Unwind>,
    /// The last label that was not local, which a `.local` name belongs to.
    pub base_label: Option<String>,
    pub absolute: Option<Absolute>,
    /// The label behind `$$` in each section, made when first needed.
    pub section_starts: HashMap<SectionId, SymbolId>,
    /// Names declared `extern`, which may stay undefined.
    pub externs: HashSet<Name>,
    /// `default rel`: a memory operand with no register is RIP-relative.
    pub default_rel: bool,
    /// `org`, for a flat binary.
    pub org: Option<(u64, Span)>,
    /// Sections whose `align=` was given explicitly.
    pub explicit_align: HashSet<SectionId>,
    /// Sections a `section` directive named in an ELF object, which NASM
    /// writes out whether or not anything went into them. A COFF object
    /// reads the same thing off `Assembler::coff`, whose entry a named
    /// section gets anyway for its characteristics.
    pub opened: HashSet<SectionId>,
    /// Nesting of expansions, for the recursion limit.
    pub depth: u32,
}
