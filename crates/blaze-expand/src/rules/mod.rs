//! `macro_rules!` and `macro`: blaze's own matcher and transcriber over the portable
//! [`TokenStream`].
//!
//! Rust's rules for declarative macros are the specification (the Reference's "Macros By
//! Example", rustc's `mbe`); the design is this crate's. A definition is compiled once
//! ([`compile`]) into a [`Program`]: per arm, a flat array of matcher locations (the fixed
//! tokens to compare, the fragments to parse, repetitions with their separators, each with its
//! jump targets) and a flat array of template operations, every token named by its index in the
//! definition. A call is then one run of the program over the call's tokens ([`expand`]):
//!
//! - the arms whose first fixed token cannot start the input are not tried
//!   ([`First`]), so most calls try one arm;
//! - matching (`matcher`) is rustc's position-set matcher run on explicit lists, and
//!   transcription (`transcribe`) runs on an explicit stack straight into the expansion, so
//!   neither recurses on the input or the template;
//! - every step is counted and every nesting is bounded: past a bound the call is refused with
//!   [`ExpandError::RulesBound`], naming the macro and the call (rules 18, 19);
//! - the matcher's lists live in the call's task frame, and the output is written once, in the
//!   expansion's allocator.
//!
//! When no arm matches, every arm of the call's style is matched again with the error
//! bookkeeping rustc's `failed_to_match_macro` does, so a call that expands pays nothing for it.

mod copy;
mod failure;
mod input;
mod linear;
mod matcher;

pub(crate) use input::{consumed, words_of};
mod transcribe;

use blaze_mem::frame::Frame;
use blaze_own::{AVec, Vec};
use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::{
    call::{CallKind, Checked},
    def::{MacroDefinition, MacroKind},
    error::{ExpandError, RulesLimit, Stream},
    expand::Out,
    expansion::{Diagnostic, TokenOrigin},
    key::call_key,
    tokens::{Ctx, Delim, Edition, LitKind, Raw, Spacing, Token, TokenKind, TokenStream, Transparency, WholeText},
};

/// No index: the end of a list, an unbound binder, no separator.
pub(crate) const NONE: u32 = u32::MAX;

/// How many positions the matcher may process, and template operations the transcriber may run,
/// in one call, every arm and the error pass together: rule 18's bound. rustc has none; a call
/// that passes it is refused, never cut short.
const STEP_BOUND: u64 = 1 << 26;

/// How deep the input's groups may nest under the matcher, and the template's groups and
/// repetitions under the transcriber.
const DEPTH_BOUND: usize = 1024;

/// The bit of a [`DefTok`] that says it is of the parameters of `macro m(params)`.
const PARAMS: u32 = 1 << 31;

/// A token of the definition: its index in the body, or with [`PARAMS`] in the parameters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DefTok(u32);

impl DefTok {
    fn at(self) -> usize {
        (self.0 & !PARAMS) as usize
    }

    /// The origin of a token copied from the definition.
    pub(crate) fn origin(self) -> TokenOrigin {
        match self.0 & PARAMS {
            0 => TokenOrigin::Def(self.0),
            _ => TokenOrigin::Params(self.0 & !PARAMS),
        }
    }
}

/// The definition's token streams, read by [`DefTok`].
#[derive(Clone, Copy)]
pub(crate) struct Def<'d> {
    pub(crate) body: &'d TokenStream<'d>,
    pub(crate) params: Option<&'d TokenStream<'d>>,
}

impl<'d> Def<'d> {
    pub(crate) fn of(def: &'d MacroDefinition<'d>) -> Def<'d> {
        Def { body: def.body(), params: def.params() }
    }

    pub(crate) fn stream(self, tok: DefTok) -> &'d TokenStream<'d> {
        match tok.0 & PARAMS {
            0 => self.body,
            _ => self.params.expect("a parameter token is of a definition with parameters"),
        }
    }

    pub(crate) fn token(self, tok: DefTok) -> Token {
        self.stream(tok).tokens()[tok.at()]
    }

    pub(crate) fn kind(self, tok: DefTok) -> TokenKind {
        self.token(tok).kind
    }

    pub(crate) fn text(self, tok: DefTok) -> &'d str {
        self.stream(tok).text_at(tok.at())
    }

    /// [`Def::text`] as bytes: what a match compares (a copy writes [`Def::whole`]).
    pub(crate) fn bytes(self, tok: DefTok) -> &'d [u8] {
        self.stream(tok).bytes_at(tok.at())
    }

    /// [`Def::text`] as a [`WholeText`]: what a copy writes, with no UTF-8 check.
    pub(crate) fn whole(self, tok: DefTok) -> WholeText<'d> {
        self.stream(tok).whole_at(tok.at())
    }

    pub(crate) fn edition(self, tok: DefTok) -> Edition {
        let stream = self.stream(tok);
        stream.edition_of(stream.tokens()[tok.at()].ctx)
    }

    /// The character of a punctuation token; `'\0'` for any other.
    pub(crate) fn char(self, tok: DefTok) -> char {
        match self.kind(tok) {
            TokenKind::Punct { char, .. } => char,
            _ => '\0',
        }
    }

    /// Whether two metavariable names are one name: the same text, and the same context once
    /// transparent marks are skipped (rustc's `MacroRulesNormalizedIdent`).
    fn same_name(self, a: DefTok, b: DefTok) -> bool {
        if self.bytes(a) != self.bytes(b) {
            return false;
        }
        let (sa, sb) = (self.stream(a), self.stream(b));
        let (mut ca, mut cb) = (self.token(a).ctx, self.token(b).ctx);
        loop {
            let na = opaque_node(sa, &mut ca);
            let nb = opaque_node(sb, &mut cb);
            match (na, nb) {
                (None, None) => return true,
                (Some(x), Some(y)) if x == y => {}
                _ => return false,
            }
        }
    }
}

/// The next mark of `ctx`'s chain that is not transparent, `ctx` moved to its parent; `None` at
/// the root.
fn opaque_node(stream: &TokenStream<'_>, ctx: &mut Ctx) -> Option<crate::tokens::Mark> {
    while let Some(node) = stream.node(*ctx) {
        *ctx = node.parent;
        if node.mark.transparency != Transparency::Transparent {
            return Some(node.mark);
        }
    }
    None
}

/// A metavariable's fragment specifier, as the edition of its `$name:spec` reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Frag {
    Path,
    Ty,
    /// `pat` from edition 2021: top-level or-patterns.
    Pat,
    /// `pat_param`, and `pat` before 2021.
    PatParam,
    Stmt,
    Block,
    Meta,
    Item,
    Vis,
    /// `expr` from edition 2024: `_`, `const { .. }` and `let` too.
    Expr,
    /// `expr_2021`, and `expr` before 2024.
    Expr2021,
    Ident,
    Tt,
    Lifetime,
    Literal,
}

impl Frag {
    fn of(spec: &str, edition: Edition) -> Option<Frag> {
        Some(match spec {
            "path" => Frag::Path,
            "ty" => Frag::Ty,
            "pat" if edition >= Edition::E2021 => Frag::Pat,
            "pat" | "pat_param" => Frag::PatParam,
            "stmt" => Frag::Stmt,
            "block" => Frag::Block,
            "meta" => Frag::Meta,
            "item" => Frag::Item,
            "vis" => Frag::Vis,
            "expr" if edition >= Edition::E2024 => Frag::Expr,
            "expr" | "expr_2021" => Frag::Expr2021,
            "ident" => Frag::Ident,
            "tt" => Frag::Tt,
            "lifetime" => Frag::Lifetime,
            "literal" => Frag::Literal,
            _ => return None,
        })
    }
}

/// How many times a repetition may match.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RepKind {
    /// `*`
    ZeroOrMore,
    /// `+`
    OneOrMore,
    /// `?`
    ZeroOrOne,
}

/// What a rule is called as: `attr(..)` and `derive(..)` rules, or the others.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Style {
    FnLike,
    Attr,
    Derive,
}

/// A repetition's separator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Sep {
    Ident(DefTok),
    Literal(DefTok),
    /// Punctuation glued into one token: `len` tokens from `at`.
    Puncts { at: DefTok, len: u8 },
    /// `'name`: the quote and the name.
    Lifetime { quote: DefTok, name: DefTok },
}

/// A place in an arm's matcher: rustc's `MatcherLoc`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Loc {
    Literal(DefTok),
    Ident(DefTok),
    /// `$crate`, written at the `crate` token.
    Crate(DefTok),
    Punct { at: DefTok, len: u8 },
    /// `$name:spec`: `kind` `None` for a specifier no edition has, which no input matches.
    Var { binder: u32, kind: Option<Frag>, name: DefTok, spec: DefTok },
    /// A group of the pattern, which needs a group of the input with this delimiter. A visible
    /// group's `Open`, contents and `Close` follow; an invisible group's contents alone.
    Subtree(Delim),
    Open(Delim),
    Close(Delim),
    /// A repetition, `Program::reps[_]`; its body follows.
    RepStart(u32),
    /// The end of the body of `Program::reps[_]`.
    RepEnd(u32),
    Eof,
}

/// A repetition of a matcher.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Rep {
    /// The location of the body's first place.
    pub(crate) body: u32,
    /// The location after the repetition's `RepEnd`.
    pub(crate) after: u32,
    /// The binders of the body, nested ones too, in pattern order: `Program::rep_vars[_]`.
    pub(crate) vars: u32,
    pub(crate) vars_len: u32,
    /// `Program::seps[_]`, or [`NONE`].
    pub(crate) sep: u32,
    pub(crate) kind: RepKind,
}

/// A template operation. A group's or a repetition's body is the operations after it up to
/// its `end`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Op {
    /// An identifier or literal of the definition, copied.
    Tok(DefTok),
    Crate(DefTok),
    Puncts { at: DefTok, len: u8 },
    /// `$name`; in a pattern also its fragment specifier (`spec` [`NONE`] in a template).
    Var { binder: u32, kind: Option<Frag>, name: DefTok, spec: DefTok },
    Group { delim: Delim, open: DefTok, close: DefTok, end: u32 },
    Repeat { end: u32, sep: u32, kind: RepKind },
    /// `${ignore($name)}`
    Ignore { binder: u32, name: DefTok },
    /// `${index(depth)}`
    Index { depth: u32 },
    /// `${len(depth)}`
    Len { depth: u32 },
    /// `${count($name, depth)}`
    Count { binder: u32, name: DefTok, depth: u32 },
    /// `${concat(..)}`: `Program::concat[start..start + len]`, made at `func`.
    Concat { start: u32, len: u32, func: DefTok },
}

/// An element of `${concat(..)}`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ConcatElem {
    Literal(DefTok),
    Ident(DefTok),
    Var { binder: u32, name: DefTok },
}

/// What the first input token must be for an arm to match, read from its first location.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum First {
    /// Anything: a metavariable, a repetition.
    Any,
    /// No token.
    Empty,
    /// This identifier (raw or not), literal, or the first character of this punctuation.
    Token(DefTok),
    Crate,
    Group(Delim),
}

/// One arm (rule) of a definition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Arm {
    pub(crate) style: Style,
    /// The matcher: `Program::locs[locs..]`, up to its `Eof`.
    pub(crate) locs: u32,
    /// The template: `Program::ops[ops..ops_end]`.
    pub(crate) ops: u32,
    pub(crate) ops_end: u32,
    /// How many names the matcher binds; their declarations are `Program::names[names..]`.
    pub(crate) binders: u32,
    pub(crate) names: u32,
    pub(crate) first: First,
}

/// Why a definition's rules do not parse: rustc's and mbe's words.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DefError {
    Message(&'static str),
    InvalidRepeat,
    EmptyRepetition,
}

impl fmt::Display for DefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DefError::Message(text) => f.write_str(text),
            DefError::InvalidRepeat => f.write_str("invalid repeat"),
            DefError::EmptyRepetition => f.write_str("empty token tree in repetition"),
        }
    }
}

/// A definition compiled for its calls: every array flat and `Copy`, every token an index into
/// the definition, so it holds nothing of the machine and could be written as a record keyed by
/// the definition's content.
pub(crate) struct Program<'a> {
    pub(crate) arms: AVec<'a, Arm>,
    pub(crate) locs: AVec<'a, Loc>,
    pub(crate) reps: AVec<'a, Rep>,
    pub(crate) rep_vars: AVec<'a, u32>,
    pub(crate) seps: AVec<'a, Sep>,
    pub(crate) ops: AVec<'a, Op>,
    pub(crate) concat: AVec<'a, ConcatElem>,
    /// Each arm's binders' declaring tokens.
    pub(crate) names: AVec<'a, DefTok>,
    /// The first parse error; the arms before it are kept, as rustc keeps them.
    pub(crate) error: Option<DefError>,
    /// The matchers' nesting, for the linear matcher ([`linear::nest`]).
    pub(crate) nests: linear::Nests<'a>,
}

pub use copy::{CopyPlant, rules_copy_plant};

/// Whether every call runs both matchers and compares them ([`rules_oracle`]).
static ORACLE: AtomicBool = AtomicBool::new(false);
static ORACLE_COUNTS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// Turns the matchers' oracle on or off for the process: with it on, every arm a call tries
/// is matched by the linear matcher and by the position-set matcher, and the two are compared
/// (matched or not, every binding, the tokens written and the transcription's error).
pub fn rules_oracle(on: bool) {
    ORACLE.store(on, Ordering::Relaxed);
}

/// The oracle's counts since the process began: arms equal, arms different, arms left to the
/// exact mode, calls no arm matched (the exact mode makes their diagnostic), calls.
pub fn rules_oracle_counts() -> [u64; 5] {
    core::array::from_fn(|at| ORACLE_COUNTS[at].load(Ordering::Relaxed))
}

/// Compiles `def`, a `macro_rules!` or `macro`, in the definition's allocator.
pub(crate) fn compile<'d>(def: &'d MacroDefinition<'d>) -> Result<Program<'d>, ExpandError> {
    let body = def.body();
    if let Some(token) = body.unbalanced() {
        return Err(ExpandError::Unbalanced { stream: Stream::Def, token });
    }
    if let Some(params) = def.params()
        && let Some(token) = params.unbalanced()
    {
        return Err(ExpandError::Unbalanced { stream: Stream::Params, token });
    }
    let alloc = body.allocator();
    let mut program = Program {
        arms: Vec::new_in(alloc),
        locs: Vec::new_in(alloc),
        reps: Vec::new_in(alloc),
        rep_vars: Vec::new_in(alloc),
        seps: Vec::new_in(alloc),
        ops: Vec::new_in(alloc),
        concat: Vec::new_in(alloc),
        names: Vec::new_in(alloc),
        error: None,
        nests: linear::Nests::new_in(alloc),
    };
    let refused = |bound| ExpandError::RulesBound {
        bound,
        def: def.content(),
        call: None,
    };
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        let defs = Def::of(def);
        let body_close = closes(body, frame);
        let params_close = def.params().map(|params| closes(params, frame));
        let mut c = Compiler {
            def: defs,
            program: &mut program,
            scratch: Vec::new_in(frame),
            body: Src { close: &body_close, base: 0 },
            params: params_close.as_ref().map(|close| Src { close, base: PARAMS }),
            frame,
        };
        let result = match (def.kind(), c.params) {
            (MacroKind::Macro2, Some(params)) => c.macro2_simple(params),
            // astlog-ignore: expand-hand-extent (the macro language: a `macro` item's rule separators)
            (MacroKind::Macro2, None) => c.rules(&[';', ',']),
            _ => c.rules(&[';']),
        };
        match result {
            Ok(()) => Ok(()),
            Err(Stop::Def(error)) => {
                c.program.error = Some(error);
                Ok(())
            }
            Err(Stop::Deep) => Err(refused(RulesLimit::Depth)),
        }
    })?;
    linear::nest(&mut program);
    Ok(program)
}

/// For each token of `stream`, the index of its group's `Close` if it opens one, else [`NONE`].
fn closes<'f>(stream: &TokenStream<'_>, frame: &'f Frame) -> Vec<u32, &'f Frame> {
    let tokens = stream.tokens();
    let mut close = Vec::with_capacity_in(tokens.len(), frame);
    close.resize(tokens.len(), NONE);
    let mut open = Vec::new_in(frame);
    for (at, token) in tokens.iter().enumerate() {
        match token.kind {
            TokenKind::Open(_) => open.push(at),
            TokenKind::Close(_) => {
                let start = open.pop().expect("the stream was checked balanced");
                close[start] = crate::tokens::index(at);
            }
            _ => {}
        }
    }
    close
}

/// What [`Compiler::next_op`] read: an operation to push, or a group or repetition already
/// pushed (at `at`) whose body `inner` is read next.
enum Next<'c> {
    Op(Op),
    Open { at: usize, inner: Cur<'c> },
}

/// Why compiling stopped: the definition's own error, a value of the program; or a nesting
/// past [`DEPTH_BOUND`].
enum Stop {
    Def(DefError),
    Deep,
}

impl From<DefError> for Stop {
    fn from(error: DefError) -> Stop {
        Stop::Def(error)
    }
}

/// One of the definition's streams: its close table and its [`DefTok`] bit.
#[derive(Clone, Copy)]
struct Src<'c> {
    close: &'c [u32],
    base: u32,
}

/// A cursor over one level of a definition stream: `at` up to `end`.
#[derive(Clone, Copy)]
struct Cur<'c> {
    src: Src<'c>,
    at: u32,
    end: u32,
}

impl Cur<'_> {
    fn tok(self) -> DefTok {
        DefTok(self.at | self.src.base)
    }

    fn is_empty(self) -> bool {
        self.at >= self.end
    }
}

struct Compiler<'p, 'd, 'f> {
    def: Def<'d>,
    program: &'p mut Program<'d>,
    /// A pattern's operations while it is read, before they become matcher locations.
    scratch: Vec<Op, &'f Frame>,
    body: Src<'f>,
    params: Option<Src<'f>>,
    frame: &'f Frame,
}

/// Whether operations are read as a pattern or as a template.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Pattern,
    Template,
}

impl<'d, 'f> Compiler<'_, 'd, 'f> {
    fn kind(&self, c: Cur<'_>) -> Option<TokenKind> {
        (!c.is_empty()).then(|| self.def.kind(c.tok()))
    }

    fn text(&self, c: Cur<'_>) -> &'d str {
        self.def.text(c.tok())
    }

    /// Takes the next token tree; a group is skipped whole.
    fn next(&self, c: &mut Cur<'_>) -> Option<DefTok> {
        let kind = self.kind(*c)?;
        let tok = c.tok();
        c.at = match kind {
            TokenKind::Open(_) => c.src.close[c.at as usize] + 1,
            _ => c.at + 1,
        };
        Some(tok)
    }

    /// Takes the next token tree if it is a group: its open token and a cursor over its inside.
    fn expect_group<'c>(&self, c: &mut Cur<'c>) -> Option<(DefTok, Cur<'c>)> {
        let open = c.at;
        let tok = self.next(c)?;
        match self.def.kind(tok) {
            TokenKind::Open(_) => {
                let close = c.src.close[open as usize];
                Some((tok, Cur { src: c.src, at: open + 1, end: close }))
            }
            _ => None,
        }
    }

    /// Takes the next token tree, and says whether it was the punctuation `char`.
    fn expect_char(&self, c: &mut Cur<'_>, chars: &[char]) -> bool {
        match self.next(c) {
            Some(tok) => matches!(self.def.kind(tok), TokenKind::Punct { char, .. } if chars.contains(&char)),
            None => false,
        }
    }

    fn peek_ident(&self, c: Cur<'_>) -> Option<&'d str> {
        match self.kind(c)? {
            TokenKind::Ident { .. } => Some(self.text(c)),
            _ => None,
        }
    }

    /// An identifier that is not `_` (`underscore` true: any identifier).
    fn expect_ident(&self, c: &mut Cur<'_>, underscore: bool) -> Option<DefTok> {
        let tok = self.next(c)?;
        match self.def.kind(tok) {
            TokenKind::Ident { .. } if underscore || self.def.text(tok) != "_" => Some(tok),
            _ => None,
        }
    }

    /// Glued punctuation at `c`: how many tokens, if the first is punctuation.
    fn glued(&self, c: Cur<'_>) -> Option<u8> {
        let len = glued_len(|at| self.kind(Cur { at, ..c }), c.at);
        (len > 0).then_some(len)
    }

    /// `macro_rules! { rules }` and `macro m { rules }`: rules, each followed by one of `seps`.
    fn rules(&mut self, seps: &[char]) -> Result<(), Stop> {
        let mut c = Cur { src: self.body, at: 0, end: crate::tokens::index(self.def.body.len()) };
        let mut error = None;
        while !c.is_empty() {
            if let Err(stop) = self.rule(&mut c) {
                match stop {
                    Stop::Def(e) => error = Some(e),
                    Stop::Deep => return Err(Stop::Deep),
                }
                break;
            }
            if !self.expect_char(&mut c, seps) {
                if !c.is_empty() {
                    error = Some(DefError::Message(match seps.len() {
                        1 => "expected `;`",
                        _ => "expected `;` or `,` to delimit rules",
                    }));
                }
                break;
            }
        }
        // rustc checks each rule's pattern once all are read; the first refused replaces a
        // parse error.
        for arm in 0..self.program.arms.len() {
            if let Err(e) = self.validate(arm) {
                error = Some(e);
                break;
            }
        }
        match error {
            Some(e) => Err(Stop::Def(e)),
            None => Ok(()),
        }
    }

    /// `macro m(params) { body }`: one function-like rule.
    fn macro2_simple(&mut self, params: Src<'f>) -> Result<(), Stop> {
        let pattern = Cur { src: params, at: 0, end: crate::tokens::index(params.close.len()) };
        let template = Cur { src: self.body, at: 0, end: crate::tokens::index(self.def.body.len()) };
        self.scratch.clear();
        self.pattern(pattern, 0)?;
        self.finish_rule(Style::FnLike, template)?;
        self.validate(0).map_err(Stop::Def)
    }

    /// One rule: `[unsafe] [attr(args) | derive()] (pattern) => {template}`.
    fn rule(&mut self, c: &mut Cur<'f>) -> Result<(), Stop> {
        if self.peek_ident(*c) == Some("unsafe") {
            self.next(c);
        }
        let (style, args) = match self.peek_ident(*c) {
            Some("attr") => {
                self.next(c);
                let args = self.expect_group(c).ok_or(DefError::Message("expected `()`"))?;
                (Style::Attr, Some(args))
            }
            Some("derive") => {
                self.next(c);
                self.expect_group(c).ok_or(DefError::Message("expected `()`"))?;
                (Style::Derive, None)
            }
            _ => (Style::FnLike, None),
        };
        let subtree = DefError::Message("expected subtree");
        let (item_open, lhs) = self.expect_group(c).ok_or(subtree)?;
        if !self.expect_char(c, &['=']) {
            return Err(DefError::Message("expected `=`").into());
        }
        // astlog-ignore: expand-hand-extent (the macro language: a rule's `=>`)
        if !self.expect_char(c, &['>']) {
            return Err(DefError::Message("expected `>`").into());
        }
        let (_, rhs) = self.expect_group(c).ok_or(subtree)?;
        self.scratch.clear();
        match args {
            None => self.pattern(lhs, 0)?,
            // An `attr(..)` rule matches one input of two parenthesized groups, the attribute's
            // arguments and the item, so its bindings from both are one match's: the item's
            // pattern is read first, as rustc reads it, and placed second.
            Some((args_open, args)) => {
                let item = self.group_op(item_open, lhs, 0)?;
                let item_ops = self.scratch.len() - item;
                let at = self.scratch.len();
                self.group_op(args_open, args, 0)?;
                // Move the args group before the item group, fixing the groups' ends.
                let moved = self.scratch.len() - at;
                self.scratch[item..].rotate_left(item_ops);
                for op in &mut self.scratch[item..] {
                    shift_end(op, item_ops as u32, moved as u32, item as u32);
                }
            }
        }
        self.finish_rule(style, rhs)
    }

    /// Reads the group `open` with inside `inner` as a pattern group of `(..)`, appended to the
    /// scratch; gives where it starts.
    fn group_op(&mut self, open: DefTok, inner: Cur<'f>, depth: usize) -> Result<usize, Stop> {
        let at = self.scratch.len();
        let close = DefTok(inner.end | inner.src.base);
        self.scratch.push(Op::Group { delim: Delim::Paren, open, close, end: 0 });
        self.pattern(inner, depth + 1)?;
        let end = crate::tokens::index(self.scratch.len());
        if let Op::Group { end: slot, .. } = &mut self.scratch[at] {
            *slot = end;
        }
        Ok(at)
    }

    /// The pattern in the scratch and the template `rhs` as an arm.
    fn finish_rule(&mut self, style: Style, rhs: Cur<'f>) -> Result<(), Stop> {
        let names = crate::tokens::index(self.program.names.len());
        // The binders: one per name, in the order the names first appear.
        for op in self.scratch.iter() {
            if let Op::Var { name, .. } = *op
                && !self.program.names[names as usize..].iter().any(|&it| self.def.same_name(it, name))
            {
                self.program.names.push(name);
            }
        }
        let binders = crate::tokens::index(self.program.names.len()) - names;
        let locs = crate::tokens::index(self.program.locs.len());
        let mut scratch = core::mem::replace(&mut self.scratch, Vec::new_in(self.frame));
        for op in scratch.iter_mut() {
            if let Op::Var { binder, name, .. } = op {
                *binder = self.binder(names, *name);
            }
        }
        self.flatten(&scratch, 0, scratch.len())?;
        self.program.locs.push(Loc::Eof);
        let first = self.first(locs);
        self.scratch = scratch;
        let ops = crate::tokens::index(self.program.ops.len());
        self.template(rhs, 0)?;
        let ops_end = crate::tokens::index(self.program.ops.len());
        for at in ops as usize..ops_end as usize {
            match &mut self.program.ops[at] {
                Op::Var { binder, name, .. }
                | Op::Ignore { binder, name }
                | Op::Count { binder, name, .. } => {
                    *binder = binder_of(&self.program.names, self.def, names, binders, *name);
                }
                _ => {}
            }
        }
        for elem in self.program.concat.iter_mut() {
            if let ConcatElem::Var { binder, name } = elem
                && *binder == NONE - 1
            {
                *binder = binder_of(&self.program.names, self.def, names, binders, *name);
            }
        }
        self.program.arms.push(Arm { style, locs, ops, ops_end, binders, names, first });
        Ok(())
    }

    fn binder(&self, names: u32, name: DefTok) -> u32 {
        let binders = crate::tokens::index(self.program.names.len()) - names;
        binder_of(&self.program.names, self.def, names, binders, name)
    }

    /// The scratch's pattern operations `[from, to)` as matcher locations.
    fn flatten(&mut self, ops: &[Op], from: usize, to: usize) -> Result<(), Stop> {
        enum Close {
            Group(Delim),
            Repeat { rep: u32, body: u32 },
        }
        struct FlatFrame {
            at: usize,
            to: usize,
            close: Option<Close>,
        }
        let mut stack: Vec<FlatFrame, &'f Frame> = Vec::new_in(self.frame);
        stack.push(FlatFrame { at: from, to, close: None });
        while let Some(top) = stack.last_mut() {
            if top.at >= top.to {
                let Some(done) = stack.pop() else { break };
                match done.close {
                    Some(Close::Group(delim)) => {
                        if delim != Delim::Invisible {
                            self.program.locs.push(Loc::Close(delim));
                        }
                    }
                    Some(Close::Repeat { rep, body }) => {
                        self.program.locs.push(Loc::RepEnd(rep));
                        let after = crate::tokens::index(self.program.locs.len());
                        let rep = &mut self.program.reps[rep as usize];
                        rep.body = body;
                        rep.after = after;
                    }
                    None => {}
                }
                continue;
            }
            let at = top.at;
            match ops[at] {
                Op::Tok(tok) => {
                    self.program.locs.push(match self.def.kind(tok) {
                        TokenKind::Literal { .. } => Loc::Literal(tok),
                        _ => Loc::Ident(tok),
                    });
                    top.at = at + 1;
                }
                Op::Crate(tok) => {
                    self.program.locs.push(Loc::Crate(tok));
                    top.at = at + 1;
                }
                Op::Puncts { at: tok, len } => {
                    self.program.locs.push(Loc::Punct { at: tok, len });
                    top.at = at + 1;
                }
                Op::Var { binder, kind, name, spec } => {
                    self.program.locs.push(Loc::Var { binder, kind, name, spec });
                    top.at = at + 1;
                }
                Op::Group { delim, end, .. } => {
                    self.program.locs.push(Loc::Subtree(delim));
                    let visible = delim != Delim::Invisible;
                    if visible {
                        self.program.locs.push(Loc::Open(delim));
                    }
                    top.at = end as usize;
                    stack.push(FlatFrame { at: at + 1, to: end as usize, close: Some(Close::Group(delim)) });
                }
                Op::Repeat { end, sep, kind } => {
                    let rep = crate::tokens::index(self.program.reps.len());
                    let vars = crate::tokens::index(self.program.rep_vars.len());
                    for op in &ops[at + 1..end as usize] {
                        if let Op::Var { binder, .. } = *op {
                            self.program.rep_vars.push(binder);
                        }
                    }
                    let vars_len = crate::tokens::index(self.program.rep_vars.len()) - vars;
                    self.program.reps.push(Rep { body: 0, after: 0, vars, vars_len, sep, kind });
                    self.program.locs.push(Loc::RepStart(rep));
                    let body = crate::tokens::index(self.program.locs.len());
                    top.at = end as usize;
                    stack.push(FlatFrame { at: at + 1, to: end as usize, close: Some(Close::Repeat { rep, body }) });
                }
                Op::Ignore { .. }
                | Op::Index { .. }
                | Op::Len { .. }
                | Op::Count { .. }
                | Op::Concat { .. } => unreachable!("a pattern has no metavariable expressions"),
            }
        }
        Ok(())
    }

    /// What the first input token must be for the matcher at `locs` to match.
    fn first(&self, locs: u32) -> First {
        match self.program.locs[locs as usize] {
            Loc::Literal(tok) | Loc::Ident(tok) | Loc::Punct { at: tok, .. } => First::Token(tok),
            Loc::Crate(_) => First::Crate,
            Loc::Subtree(delim) if delim != Delim::Invisible => First::Group(delim),
            Loc::Eof => First::Empty,
            _ => First::Any,
        }
    }

    /// rustc's check that no repetition of the arm's pattern can match no tokens.
    fn validate(&self, arm: usize) -> Result<(), DefError> {
        let start = self.program.arms[arm].locs as usize;
        let mut at = start;
        loop {
            match self.program.locs[at] {
                Loc::Eof => return Ok(()),
                Loc::RepStart(rep) => {
                    let rep = self.program.reps[rep as usize];
                    if rep.sep == NONE && self.empty_body(rep) {
                        return Err(DefError::EmptyRepetition);
                    }
                }
                _ => {}
            }
            at += 1;
        }
    }

    /// Whether every top-level place of `rep`'s body is a `vis` or a repetition that may match
    /// nothing (an empty body too).
    fn empty_body(&self, rep: Rep) -> bool {
        let mut at = rep.body as usize;
        let end = rep.after as usize - 1;
        while at < end {
            match self.program.locs[at] {
                Loc::Var { kind: Some(Frag::Vis), .. } => at += 1,
                Loc::RepStart(inner) => {
                    let inner = self.program.reps[inner as usize];
                    if inner.kind == RepKind::OneOrMore {
                        return false;
                    }
                    at = inner.after as usize;
                }
                _ => return false,
            }
        }
        true
    }

    /// Reads a pattern into the scratch.
    fn pattern(&mut self, c: Cur<'f>, depth: usize) -> Result<(), Stop> {
        self.read(c, Mode::Pattern, depth)
    }

    /// Reads a template into the program's operations.
    fn template(&mut self, c: Cur<'f>, depth: usize) -> Result<(), Stop> {
        self.read(c, Mode::Template, depth)
    }

    /// Reads the level `c`, `depth` deep, and every group and repetition in it, on an explicit
    /// stack of the levels open (so a definition's nesting costs the frame, not the thread's
    /// stack); past [`DEPTH_BOUND`] it stops.
    fn read(&mut self, mut c: Cur<'f>, mode: Mode, depth: usize) -> Result<(), Stop> {
        if depth > DEPTH_BOUND {
            return Err(Stop::Deep);
        }
        // Each open level: the enclosing level's cursor after it, and its operation's index.
        let mut open: Vec<(Cur<'f>, usize), &'f Frame> = Vec::new_in(self.frame);
        loop {
            if c.is_empty() {
                let Some((outer, at)) = open.pop() else { return Ok(()) };
                let end = self.ops_len(mode);
                self.set_end(mode, at, end);
                c = outer;
                continue;
            }
            if mode == Mode::Pattern
                && let Some(after) = self.doc_attr_end(c)
            {
                c = after;
                continue;
            }
            match self.next_op(&mut c, mode)? {
                Next::Op(op) => {
                    self.push_op(mode, op);
                }
                Next::Open { at, inner } => {
                    if depth + open.len() + 1 > DEPTH_BOUND {
                        return Err(Stop::Deep);
                    }
                    open.push((c, at));
                    c = inner;
                }
            }
        }
    }

    /// BLAZE'S RULE (the Reference is silent; konst 0.2.20's `__declare_string_cmp_fns!` needs
    /// it): a doc attribute in a matcher, `#[doc = "..."]` or `#![doc = "..."]` with a string
    /// literal (what a `///` or `//!` comment lexes to: the Reference, "Comments", "Doc comments
    /// are interpreted as a special syntax for attributes"), matches nothing: it is a comment on
    /// the matcher, not tokens the call must write. Gives the cursor after it, `None` when `c`
    /// starts no such attribute. A `$` fragment inside the brackets is not one: it binds.
    fn doc_attr_end(&self, c: Cur<'f>) -> Option<Cur<'f>> {
        let mut at = c;
        if !self.expect_char(&mut at, &['#']) {
            return None;
        }
        if matches!(self.kind(at), Some(TokenKind::Punct { char: '!', .. })) {
            self.next(&mut at);
        }
        if !matches!(self.kind(at), Some(TokenKind::Open(Delim::Bracket))) {
            return None;
        }
        let (_, mut inside) = self.expect_group(&mut at)?;
        let doc = self.expect_ident(&mut inside, false).is_some_and(|tok| self.def.text(tok) == "doc");
        let eq = doc && self.expect_char(&mut inside, &['=']);
        let lit = eq && matches!(self.next(&mut inside).map(|tok| self.def.kind(tok)), Some(TokenKind::Literal { kind: LitKind::Str, .. }));
        (lit && inside.is_empty()).then_some(at)
    }

    fn push_op(&mut self, mode: Mode, op: Op) -> usize {
        match mode {
            Mode::Pattern => {
                self.scratch.push(op);
                self.scratch.len() - 1
            }
            Mode::Template => {
                self.program.ops.push(op);
                self.program.ops.len() - 1
            }
        }
    }

    fn ops_len(&self, mode: Mode) -> u32 {
        crate::tokens::index(match mode {
            Mode::Pattern => self.scratch.len(),
            Mode::Template => self.program.ops.len(),
        })
    }

    fn set_end(&mut self, mode: Mode, at: usize, end: u32) {
        let op = match mode {
            Mode::Pattern => &mut self.scratch[at],
            Mode::Template => &mut self.program.ops[at],
        };
        match op {
            Op::Group { end: slot, .. } | Op::Repeat { end: slot, .. } => *slot = end,
            _ => unreachable!("only groups and repetitions have an end"),
        }
    }

    /// Reads one operation at `c`; a group or a repetition is pushed itself and its body given
    /// back to read, any other operation is given back to push.
    fn next_op(&mut self, c: &mut Cur<'f>, mode: Mode) -> Result<Next<'f>, Stop> {
        let kind = self.kind(*c).expect("`next_op` is called before a token");
        let tok = c.tok();
        match kind {
            TokenKind::Punct { char: '$', .. } => {
                self.next(c);
                let Some(second) = self.kind(*c) else {
                    return Ok(Next::Op(Op::Puncts { at: tok, len: 1 }));
                };
                let second_tok = c.tok();
                match second {
                    TokenKind::Open(delim) => {
                        let (_, inner) = self.expect_group(c).expect("the token opens a group");
                        match delim {
                            Delim::Paren => {
                                let (sep, kind) = self.repeat_kind(c)?;
                                let at = self.push_op(mode, Op::Repeat { end: 0, sep, kind });
                                Ok(Next::Open { at, inner })
                            }
                            Delim::Brace => match mode {
                                Mode::Template => match self.metavar_expr(inner) {
                                    Some(op) => Ok(Next::Op(op)),
                                    None => {
                                        Err(DefError::Message("invalid metavariable expression").into())
                                    }
                                },
                                Mode::Pattern => Err(DefError::Message(
                                    "`${}` metavariable expressions are not allowed in matchers",
                                )
                                .into()),
                            },
                            _ => Err(DefError::Message("expected `$()` repetition or `${}` expression")
                                .into()),
                        }
                    }
                    TokenKind::Ident { .. } => {
                        self.next(c);
                        if self.def.text(second_tok) == "crate" {
                            return Ok(Next::Op(Op::Crate(second_tok)));
                        }
                        let (kind, spec) = match mode {
                            Mode::Pattern => self.fragment_spec(c)?,
                            Mode::Template => (None, DefTok(NONE)),
                        };
                        Ok(Next::Op(Op::Var { binder: NONE, kind, name: second_tok, spec }))
                    }
                    TokenKind::Punct { char: '$', .. } => {
                        self.next(c);
                        match mode {
                            Mode::Pattern => Err(DefError::Message(
                                "`$$` is not allowed on the pattern side",
                            )
                            .into()),
                            Mode::Template => Ok(Next::Op(Op::Puncts { at: second_tok, len: 1 })),
                        }
                    }
                    _ => {
                        self.next(c);
                        Err(DefError::Message("expected ident").into())
                    }
                }
            }
            TokenKind::Literal { .. } | TokenKind::Ident { .. } => {
                self.next(c);
                Ok(Next::Op(Op::Tok(tok)))
            }
            TokenKind::Punct { .. } => {
                let len = self.glued(*c).expect("the token is punctuation");
                c.at += u32::from(len);
                Ok(Next::Op(Op::Puncts { at: tok, len }))
            }
            TokenKind::Open(delim) => {
                let (open, inner) = self.expect_group(c).expect("the token opens a group");
                let close = DefTok(inner.end | inner.src.base);
                let at = self.push_op(mode, Op::Group { delim, open, close, end: 0 });
                Ok(Next::Open { at, inner })
            }
            TokenKind::Close(_) => unreachable!("a level's cursor ends before its close"),
        }
    }

    /// `:spec` after a pattern's `$name`.
    fn fragment_spec(&self, c: &mut Cur<'_>) -> Result<(Option<Frag>, DefTok), Stop> {
        let missing = DefError::Message("missing fragment specifier");
        if !self.expect_char(c, &[':']) {
            return Err(missing.into());
        }
        let spec = self.expect_ident(c, false).ok_or(missing)?;
        Ok((Frag::of(self.def.text(spec), self.def.edition(spec)), spec))
    }

    /// rustc's `parse_sep_and_kleene_op`: after `$(..)`, a separator if any and the operator.
    fn repeat_kind(&mut self, c: &mut Cur<'_>) -> Result<(u32, RepKind), Stop> {
        let sep = match self.kleene_or_sep(c)? {
            Ok(kind) => return Ok((NONE, kind)),
            Err(sep) => sep,
        };
        match self.kleene_or_sep(c)? {
            Ok(RepKind::ZeroOrOne) | Err(_) => Err(DefError::InvalidRepeat.into()),
            Ok(kind) => {
                self.program.seps.push(sep);
                Ok((crate::tokens::index(self.program.seps.len() - 1), kind))
            }
        }
    }

    /// The next token as an operator (`Ok`) or a separator (`Err`).
    fn kleene_or_sep(&self, c: &mut Cur<'_>) -> Result<Result<RepKind, Sep>, Stop> {
        let tok = c.tok();
        let sep = match self.kind(*c) {
            Some(TokenKind::Ident { .. }) => Sep::Ident(tok),
            Some(TokenKind::Literal { .. }) => Sep::Literal(tok),
            Some(TokenKind::Punct { char, .. }) => {
                let len = self.glued(*c).expect("the token is punctuation");
                c.at += u32::from(len);
                return Ok(match (len, char) {
                    (1, '*') => Ok(RepKind::ZeroOrMore),
                    (1, '+') => Ok(RepKind::OneOrMore),
                    (1, '?') => Ok(RepKind::ZeroOrOne),
                    (1, '\'') => {
                        let name = self.expect_ident(c, true).ok_or(DefError::InvalidRepeat)?;
                        Err(Sep::Lifetime { quote: tok, name })
                    }
                    _ => Err(Sep::Puncts { at: tok, len }),
                });
            }
            Some(TokenKind::Open(_) | TokenKind::Close(_)) | None => {
                return Err(DefError::InvalidRepeat.into());
            }
        };
        self.next(c);
        Ok(Err(sep))
    }

    /// The inside of `${..}`: one metavariable expression, `None` if it is none.
    fn metavar_expr(&mut self, mut c: Cur<'f>) -> Option<Op> {
        let func = self.expect_ident(&mut c, false)?;
        let (open, mut args) = self.expect_group(&mut c)?;
        if !matches!(self.def.kind(open), TokenKind::Open(Delim::Paren)) {
            return None;
        }
        let op = match self.def.text(func) {
            "ignore" => {
                self.expect_dollar(&mut args)?;
                let name = self.expect_ident(&mut args, false)?;
                Op::Ignore { binder: NONE, name }
            }
            "index" => Op::Index { depth: self.depth(&mut args)? },
            "len" => Op::Len { depth: self.depth(&mut args)? },
            "count" => {
                self.expect_dollar(&mut args)?;
                let name = self.expect_ident(&mut args, false)?;
                // astlog-ignore: expand-hand-extent (the macro language: `${count($x, depth)}`)
                let comma = matches!(self.kind(args), Some(TokenKind::Punct { char: ',', .. }));
                let depth = match comma {
                    true => {
                        self.next(&mut args);
                        self.depth(&mut args)?
                    }
                    false => 0,
                };
                Op::Count { binder: NONE, name, depth }
            }
            "concat" => {
                let start = crate::tokens::index(self.program.concat.len());
                while !args.is_empty() {
                    let elem = match self.kind(args) {
                        Some(TokenKind::Literal { .. }) => {
                            ConcatElem::Literal(self.next(&mut args).expect("peeked"))
                        }
                        _ => {
                            let var =
                                matches!(self.kind(args), Some(TokenKind::Punct { char: '$', .. }));
                            if var {
                                self.next(&mut args);
                            }
                            let name = self.expect_ident(&mut args, true)?;
                            match var {
                                // Resolved with the arm's binders once its pattern is read.
                                true => ConcatElem::Var { binder: NONE - 1, name },
                                false => ConcatElem::Ident(name),
                            }
                        }
                    };
                    self.program.concat.push(elem);
                    // astlog-ignore: expand-hand-extent (the macro language: `${concat(a, $b)}`)
                    if !args.is_empty() && !self.expect_char(&mut args, &[',']) {
                        self.program.concat.truncate(start as usize);
                        return None;
                    }
                }
                let len = crate::tokens::index(self.program.concat.len()) - start;
                if len < 2 {
                    self.program.concat.truncate(start as usize);
                    return None;
                }
                Op::Concat { start, len, func }
            }
            _ => return None,
        };
        args.is_empty().then_some(op)
    }

    fn expect_dollar(&self, c: &mut Cur<'_>) -> Option<()> {
        self.expect_char(c, &['$']).then_some(())
    }

    /// A metavariable expression's depth: none is 0, else an unsuffixed integer literal.
    fn depth(&self, c: &mut Cur<'_>) -> Option<u32> {
        if c.is_empty() {
            return Some(0);
        }
        let tok = self.next(c)?;
        match self.def.kind(tok) {
            TokenKind::Literal { suffix_len: 0, .. } => {
                let digits = self.def.text(tok);
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                // A depth past `u32::MAX` is one no repetition has, as one past `usize` would be.
                Some(digits.parse::<u32>().unwrap_or(u32::MAX))
            }
            _ => None,
        }
    }
}

/// The binder `name` declares among the arm's `binders` names from `names`; [`NONE`] if none.
fn binder_of(all: &[DefTok], def: Def<'_>, names: u32, binders: u32, name: DefTok) -> u32 {
    let names = &all[names as usize..(names + binders) as usize];
    names.iter().position(|&it| def.same_name(it, name)).map_or(NONE, crate::tokens::index)
}

/// After moving `moved` operations before the `item_ops` at `item`, a group's or repetition's
/// end shifted by where its operations went.
fn shift_end(op: &mut Op, item_ops: u32, moved: u32, item: u32) {
    if let Op::Group { end, .. } | Op::Repeat { end, .. } = op {
        // An end inside the item's operations (after `item + item_ops`) moves by `moved`, one
        // inside the args' by `-item_ops`.
        *end = match *end > item + item_ops {
            true => *end - item_ops,
            false => *end + moved,
        };
    }
}

/// How many tokens of punctuation glue into one from `at` (0 if the token is no punctuation):
/// rustc's lexer glues a joint token to the next where they make an operator.
pub(crate) fn glued_len(kind_at: impl Fn(u32) -> Option<TokenKind>, at: u32) -> u8 {
    let Some(TokenKind::Punct { char, spacing }) = kind_at(at) else { return 0 };
    let mut buf = [0u8; 4];
    let mut glued: &str = char.encode_utf8(&mut buf);
    let mut last = spacing;
    let mut len = 1u8;
    while last != Spacing::Alone {
        let Some(TokenKind::Punct { char: next, spacing }) = kind_at(at + u32::from(len)) else {
            break;
        };
        let Some(joined) = glue(glued, next) else { break };
        glued = joined;
        last = spacing;
        len += 1;
    }
    len
}

/// The operator `left` and `right` make, if they make one: rustc's `Token::glue`.
pub(crate) fn glue(left: &str, right: char) -> Option<&'static str> {
    Some(match (left, right) {
        ("=", '=') => "==",
        // astlog-ignore: expand-hand-extent (rustc's operator glue table)
        ("=", '>') => "=>",
        ("<", '=') => "<=",
        // astlog-ignore: expand-hand-extent (the glue table)
        ("<", '<') => "<<",
        ("<", '-') => "<-",
        ("<<", '=') => "<<=",
        (">", '=') => ">=",
        // astlog-ignore: expand-hand-extent (the glue table)
        (">", '>') => ">>",
        (">>", '=') => ">>=",
        ("!", '=') => "!=",
        ("+", '=') => "+=",
        ("-", '=') => "-=",
        // astlog-ignore: expand-hand-extent (the glue table)
        ("-", '>') => "->",
        ("*", '=') => "*=",
        ("/", '=') => "/=",
        ("%", '=') => "%=",
        ("^", '=') => "^=",
        ("&", '=') => "&=",
        ("&", '&') => "&&",
        ("|", '=') => "|=",
        ("|", '|') => "||",
        (".", '.') => "..",
        ("..", '.') => "...",
        ("..", '=') => "..=",
        (":", ':') => "::",
        _ => return None,
    })
}

/// A literal as its source writes it: rustc's `Display` of a literal token.
pub(crate) fn write_literal(
    f: &mut impl fmt::Write,
    kind: LitKind,
    text: &str,
    suffix_len: u32,
) -> fmt::Result {
    let (prefix, hashes, quote) = match kind {
        LitKind::Byte => ("b", 0, "'"),
        LitKind::Char => ("", 0, "'"),
        LitKind::Integer | LitKind::Float | LitKind::Err => ("", 0, ""),
        LitKind::Str => ("", 0, "\""),
        LitKind::ByteStr => ("b", 0, "\""),
        LitKind::CStr => ("c", 0, "\""),
        LitKind::StrRaw(hashes) => ("r", hashes, "\""),
        LitKind::ByteStrRaw(hashes) => ("br", hashes, "\""),
        LitKind::CStrRaw(hashes) => ("cr", hashes, "\""),
    };
    let (inside, suffix) = text.split_at(text.len() - suffix_len as usize);
    f.write_str(prefix)?;
    for _ in 0..hashes {
        f.write_char('#')?;
    }
    f.write_str(quote)?;
    f.write_str(inside)?;
    f.write_str(quote)?;
    for _ in 0..hashes {
        f.write_char('#')?;
    }
    f.write_str(suffix)
}

/// Where a diagnostic of a call points: a token of the call or the definition, or the call
/// site.
pub(crate) type At = TokenOrigin;

/// The call site: a token the engine makes.
pub(crate) const SITE: At = TokenOrigin::Made(Transparency::Transparent);

/// The words of a binding error, written only for the error reported.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Msg {
    Static(&'static str),
    /// The fragment parser's message: `Engine::parser_texts[start..start + len]`.
    Parser { start: u32, len: u32 },
    ExpectedLiteral(DefTok),
    ExpectedIdent(DefTok),
    ExpectedCrate,
    ExpectedPunct { at: DefTok, len: u8 },
    NestedNotFound(DefTok),
    EmptyNotFound(DefTok),
    FoundNested(DefTok),
    FoundEmpty(DefTok),
    /// A name no binder declares, in `${count(..)}`.
    Unresolved(DefTok),
}

/// An error of a call's match or transcription: rustc's error kinds, where.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CallError {
    NoMatchingRule(At),
    Binding(At, Msg),
    LeftoverTokens(At),
    LimitExceeded,
    UnexpectedToken(At),
}

/// The engine's refusal of a call past a bound.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Bound(pub(crate) RulesLimit);

/// Counts the steps of one call.
pub(crate) struct Budget {
    steps: u64,
}

impl Budget {
    #[inline]
    pub(crate) fn step(&mut self) -> Result<(), Bound> {
        self.steps += 1;
        match self.steps > STEP_BOUND {
            true => Err(Bound(RulesLimit::Steps)),
            false => Ok(()),
        }
    }
}

/// Expands the call in `out` by the compiled `program`.
pub(crate) fn expand(
    program: &Program<'_>,
    out: &mut Out<'_, '_>,
    _balanced: Checked,
) -> Result<(), ExpandError> {
    if let Some(error) = program.error {
        let message = out.note_display(&error)?;
        out.diagnose(Diagnostic::BadDefinition { message });
        return Ok(());
    }
    let call = out.call;
    let style = match call.kind {
        CallKind::FnLike { .. } => Style::FnLike,
        CallKind::Attr { .. } => Style::Attr,
        CallKind::Derive { .. } => Style::Derive,
    };
    let (def, content) = (Def::of(out.def), out.def.content());
    let refused = |Bound(bound): Bound, out: &Out<'_, '_>| ExpandError::RulesBound {
        bound,
        def: content,
        call: Some(call_key(out.def, out.call)),
    };
    let result = blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        let mut engine = Engine::new_in(program, def, frame);
        engine.run(style, out)
    });
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(bound) => Err(refused(bound, out)),
    }
}

/// A call being expanded: the program, the definition, and the call's scratch in its frame.
pub(crate) struct Engine<'p, 'd, 'f> {
    pub(crate) program: &'p Program<'p>,
    pub(crate) def: Def<'d>,
    pub(crate) frame: &'f Frame,
    pub(crate) budget: Budget,
    /// The fragment parser's messages of errors kept, by [`Msg::Parser`].
    pub(crate) parser_texts: Vec<u8, &'f Frame>,
}

impl<'p, 'd, 'f> Engine<'p, 'd, 'f> {
    fn new_in(program: &'p Program<'p>, def: Def<'d>, frame: &'f Frame) -> Self {
        Engine { program, def, frame, budget: Budget { steps: 0 }, parser_texts: Vec::new_in(frame) }
    }

    /// The call: the first arm that matches and transcribes, else the error rustc gives.
    fn run(
        &mut self,
        style: Style,
        out: &mut Out<'_, '_>,
    ) -> Result<Result<(), ExpandError>, Bound> {
        let call = out.call;
        let any = self.program.arms.iter().any(|arm| arm.style == style);
        if !any {
            let rules = crate::tokens::index(self.program.arms.len());
            return Ok(diagnose(out, CallError::NoMatchingRule(SITE), rules, self));
        }
        let mut view = input::View::new_in(call, style, self.frame);
        let mut buffers = matcher::Buffers::new_in(self.frame);
        let mut scratch = matcher::Match::new_in(self.frame);
        let mut tx = transcribe::Scratch::new_in(self.frame);
        let start = out.exp.tokens.mark();
        // The arms that can match, in order: the first that matches and transcribes is the
        // call's, as in rustc.
        let mut lin = linear::Linear::new_in(self.program, self.frame);
        let oracle = ORACLE.load(Ordering::Relaxed);
        if oracle {
            ORACLE_COUNTS[4].fetch_add(1, Ordering::Relaxed);
        }
        for idx in 0..self.program.arms.len() {
            let arm = self.program.arms[idx];
            if arm.style != style || !view.admits(self.def, arm.first) {
                continue;
            }
            let outcome = linear::match_(self, idx, &mut view, &mut lin)?;
            if oracle {
                match outcome {
                    linear::Outcome::Exact => {
                        ORACLE_COUNTS[2].fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {
                        let checked = self.oracle_arm(
                            &arm, outcome, &mut view, &mut buffers, &mut scratch, &lin, &mut tx, out,
                        )?;
                        if let Err(error) = checked {
                            return Ok(Err(error));
                        }
                    }
                }
            }
            let written = match outcome {
                linear::Outcome::Failed => continue,
                linear::Outcome::Matched => {
                    transcribe::transcribe(self, &arm, &view, &lin.flat, out, &mut tx)?
                }
                linear::Outcome::Exact => {
                    matcher::match_(self, &arm, &mut view, &mut buffers, &mut scratch, None)?;
                    if scratch.err.is_some() {
                        continue;
                    }
                    transcribe::transcribe(self, &arm, &view, &scratch.bindings, out, &mut tx)?
                }
            };
            match written {
                Ok(None) => {
                    out.exp.arm = Some(crate::tokens::index(idx));
                    return Ok(Ok(()));
                }
                Ok(Some(_)) => tx.truncate(out, start),
                Err(error) => return Ok(Err(error)),
            }
        }
        // No arm gave the expansion: every arm of the style again, as rustc's expander tries
        // them, for the error of the best. An arm that matched failed to transcribe above, so
        // it is matched again only. This is the exact mode: the position-set matcher, which
        // keeps every position's first error and what it had bound.
        if oracle {
            ORACLE_COUNTS[3].fetch_add(1, Ordering::Relaxed);
        }
        let mut best: Option<(matcher::Match<'f>, usize)> = None;
        for idx in 0..self.program.arms.len() {
            let arm = self.program.arms[idx];
            if arm.style != style {
                continue;
            }
            matcher::match_(self, &arm, &mut view, &mut buffers, &mut scratch, None)?;
            match &mut best {
                Some((prev, prev_idx)) => {
                    let better = (scratch.unmatched, -(scratch.bound as i64))
                        < (prev.unmatched, -(prev.bound as i64));
                    if better {
                        core::mem::swap(prev, &mut scratch);
                        *prev_idx = idx;
                    }
                }
                None => {
                    let taken = core::mem::replace(&mut scratch, matcher::Match::new_in(self.frame));
                    best = Some((taken, idx));
                }
            }
        }
        let rules = crate::tokens::index(self.program.arms.len());
        let (best, idx) = best.expect("an arm of the style was tried");
        let arm = self.program.arms[idx];
        let error = match best.err {
            Some(matcher::MatchError::Fragment(error)) => error,
            Some(error) => {
                match failure::match_failure(self, style, &mut view, &mut buffers, &mut scratch, out)? {
                    Ok(Some(found)) => {
                        out.diagnose(found);
                        return Ok(Ok(()));
                    }
                    Ok(None) => error.build(),
                    Err(error) => return Ok(Err(error)),
                }
            }
            None => {
                match transcribe::transcribe(self, &arm, &view, &best.bindings, out, &mut tx)? {
                    Ok(Some(error)) => {
                        tx.truncate(out, start);
                        error
                    }
                    Ok(None) => unreachable!("an arm that matches and transcribes was found first"),
                    Err(error) => return Ok(Err(error)),
                }
            }
        };
        Ok(diagnose(out, error, rules, self))
    }

    /// The oracle's check of one arm: the position-set matcher on it too, then whether both
    /// matched, the bindings node by node, and (transcribed from each) the tokens written,
    /// their origins and the transcription's error. Leaves the expansion as it found it.
    #[expect(clippy::too_many_arguments, reason = "the engine's state, passed as its parts")]
    fn oracle_arm(
        &mut self,
        arm: &Arm,
        outcome: linear::Outcome,
        view: &mut input::View<'_, 'f>,
        buffers: &mut matcher::Buffers<'f>,
        scratch: &mut matcher::Match<'f>,
        lin: &linear::Linear<'_, 'f>,
        tx: &mut transcribe::Scratch<'f>,
        out: &mut Out<'_, '_>,
    ) -> Result<Result<(), ExpandError>, Bound> {
        matcher::match_(self, arm, view, buffers, scratch, None)?;
        let old = scratch.err.is_none();
        let mut equal = old == (outcome == linear::Outcome::Matched);
        if equal && old {
            equal = linear::same(&scratch.bindings, &lin.flat, arm.binders, self.frame);
            let start = out.exp.tokens.mark();
            let a = match transcribe::transcribe(self, arm, view, &scratch.bindings, out, tx)? {
                Ok(error) => error,
                Err(error) => return Ok(Err(error)),
            };
            let mut wrote = Vec::new_in(self.frame);
            for at in start.tokens..out.exp.tokens.len() {
                wrote.push((out.exp.tokens.tokens()[at].kind, out.exp.origins[at]));
            }
            tx.truncate(out, start);
            let b = match transcribe::transcribe(self, arm, view, &lin.flat, out, tx)? {
                Ok(error) => error,
                Err(error) => return Ok(Err(error)),
            };
            equal &= a == b && out.exp.tokens.len() - start.tokens == wrote.len();
            equal = equal
                && wrote.iter().enumerate().all(|(i, &(kind, origin))| {
                    let at = start.tokens + i;
                    out.exp.tokens.tokens()[at].kind == kind && out.exp.origins[at] == origin
                });
            tx.truncate(out, start);
        }
        ORACLE_COUNTS[if equal { 0 } else { 1 }].fetch_add(1, Ordering::Relaxed);
        Ok(Ok(()))
    }

    /// The text of a kept parser message.
    pub(crate) fn parser_text(&self, start: u32, len: u32) -> &str {
        let bytes = &self.parser_texts[start as usize..(start + len) as usize];
        core::str::from_utf8(bytes).expect("a parser message is kept whole")
    }

    /// Keeps a parser message for the error that names it.
    pub(crate) fn keep_text(&mut self, text: &str) -> Msg {
        let start = crate::tokens::index(self.parser_texts.len());
        self.parser_texts.extend_from_slice(text.as_bytes());
        Msg::Parser { start, len: crate::tokens::index(text.len()) }
    }
}

/// The call's error as the expansion's diagnostic.
fn diagnose(
    out: &mut Out<'_, '_>,
    error: CallError,
    rules: u32,
    engine: &Engine<'_, '_, '_>,
) -> Result<(), ExpandError> {
    let diagnostic = match error {
        CallError::NoMatchingRule(at) => {
            let expected = out.note("")?;
            Diagnostic::NoRuleMatched { rules, closest: None, token: None, expected, at: Some(at) }
        }
        CallError::Binding(at, msg) => {
            let message = out.note_display(&MsgText { msg, engine })?;
            Diagnostic::Binding { message, at: Some(at) }
        }
        CallError::LeftoverTokens(at) => Diagnostic::LeftoverTokens { at: Some(at) },
        CallError::LimitExceeded => Diagnostic::LimitExceeded,
        CallError::UnexpectedToken(at) => Diagnostic::UnexpectedToken { at: Some(at) },
    };
    out.diagnose(diagnostic);
    Ok(())
}

/// A [`Msg`] as its words.
struct MsgText<'e, 'p, 'd, 'f> {
    msg: Msg,
    engine: &'e Engine<'p, 'd, 'f>,
}

impl fmt::Display for MsgText<'_, '_, '_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let def = self.engine.def;
        match self.msg {
            Msg::Static(text) => f.write_str(text),
            Msg::Parser { start, len } => f.write_str(self.engine.parser_text(start, len)),
            Msg::ExpectedLiteral(tok) => {
                f.write_str("expected literal: `")?;
                let TokenKind::Literal { kind, suffix_len, .. } = def.kind(tok) else {
                    unreachable!("a literal location is a literal")
                };
                write_literal(f, kind, def.text(tok), suffix_len)?;
                f.write_str("`")
            }
            Msg::ExpectedIdent(tok) => {
                let raw = matches!(def.kind(tok), TokenKind::Ident { raw: Raw::Yes, .. });
                write!(f, "expected ident: `{}{}`", if raw { "r#" } else { "" }, def.text(tok))
            }
            Msg::ExpectedCrate => f.write_str("expected ident: `$crate`"),
            Msg::ExpectedPunct { at, len } => {
                f.write_str("expected punct: `")?;
                for i in 0..u32::from(len) {
                    f.write_char(def.char(DefTok(at.0 + i)))?;
                }
                f.write_str("`")
            }
            Msg::NestedNotFound(name) => {
                write!(f, "could not find nested binding `{}`", def.text(name))
            }
            Msg::EmptyNotFound(name) => write!(f, "could not find empty binding `{}`", def.text(name)),
            Msg::FoundNested(name) => {
                write!(f, "expected simple binding, found nested binding `{}`", def.text(name))
            }
            Msg::FoundEmpty(name) => {
                write!(f, "expected simple binding, found empty binding `{}`", def.text(name))
            }
            Msg::Unresolved(name) => f.write_str(def.text(name)),
        }
    }
}
