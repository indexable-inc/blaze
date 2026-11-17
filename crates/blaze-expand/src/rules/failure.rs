//! Why no arm matched: every arm of the call's style matched again with a tracker that keeps
//! the furthest any got, the token there and what the arm expected (rustc's
//! `failed_to_match_macro`). Run only for a call that fails, so a call that expands pays nothing.

use blaze_mem::frame::Frame;
use blaze_own::Vec;
use core::fmt::{self, Write as _};

use super::{
    At, Bound, Def, DefTok, Engine, Loc, NONE, Program, SITE, Sep, Style,
    input::{Level, View},
    matcher::{self, Buffers, Match},
    write_literal,
};
use crate::{
    error::ExpandError,
    expand::Out,
    expansion::Diagnostic,
    tokens::{Delim, Edition, Raw, Spacing, TokenKind},
};

/// A location of an arm, as the tracker last saw it.
#[derive(Clone, Copy, Debug)]
struct Seen {
    dot: u32,
    sep_matched: bool,
}

/// The input token where a match stopped, described when it is reported.
#[derive(Clone, Copy, Debug)]
enum Found {
    Puncts { at: u32, len: u8 },
    Lifetime { name: u32 },
    Ident(u32),
    Literal(u32),
    Delim { delim: Delim, closing: bool },
}

#[derive(Clone, Copy, Debug)]
struct Best {
    position: u32,
    found: Option<Found>,
    at: At,
    remaining: Seen,
    /// The rule (definition order) that got here.
    arm: u32,
}

/// The tracker the matcher calls at each location and token.
pub(crate) struct Tracker<'f> {
    position: u32,
    closes: Vec<(Delim, At), &'f Frame>,
    remaining: Option<Seen>,
    best: Option<Best>,
    /// The rule being matched now (set by [`match_failure`] before each).
    arm: u32,
}

impl<'f> Tracker<'f> {
    fn new_in(frame: &'f Frame) -> Self {
        Tracker { position: 0, closes: Vec::new_in(frame), remaining: None, best: None, arm: 0 }
    }

    pub(crate) fn start(&mut self) {
        self.position = 0;
        self.closes.clear();
    }

    pub(crate) fn before_loc(&mut self, program: &Program<'_>, dot: u32, sep_matched: bool, last: bool) {
        if self.remaining.is_none() || (last && program.locs[dot as usize] != Loc::Eof) {
            self.remaining = Some(Seen { dot, sep_matched });
        }
    }

    pub(crate) fn bump(&mut self, tokens: u32) {
        self.position += tokens;
    }

    pub(crate) fn step(&mut self, view: &View<'_, '_>, next: Option<u32>) {
        self.position += 1;
        match next {
            Some(at) => {
                if let TokenKind::Open(delim) = view.kind(at) {
                    self.closes.push((delim, view.origin(view.close_of(at))));
                }
            }
            None => {
                self.closes.pop();
            }
        }
    }

    pub(crate) fn fail(&mut self, view: &View<'_, '_>, src: Option<Level>) {
        if self.best.as_ref().is_some_and(|best| self.position <= best.position) {
            return;
        }
        let remaining = self.remaining.expect("every step goes through a location");
        let (found, at) = match src {
            None => (None, SITE),
            Some(src) => {
                let (found, at) = self.token_at(view, src);
                (Some(found), at)
            }
        };
        self.best = Some(Best { position: self.position, found, at, remaining, arm: self.arm });
    }

    fn token_at(&self, view: &View<'_, '_>, src: Level) -> (Found, At) {
        if src.is_empty() {
            let &(delim, at) = self.closes.last().expect("the input ends in a group");
            return (Found::Delim { delim, closing: true }, at);
        }
        let at = src.at;
        match view.kind(at) {
            TokenKind::Punct { char, .. } => {
                let len = view.glued(at, src.end);
                if len == 1
                    && char == '\''
                    && at + 1 < src.end
                    && matches!(view.kind(at + 1), TokenKind::Ident { .. })
                {
                    return (Found::Lifetime { name: at + 1 }, view.origin(at));
                }
                (Found::Puncts { at, len }, view.origin(at))
            }
            TokenKind::Ident { .. } => (Found::Ident(at), view.origin(at)),
            TokenKind::Literal { .. } => (Found::Literal(at), view.origin(at)),
            TokenKind::Open(delim) => (Found::Delim { delim, closing: false }, view.origin(at)),
            TokenKind::Close(_) => unreachable!("a level ends before its close"),
        }
    }
}

/// The tokens rustc's parser would count in `from..to`: a group two, glued punctuation one.
pub(crate) fn tokens(view: &View<'_, '_>, from: u32, to: u32) -> u32 {
    let mut count = 0;
    let mut joined = false;
    for at in from..to {
        match view.kind(at) {
            TokenKind::Open(_) => {
                count += 2;
                joined = false;
            }
            TokenKind::Close(_) => {}
            kind => {
                if !joined {
                    count += 1;
                }
                joined = matches!(kind, TokenKind::Punct { spacing: Spacing::Joint, .. });
            }
        }
    }
    count
}

/// The error of a call no arm matched: the best of every arm's failures, if any made one.
pub(crate) fn match_failure<'f>(
    engine: &mut Engine<'_, '_, 'f>,
    style: Style,
    view: &mut View<'_, 'f>,
    buffers: &mut Buffers<'f>,
    scratch: &mut Match<'f>,
    out: &mut Out<'_, '_>,
) -> Result<Result<Option<Diagnostic>, ExpandError>, Bound> {
    let mut track = Tracker::new_in(engine.frame);
    for idx in 0..engine.program.arms.len() {
        let arm = engine.program.arms[idx];
        if arm.style == style {
            track.arm = crate::tokens::index(idx);
            matcher::match_(engine, &arm, view, buffers, scratch, Some(&mut track))?;
        }
    }
    let Some(best) = track.best else { return Ok(Ok(None)) };
    let def = engine.def;
    let rules = crate::tokens::index(engine.program.arms.len());
    let token = match best.found {
        Some(found) => match out.note_display(&FoundText { found, view }) {
            Ok(text) => Some(text),
            Err(error) => return Ok(Err(error)),
        },
        None => None,
    };
    let expected = match out.note_display(&Describe { program: engine.program, def, seen: best.remaining }) {
        Ok(text) => text,
        Err(error) => return Ok(Err(error)),
    };
    Ok(Ok(Some(Diagnostic::NoRuleMatched { rules, closest: Some(best.arm), token, expected, at: Some(best.at) })))
}

/// Which keyword an identifier is, for its description.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Keyword {
    ReservedIdent,
    Used,
    Unused,
}

fn keyword(text: &str, edition: Edition) -> Option<Keyword> {
    const USED: &[&str] = &[
        "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
        "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
        "use", "where", "while",
    ];
    const UNUSED: &[&str] = &[
        "abstract", "become", "box", "do", "final", "macro", "override", "priv", "typeof",
        "unsized", "virtual", "yield",
    ];
    let at_least_2018 = edition >= Edition::E2018;
    if matches!(text, "_" | "$crate") {
        Some(Keyword::ReservedIdent)
    } else if USED.contains(&text) || (at_least_2018 && matches!(text, "async" | "await" | "dyn")) {
        Some(Keyword::Used)
    } else if UNUSED.contains(&text)
        || (at_least_2018 && text == "try")
        || (edition >= Edition::E2024 && text == "gen")
    {
        Some(Keyword::Unused)
    } else {
        None
    }
}

fn write_ident(f: &mut fmt::Formatter<'_>, text: &str, raw: Raw, edition: Edition) -> fmt::Result {
    let prefix = if raw == Raw::Yes { "r#" } else { "" };
    let kind = match raw {
        Raw::Yes => None,
        Raw::No => keyword(text, edition),
    };
    match kind {
        Some(Keyword::ReservedIdent) => write!(f, "reserved identifier `{prefix}{text}`"),
        Some(Keyword::Used) => write!(f, "keyword `{prefix}{text}`"),
        Some(Keyword::Unused) => write!(f, "reserved keyword `{prefix}{text}`"),
        None => write!(f, "`{prefix}{text}`"),
    }
}

fn delimiter_descr(delim: Delim, closing: bool) -> &'static str {
    match (delim, closing) {
        (Delim::Paren, false) => "`(`",
        (Delim::Paren, true) => "`)`",
        (Delim::Brace, false) => "`{`",
        (Delim::Brace, true) => "`}`",
        (Delim::Bracket, false) => "`[`",
        (Delim::Bracket, true) => "`]`",
        (Delim::Invisible, _) => "metavariable",
    }
}

fn raw_of(kind: TokenKind) -> Raw {
    match kind {
        TokenKind::Ident { raw, .. } => raw,
        _ => Raw::No,
    }
}

struct FoundText<'v, 'c, 'f> {
    found: Found,
    view: &'v View<'c, 'f>,
}

impl fmt::Display for FoundText<'_, '_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let view = self.view;
        match self.found {
            Found::Puncts { at, len } => {
                f.write_str("`")?;
                for i in 0..u32::from(len) {
                    f.write_char(view.char(at + i).expect("glued punctuation"))?;
                }
                f.write_str("`")
            }
            Found::Lifetime { name } => write!(f, "`'{}`", view.text(name)),
            Found::Ident(at) => write_ident(f, view.text(at), raw_of(view.kind(at)), view.edition(at)),
            Found::Literal(at) => {
                let TokenKind::Literal { kind, suffix_len, .. } = view.kind(at) else {
                    unreachable!("found a literal")
                };
                f.write_str("`")?;
                write_literal(f, kind, view.text(at), suffix_len)?;
                f.write_str("`")
            }
            Found::Delim { delim, closing } => f.write_str(delimiter_descr(delim, closing)),
        }
    }
}

/// What the arm expected at a location.
struct Describe<'p, 'd> {
    program: &'p Program<'p>,
    def: Def<'d>,
    seen: Seen,
}

fn write_def_literal(f: &mut fmt::Formatter<'_>, def: Def<'_>, tok: DefTok) -> fmt::Result {
    let TokenKind::Literal { kind, suffix_len, .. } = def.kind(tok) else {
        unreachable!("a literal location is a literal")
    };
    f.write_str("`")?;
    write_literal(f, kind, def.text(tok), suffix_len)?;
    f.write_str("`")
}

fn write_def_puncts(f: &mut fmt::Formatter<'_>, def: Def<'_>, at: DefTok, len: u8) -> fmt::Result {
    f.write_str("`")?;
    for i in 0..u32::from(len) {
        f.write_char(def.char(DefTok(at.0 + i)))?;
    }
    f.write_str("`")
}

impl fmt::Display for Describe<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let def = self.def;
        match self.program.locs[self.seen.dot as usize] {
            Loc::Literal(tok) => write_def_literal(f, def, tok),
            Loc::Ident(tok) => write_ident(f, def.text(tok), raw_of(def.kind(tok)), def.edition(tok)),
            Loc::Crate(tok) => write_ident(f, "$crate", Raw::No, def.edition(tok)),
            Loc::Punct { at, len } => write_def_puncts(f, def, at, len),
            Loc::Var { name, spec, .. } => {
                write!(f, "meta-variable `${}:{}`", def.text(name), def.text(spec))
            }
            Loc::Subtree(_) => f.write_str("delimiter"),
            Loc::Open(delim) => f.write_str(delimiter_descr(delim, false)),
            Loc::Close(delim) => f.write_str(delimiter_descr(delim, true)),
            Loc::RepStart(_) => f.write_str("sequence start"),
            Loc::RepEnd(rep) => {
                let rep = self.program.reps[rep as usize];
                if rep.sep == NONE || self.seen.sep_matched {
                    return f.write_str("sequence end");
                }
                match self.program.seps[rep.sep as usize] {
                    Sep::Literal(tok) => write_def_literal(f, def, tok),
                    Sep::Ident(tok) => {
                        write_ident(f, def.text(tok), raw_of(def.kind(tok)), def.edition(tok))
                    }
                    Sep::Puncts { at, len } => write_def_puncts(f, def, at, len),
                    Sep::Lifetime { name, .. } => write!(f, "`'{}`", def.text(name)),
                }
            }
            Loc::Eof => f.write_str("end of macro"),
        }
    }
}
