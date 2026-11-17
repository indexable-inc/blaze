//! A toolchain crate's implicit sysroot dependency: a fact of its own text, read once by the
//! cutter (scripts/src/bin/toolchain_cut.rs) and written into the manifest as data, one value
//! per unit and target ([`ImplicitSysroot`], the unit row's `sysroot`), and as real dependency
//! edges of that unit ([`implied_edges`]). So a reader opens Rust's `std` exactly when a crate
//! of its closure names it, by walking edges, with no special case; and which sysroot crates a
//! crate is given is read from its row's edges, in one home.
//!
//! The Rust Reference, "Preludes", "Extern prelude": "`core` is always added to the extern
//! prelude. `std` is added as long as the `no_std` attribute is not specified in the crate
//! root"; "The `no_std` attribute": it "prevents `std` from being added to the extern prelude"
//! and makes the standard prelude `core`'s; "The `no_core` attribute": it "prevents `core`
//! from being added to the extern prelude" (and with it every injected crate). `extern crate`:
//! "If the `extern crate` appears in the crate root, then the crate name is also added to the
//! extern prelude"; its crate is found in the sysroot when no `--extern` of that name is given
//! (so an explicit dependency of the same name wins, here and in [`implied_edges`]).
//!
//! What is read, from the ROOT FILE's text only (sans-IO: the caller hands the text in):
//! - its inner attributes `#![no_std]`, `#![no_core]`, and `#![cfg_attr(pred, ..)]` holding
//!   them (nested `cfg_attr` too), the predicate evaluated under the unit's cfg: the target's
//!   atoms as the toolchain records them and `feature = ".."` per feature on. `test` is not
//!   among them (a library unit is no test). A predicate naming `debug_assertions` is refused
//!   by name: the cut is the same for every profile, so its value is not known here.
//! - its root module's `extern crate core|alloc|std` items (not `extern crate self as std`),
//!   each under its outer `#[cfg(..)]` (and `#[cfg_attr(.., cfg(..))]`) attributes. One in a
//!   nested module or another file is not seen: the names run then reports that crate's
//!   `std` by name if it is reached; it never resolves silently.
//!
//! Only attributes that can change the answer have their predicates evaluated: a
//! `#![cfg_attr(debug_assertions, allow(dead_code))]` is not refused.

use blaze_own::{AVec, Alloc, AllocFailure};

/// What a unit's root gives it from the sysroot with nothing in its manifest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum ImplicitSysroot {
    /// `#![no_core]`: nothing (no `core`, no `std`, no prelude).
    None = 1,
    /// `#![no_std]`: `core`, and `core`'s prelude.
    Core = 2,
    /// Neither: `core`, `std`, and `std`'s prelude.
    Std = 3,
}

impl ImplicitSysroot {
    /// The value whose byte in the record is `byte`.
    pub fn of(byte: u8) -> Option<ImplicitSysroot> {
        [ImplicitSysroot::None, ImplicitSysroot::Core, ImplicitSysroot::Std].into_iter().find(|s| *s as u8 == byte)
    }

    /// As the record's documentation and a refusal name it.
    pub const fn name(self) -> &'static str {
        match self {
            ImplicitSysroot::None => "none",
            ImplicitSysroot::Core => "core",
            ImplicitSysroot::Std => "std",
        }
    }
}

/// What a root's text says of the sysroot ([`root_facts`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RootFacts {
    pub sysroot: ImplicitSysroot,
    /// A live `extern crate core;` in the root module.
    pub extern_core: bool,
    /// A live `extern crate alloc;` in the root module.
    pub extern_alloc: bool,
    /// A live `extern crate std;` in the root module.
    pub extern_std: bool,
}

/// Why a root's text could not be read for its sysroot. `line` is 1-based in the root file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImpliedRefused<'t> {
    /// A comment, string, character or bracket that does not close.
    Unclosed { what: &'static str, line: u32 },
    /// A cfg predicate this reader does not take, with the reason.
    Predicate { why: &'static str, line: u32 },
    /// A predicate word that is no `all`, `any` or `not`.
    UnknownPredicate { name: &'t str, line: u32 },
    /// A profile's atom: the cut is one for every profile.
    ProfileAtom { name: &'t str, line: u32 },
    /// A cfg value written with an escape or as no plain string.
    ValueForm { line: u32 },
    /// Predicates or `cfg_attr`s nested past [`MAX_DEPTH`].
    TooDeep { line: u32 },
}

impl core::fmt::Display for ImpliedRefused<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ImpliedRefused::Unclosed { what, line } => write!(f, "line {line}: a {what} that does not close"),
            ImpliedRefused::Predicate { why, line } => write!(f, "line {line}: {why}"),
            ImpliedRefused::UnknownPredicate { name, line } => write!(f, "line {line}: `{name}(..)` is no cfg predicate (`all`, `any`, `not`)"),
            ImpliedRefused::ProfileAtom { name, line } => {
                write!(f, "line {line}: the sysroot a crate is given depends on `{name}`, a profile's atom; the toolchain is cut once for every profile")
            }
            ImpliedRefused::ValueForm { line } => write!(f, "line {line}: a cfg value that is not a plain string"),
            ImpliedRefused::TooDeep { line } => write!(f, "line {line}: cfg predicates or `cfg_attr`s nested past {MAX_DEPTH}"),
        }
    }
}

impl core::error::Error for ImpliedRefused<'_> {}

/// How deep predicates nest, and how many attributes one `cfg_attr` tree may hold.
pub const MAX_DEPTH: usize = 32;

fn line_of(text: &str, at: usize) -> u32 {
    1 + text.as_bytes()[..at.min(text.len())].iter().filter(|b| **b == b'\n').count() as u32
}

/// One token of Rust's lexical grammar, as far as attributes and item heads need it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tok<'t> {
    Ident(&'t str),
    /// A plain or raw string literal with no escape in it: its text.
    Str(&'t str),
    /// Any other literal (an escaped, byte or C string, a character, a number) or a lifetime.
    Lit,
    Punct(u8),
    Open(u8),
    Close(u8),
}

fn ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// A lexer over `text[at..end]`.
#[derive(Clone, Copy)]
struct Lexer<'t> {
    text: &'t str,
    at: usize,
    end: usize,
}

impl<'t> Lexer<'t> {
    fn byte(&self, at: usize) -> Option<u8> {
        if at < self.end { Some(self.text.as_bytes()[at]) } else { None }
    }

    fn line(&self, at: usize) -> u32 {
        line_of(self.text, at)
    }

    /// Past whitespace and comments (inner and outer doc comments are attributes that give no
    /// sysroot, so they are passed like comments).
    fn trivia(&mut self) -> Result<(), ImpliedRefused<'t>> {
        loop {
            match (self.byte(self.at), self.byte(self.at + 1)) {
                (Some(c), _) if c.is_ascii_whitespace() => self.at += 1,
                (Some(b'/'), Some(b'/')) => {
                    while let Some(c) = self.byte(self.at) {
                        if c == b'\n' {
                            break;
                        }
                        self.at += 1;
                    }
                }
                (Some(b'/'), Some(b'*')) => {
                    let start = self.at;
                    self.at += 2;
                    let mut depth = 1u32;
                    while depth > 0 {
                        match (self.byte(self.at), self.byte(self.at + 1)) {
                            (None, _) => return Err(ImpliedRefused::Unclosed { what: "block comment", line: self.line(start) }),
                            (Some(b'/'), Some(b'*')) => {
                                depth += 1;
                                self.at += 2;
                            }
                            (Some(b'*'), Some(b'/')) => {
                                depth -= 1;
                                self.at += 2;
                            }
                            _ => self.at += 1,
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    /// A `"..."` whose opening quote is at `self.at`: its text and whether it holds an escape.
    fn quoted(&mut self) -> Result<(&'t str, bool), ImpliedRefused<'t>> {
        let open = self.at;
        self.at += 1;
        let mut escaped = false;
        loop {
            match self.byte(self.at) {
                None => return Err(ImpliedRefused::Unclosed { what: "string", line: self.line(open) }),
                Some(b'\\') => {
                    escaped = true;
                    self.at += 2;
                }
                Some(b'"') => {
                    self.at += 1;
                    return Ok((&self.text[open + 1..self.at - 1], escaped));
                }
                Some(_) => self.at += 1,
            }
        }
    }

    /// The hashes at `self.at` followed by `"`: their count, or `None` when no quote follows.
    fn raw_hashes(&self) -> Option<usize> {
        let mut n = 0;
        while self.byte(self.at + n) == Some(b'#') {
            n += 1;
        }
        (self.byte(self.at + n) == Some(b'"')).then_some(n)
    }

    /// A raw string whose hashes (`hashes` of them) start at `self.at`: its text.
    fn raw(&mut self, hashes: usize) -> Result<&'t str, ImpliedRefused<'t>> {
        let open = self.at;
        self.at += hashes + 1;
        let start = self.at;
        loop {
            match self.byte(self.at) {
                None => return Err(ImpliedRefused::Unclosed { what: "raw string", line: self.line(open) }),
                Some(b'"') if (1..=hashes).all(|k| self.byte(self.at + k) == Some(b'#')) => {
                    let text = &self.text[start..self.at];
                    self.at += 1 + hashes;
                    return Ok(text);
                }
                Some(_) => self.at += 1,
            }
        }
    }

    /// A character literal or a lifetime whose `'` is at `self.at`.
    fn quote(&mut self) -> Result<(), ImpliedRefused<'t>> {
        let open = self.at;
        self.at += 1;
        if self.byte(self.at) == Some(b'\\') {
            self.at += 2;
            loop {
                match self.byte(self.at) {
                    None => return Err(ImpliedRefused::Unclosed { what: "character", line: self.line(open) }),
                    Some(b'\'') => {
                        self.at += 1;
                        return Ok(());
                    }
                    Some(_) => self.at += 1,
                }
            }
        }
        let Some(first) = self.byte(self.at) else { return Err(ImpliedRefused::Unclosed { what: "character", line: self.line(open) }) };
        let width = match first {
            0..0x80 => 1,
            0xC0..0xE0 => 2,
            0xE0..0xF0 => 3,
            _ => 4,
        };
        if self.byte(self.at + width) == Some(b'\'') {
            self.at += width + 1;
            return Ok(());
        }
        // A lifetime or a label.
        while self.byte(self.at).is_some_and(ident_byte) {
            self.at += 1;
        }
        Ok(())
    }

    /// The next token and where it starts.
    fn next(&mut self) -> Result<Option<(Tok<'t>, usize)>, ImpliedRefused<'t>> {
        self.trivia()?;
        let start = self.at;
        let Some(c) = self.byte(start) else { return Ok(None) };
        let tok = match c {
            b'"' => match self.quoted()? {
                (text, false) => Tok::Str(text),
                (_, true) => Tok::Lit,
            },
            b'\'' => {
                self.quote()?;
                Tok::Lit
            }
            b'0'..=b'9' => {
                while self.byte(self.at).is_some_and(ident_byte) {
                    self.at += 1;
                }
                Tok::Lit
            }
            c if ident_byte(c) => {
                while self.byte(self.at).is_some_and(ident_byte) {
                    self.at += 1;
                }
                let word = &self.text[start..self.at];
                match (word, self.byte(self.at)) {
                    ("b" | "c", Some(b'"')) => {
                        self.quoted()?;
                        Tok::Lit
                    }
                    ("b", Some(b'\'')) => {
                        self.quote()?;
                        Tok::Lit
                    }
                    ("r" | "br" | "cr", Some(b'"' | b'#')) => match self.raw_hashes() {
                        Some(hashes) => {
                            let text = self.raw(hashes)?;
                            if word == "r" { Tok::Str(text) } else { Tok::Lit }
                        }
                        // `r#ident`: a raw identifier.
                        None if word == "r" && self.byte(self.at + 1).is_some_and(ident_byte) => {
                            self.at += 1;
                            let name = self.at;
                            while self.byte(self.at).is_some_and(ident_byte) {
                                self.at += 1;
                            }
                            Tok::Ident(&self.text[name..self.at])
                        }
                        None => Tok::Ident(word),
                    },
                    _ => Tok::Ident(word),
                }
            }
            b'(' | b'[' | b'{' => {
                self.at += 1;
                Tok::Open(c)
            }
            b')' | b']' | b'}' => {
                self.at += 1;
                Tok::Close(c)
            }
            _ => {
                self.at += 1;
                Tok::Punct(c)
            }
        };
        Ok(Some((tok, start)))
    }

    /// Past a bracketed group whose opener was just read at `open`: the range inside it.
    fn group(&mut self, open: usize) -> Result<(usize, usize), ImpliedRefused<'t>> {
        let inside = self.at;
        let mut depth = 1u32;
        loop {
            match self.next()? {
                None => return Err(ImpliedRefused::Unclosed { what: "bracket", line: self.line(open) }),
                Some((Tok::Open(_), _)) => depth += 1,
                Some((Tok::Close(_), at)) => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok((inside, at));
                    }
                }
                Some(_) => {}
            }
        }
    }
}

/// A unit's cfg: (name, value) atoms.
pub type Cfg<'c> = &'c [(&'c str, Option<&'c str>)];

fn atom<'t>(name: &'t str, value: Option<&str>, cfg: Cfg<'_>, line: u32) -> Result<bool, ImpliedRefused<'t>> {
    if name == "debug_assertions" {
        return Err(ImpliedRefused::ProfileAtom { name, line });
    }
    Ok(cfg.iter().any(|(n, v)| *n == name && *v == value))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Combine {
    All,
    Any,
    Not,
}

/// One cfg predicate read from `lx` and evaluated under `cfg` (an explicit stack, no
/// recursion); `lx` is left just past it.
fn predicate<'t>(lx: &mut Lexer<'t>, cfg: Cfg<'_>) -> Result<bool, ImpliedRefused<'t>> {
    let mut stack = [(Combine::All, false, 0u32); MAX_DEPTH];
    let mut depth = 0usize;
    loop {
        let line = lx.line(lx.at);
        let Some((tok, at)) = lx.next()? else { return Err(ImpliedRefused::Predicate { why: "a cfg predicate cut short", line }) };
        let line = lx.line(at);
        let value = match tok {
            Tok::Ident(name) => {
                let save = lx.at;
                match lx.next()? {
                    Some((Tok::Open(b'('), _)) => {
                        let kind = match name {
                            "all" => Combine::All,
                            "any" => Combine::Any,
                            "not" => Combine::Not,
                            _ => return Err(ImpliedRefused::UnknownPredicate { name, line }),
                        };
                        if depth == MAX_DEPTH {
                            return Err(ImpliedRefused::TooDeep { line });
                        }
                        stack[depth] = (kind, kind == Combine::All, 0);
                        depth += 1;
                        continue;
                    }
                    Some((Tok::Punct(b'='), _)) => match lx.next()? {
                        Some((Tok::Str(value), _)) => atom(name, Some(value), cfg, line)?,
                        _ => return Err(ImpliedRefused::ValueForm { line }),
                    },
                    _ => {
                        lx.at = save;
                        atom(name, None, cfg, line)?
                    }
                }
            }
            Tok::Punct(b',') if depth > 0 => continue,
            Tok::Close(b')') if depth > 0 => {
                depth -= 1;
                let (kind, acc, n) = stack[depth];
                if kind == Combine::Not && n != 1 {
                    return Err(ImpliedRefused::Predicate { why: "`not(..)` of other than one predicate", line });
                }
                acc
            }
            _ => return Err(ImpliedRefused::Predicate { why: "a token no cfg predicate holds", line }),
        };
        if depth == 0 {
            return Ok(value);
        }
        let top = &mut stack[depth - 1];
        top.2 += 1;
        match top.0 {
            Combine::All => top.1 &= value,
            Combine::Any => top.1 |= value,
            Combine::Not => top.1 = !value,
        }
    }
}

/// The attributes inside a `cfg_attr(pred, a, b, ..)` whose `(` was just read: `pred`'s
/// start, and the ranges of `a`, `b`, .. pushed onto `out`; `lx` is left past the `)`.
fn cfg_attr_parts<'t>(lx: &mut Lexer<'t>, out: &mut Ranges) -> Result<usize, ImpliedRefused<'t>> {
    let pred = lx.at;
    let mut depth = 0u32;
    // Past the predicate, to its depth-0 comma.
    loop {
        match lx.next()? {
            None => return Err(ImpliedRefused::Unclosed { what: "`cfg_attr`", line: lx.line(pred) }),
            Some((Tok::Open(_), _)) => depth += 1,
            Some((Tok::Close(_), at)) if depth == 0 => {
                return Err(ImpliedRefused::Predicate { why: "a `cfg_attr` with no attribute", line: lx.line(at) });
            }
            Some((Tok::Close(_), _)) => depth -= 1,
            Some((Tok::Punct(b','), _)) if depth == 0 => break,
            Some(_) => {}
        }
    }
    let mut start = lx.at;
    loop {
        match lx.next()? {
            None => return Err(ImpliedRefused::Unclosed { what: "`cfg_attr`", line: lx.line(pred) }),
            Some((Tok::Open(_), _)) => depth += 1,
            Some((Tok::Close(_), at)) if depth == 0 => {
                out.push((start, at), lx.line(at))?;
                return Ok(pred);
            }
            Some((Tok::Close(_), _)) => depth -= 1,
            Some((Tok::Punct(b','), at)) if depth == 0 => {
                out.push((start, at), lx.line(at))?;
                start = lx.at;
            }
            Some(_) => {}
        }
    }
}

/// A bounded work list of attribute ranges.
struct Ranges {
    items: [(usize, usize); MAX_DEPTH],
    len: usize,
}

impl Ranges {
    fn new() -> Ranges {
        Ranges { items: [(0, 0); MAX_DEPTH], len: 0 }
    }

    fn push<'t>(&mut self, range: (usize, usize), line: u32) -> Result<(), ImpliedRefused<'t>> {
        if self.len == MAX_DEPTH {
            return Err(ImpliedRefused::TooDeep { line });
        }
        self.items[self.len] = range;
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<(usize, usize)> {
        self.len = self.len.checked_sub(1)?;
        Some(self.items[self.len])
    }
}

/// The first word of the attribute at `range` (`None` for an empty one or a path).
fn word_of<'t>(text: &'t str, range: (usize, usize)) -> Result<Option<(&'t str, Lexer<'t>)>, ImpliedRefused<'t>> {
    let mut lx = Lexer { text, at: range.0, end: range.1 };
    let Some((Tok::Ident(word), _)) = lx.next()? else { return Ok(None) };
    let mut after = lx;
    if let Some((Tok::Punct(b':'), _)) = after.next()? {
        return Ok(None);
    }
    Ok(Some((word, lx)))
}

/// Runs the attribute at `range` with every `cfg_attr` in it expanded under `cfg` (a work
/// list, no recursion). `relevant` says which attribute words can change the caller's answer:
/// a `cfg_attr` holding none of them is passed without its predicate evaluated. `each` sees
/// each resulting relevant attribute's word and a lexer just past it.
fn expand<'t>(
    text: &'t str,
    range: (usize, usize),
    cfg: Cfg<'_>,
    relevant: fn(&str) -> bool,
    mut each: impl FnMut(&'t str, Lexer<'t>) -> Result<(), ImpliedRefused<'t>>,
) -> Result<(), ImpliedRefused<'t>> {
    let mut work = Ranges::new();
    work.push(range, line_of(text, range.0))?;
    while let Some(range) = work.pop() {
        let Some((word, mut lx)) = word_of(text, range)? else { continue };
        if word != "cfg_attr" {
            if relevant(word) {
                each(word, lx)?;
            }
            continue;
        }
        let Some((Tok::Open(b'('), _)) = lx.next()? else { continue };
        let mut parts = Ranges::new();
        let pred = cfg_attr_parts(&mut lx, &mut parts)?;
        let mut any = false;
        for part in &parts.items[..parts.len] {
            any |= word_of(text, *part)?.is_some_and(|(w, _)| w == "cfg_attr" || relevant(w));
        }
        if !any {
            continue;
        }
        let mut p = Lexer { text, at: pred, end: range.1 };
        if predicate(&mut p, cfg)? {
            for part in &parts.items[..parts.len] {
                work.push(*part, line_of(text, part.0))?;
            }
        }
    }
    Ok(())
}

/// The sysroot facts of a crate root whose text is `text`, under the unit's `cfg` (the target's
/// atoms and `feature = ".."` per feature on).
pub fn root_facts<'t>(text: &'t str, cfg: Cfg<'_>) -> Result<RootFacts, ImpliedRefused<'t>> {
    let mut at = 0;
    if text.starts_with('\u{feff}') {
        at = 3;
    }
    let mut lx = Lexer { text, at, end: text.len() };
    // A shebang: `#!` not followed (past whitespace and comments) by `[`.
    if text[at..].starts_with("#!") {
        let mut probe = Lexer { text, at: at + 2, end: text.len() };
        probe.trivia()?;
        if probe.byte(probe.at) != Some(b'[') {
            while lx.byte(lx.at).is_some_and(|c| c != b'\n') {
                lx.at += 1;
            }
        }
    }
    let (mut no_std, mut no_core) = (false, false);
    // The inner attributes, before the first item.
    loop {
        let save = lx.at;
        let inner = matches!(lx.next()?, Some((Tok::Punct(b'#'), _))) && matches!(lx.next()?, Some((Tok::Punct(b'!'), _)));
        let open = if inner { lx.next()? } else { None };
        let Some((Tok::Open(b'['), open)) = open else {
            lx.at = save;
            break;
        };
        let range = lx.group(open)?;
        expand(text, range, cfg, |w| matches!(w, "no_std" | "no_core"), |word, mut rest| {
            if rest.next()?.is_some() {
                return Err(ImpliedRefused::Predicate { why: "a `no_std` or `no_core` with arguments", line: rest.line(rest.at) });
            }
            match word {
                "no_std" => no_std = true,
                _ => no_core = true,
            }
            Ok(())
        })?;
    }
    let mut facts = RootFacts {
        sysroot: if no_core {
            ImplicitSysroot::None
        } else if no_std {
            ImplicitSysroot::Core
        } else {
            ImplicitSysroot::Std
        },
        extern_core: false,
        extern_alloc: false,
        extern_std: false,
    };
    // The root module's items: `extern crate` under its outer `#[cfg]`s.
    let mut depth = 0u32;
    let mut live = true;
    while let Some((tok, at)) = lx.next()? {
        match tok {
            Tok::Punct(b'#') if depth == 0 => {
                let save = lx.at;
                match lx.next()? {
                    Some((Tok::Open(b'['), open)) => {
                        let range = lx.group(open)?;
                        expand(text, range, cfg, |w| w == "cfg", |_, mut rest| {
                            let line = rest.line(rest.at);
                            let Some((Tok::Open(b'('), _)) = rest.next()? else {
                                return Err(ImpliedRefused::Predicate { why: "a `cfg` with no predicate", line });
                            };
                            live &= predicate(&mut rest, cfg)?;
                            Ok(())
                        })?;
                    }
                    _ => lx.at = save,
                }
            }
            Tok::Open(_) => depth += 1,
            Tok::Close(c) => {
                depth = depth.checked_sub(1).ok_or(ImpliedRefused::Unclosed { what: "bracket closed before it opened", line: line_of(text, at) })?;
                if depth == 0 && c == b'}' {
                    live = true;
                }
            }
            Tok::Punct(b';') if depth == 0 => live = true,
            Tok::Ident("extern") if depth == 0 => {
                let save = lx.at;
                match (lx.next()?, lx.next()?) {
                    (Some((Tok::Ident("crate"), _)), Some((Tok::Ident(name), _))) => {
                        if live {
                            match name {
                                "core" => facts.extern_core = true,
                                "alloc" => facts.extern_alloc = true,
                                "std" => facts.extern_std = true,
                                _ => {}
                            }
                        }
                    }
                    _ => lx.at = save,
                }
            }
            _ => {}
        }
    }
    Ok(facts)
}

/// The toolchain's sysroot units of one target and side, by index in its unit list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SysrootUnits {
    pub core: u32,
    pub alloc: u32,
    pub std: u32,
}

/// The edges `facts` add to a unit (extern name, unit), sorted by name: `core` unless
/// `#![no_core]` (or by `extern crate core`), `std` when its prelude is std's or it writes
/// `extern crate std`, `alloc` when it writes `extern crate alloc`. A name the unit already
/// depends on explicitly is left out (an explicit dependency wins, as `--extern` does).
pub fn implied_edges(facts: &RootFacts, units: SysrootUnits, explicit: impl Fn(&str) -> bool) -> [Option<(&'static str, u32)>; 3] {
    let core = facts.sysroot != ImplicitSysroot::None || facts.extern_core;
    let std = facts.sysroot == ImplicitSysroot::Std || facts.extern_std;
    let edge = |on: bool, name: &'static str, unit: u32| (on && !explicit(name)).then_some((name, unit));
    [edge(facts.extern_alloc, "alloc", units.alloc), edge(core, "core", units.core), edge(std, "std", units.std)]
}

/// A unit on or above a dependency cycle among `deps` (each unit's dependencies by index),
/// or `None` when they form a DAG: passes over the units, each settling those whose
/// dependencies are all settled (no recursion; at most `deps.len()` passes).
pub fn first_in_cycle(deps: &[&[u32]], alloc: Alloc<'_>) -> Result<Option<usize>, AllocFailure> {
    let mut settled: AVec<'_, bool> = AVec::new_in(alloc);
    settled.try_reserve(deps.len())?;
    settled.extend(deps.iter().map(|_| false));
    let mut left = deps.len();
    while left > 0 {
        let before = left;
        for u in 0..deps.len() {
            if !settled[u] && deps[u].iter().all(|d| settled.get(*d as usize).copied().unwrap_or(false)) {
                settled[u] = true;
                left -= 1;
            }
        }
        if left == before {
            return Ok(settled.iter().position(|s| !s));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{self, CrateIn, RecordRefused, Role, TargetIn, UnitIn, UnitKind};
    use crate::ToolchainView;

    const MAC: &[(&str, Option<&str>)] = &[("target_os", Some("macos")), ("unix", None)];

    fn of(text: &str) -> RootFacts {
        root_facts(text, MAC).unwrap_or_else(|e| panic!("{e}"))
    }

    /// The Reference, "The `no_std` attribute" and "The `no_core` attribute": the root's inner
    /// attribute decides; comments, doc comments and other attributes around it do not.
    /// CONTROL: a root with neither is `std`. Prints `implied: std core none core core`.
    #[test]
    fn the_cutter_reads_no_std_and_no_core() {
        let plain = of("//! A crate.\npub struct Bits<S = Vec<u64>>(S);");
        let no_std = of("/* a ] in a comment */\n#![doc = \"a ] in a string\"]\n#![no_std]\n#![allow(dead_code)]\npub fn f() {}");
        let no_core = of("#![feature(no_core)]\n#![no_core]\n");
        let shebang = of("#!/usr/bin/env run\n#![no_std]\n");
        let raw = of("#![doc = r#\"]\"#]\n#![no_std]");
        assert_eq!(plain.sysroot, ImplicitSysroot::Std, "no attribute: std");
        assert_eq!(no_std.sysroot, ImplicitSysroot::Core);
        assert_eq!(no_core.sysroot, ImplicitSysroot::None);
        assert_eq!(shebang.sysroot, ImplicitSysroot::Core, "a shebang is passed");
        assert_eq!(raw.sysroot, ImplicitSysroot::Core);
        // An inner attribute after the first item is not the root's: Rust refuses it there,
        // and it is not read as one.
        assert_eq!(of("pub fn f() {}\n#![no_std]").sysroot, ImplicitSysroot::Std);
        std::println!("implied: {} {} {} {} {}", plain.sysroot.name(), no_std.sysroot.name(), no_core.sysroot.name(), shebang.sysroot.name(), raw.sysroot.name());
    }

    /// `cfg_attr(not(test), no_std)` (bstd's form, crates/blaze-std/src/lib.rs:101): `test` is
    /// not among a library unit's atoms, so it is `no_std`; libc's pair (vendor/libc
    /// lib.rs:219-220) under `rustc-dep-of-std` is `no_core`, without it `no_std`. A
    /// `cfg_attr` holding no sysroot attribute is not evaluated, so `debug_assertions` there
    /// is no refusal; where it decides the sysroot, it is refused by name. Prints
    /// `implied cfg_attr: bstd core, libc std-dep none, libc core, feature-gated std`.
    #[test]
    fn the_cutter_evaluates_cfg_attr_under_the_units_cfg() {
        assert_eq!(of("#![cfg_attr(not(test), no_std)]").sysroot, ImplicitSysroot::Core);
        assert_eq!(of("#![cfg_attr(test, no_std)]").sysroot, ImplicitSysroot::Std, "CONTROL: `test` is false");
        let libc = "#![cfg_attr(feature = \"rustc-dep-of-std\", feature(link_cfg, no_core))]\n#![cfg_attr(not(feature = \"rustc-dep-of-std\"), no_std)]\n#![cfg_attr(feature = \"rustc-dep-of-std\", no_core)]";
        let dep_of_std: &[(&str, Option<&str>)] = &[("feature", Some("rustc-dep-of-std")), ("unix", None)];
        let in_std = root_facts(libc, dep_of_std).expect("libc reads");
        assert_eq!(in_std.sysroot, ImplicitSysroot::None);
        assert_eq!(of(libc).sysroot, ImplicitSysroot::Core);
        let gated = "#![cfg_attr(not(any(feature = \"std\", all(unix, not(unix)))), no_std)]";
        assert_eq!(of(gated).sysroot, ImplicitSysroot::Core);
        assert_eq!(root_facts(gated, &[("feature", Some("std"))]).expect("reads").sysroot, ImplicitSysroot::Std);
        assert_eq!(of("#![cfg_attr(unix, cfg_attr(target_os = \"macos\", no_std))]").sysroot, ImplicitSysroot::Core, "nested");
        assert_eq!(of("#![cfg_attr(debug_assertions, allow(dead_code))]").sysroot, ImplicitSysroot::Std);
        assert!(matches!(root_facts("#![cfg_attr(debug_assertions, no_std)]", MAC), Err(ImpliedRefused::ProfileAtom { name: "debug_assertions", line: 1 })));
        assert!(matches!(root_facts("#![cfg_attr(version(\"1.0\"), no_std)]", MAC), Err(ImpliedRefused::UnknownPredicate { name: "version", .. })));
        std::println!("implied cfg_attr: bstd core, libc std-dep none, libc core, feature-gated std");
    }

    /// The Reference, "Extern crate declarations": `extern crate std;` in the root names
    /// `std` from the sysroot; under a false `#[cfg]` it is not there. blaze-own's form
    /// (lib.rs:98-100) under no feature names nothing; with `std` it names `std`.
    /// `extern crate self as std` and `extern "C"` are not sysroot crates; one in a nested
    /// module is not the root's. Prints `implied externs: own std=false, own+std std=true`.
    #[test]
    fn the_cutter_reads_the_roots_extern_crates() {
        let own = "#![no_std]\n#[cfg(any(test, feature = \"std\"))]\nextern crate std;\nextern crate alloc;\nextern \"C\" { fn f(); }\nmod m { extern crate core; }\nextern crate self as me;";
        let plain = of(own);
        assert_eq!(plain.sysroot, ImplicitSysroot::Core);
        assert!(!plain.extern_std && plain.extern_alloc && !plain.extern_core);
        let with_std = root_facts(own, &[("feature", Some("std"))]).expect("reads");
        assert!(with_std.extern_std, "CONTROL: under `std` it is live");
        let stacked = of("#![no_std]\n#[cfg(test)]\n#[macro_use]\nextern crate std;\nfn g<'a>(x: &'a u8) -> char { let _ = *x; '}' }\nextern crate core;");
        assert!(!stacked.extern_std && stacked.extern_core, "the `#[cfg(test)]` holds to its item only; a `'}}'` closes nothing");
        std::println!("implied externs: own std={}, own+std std={}", plain.extern_std, with_std.extern_std);
    }

    /// The edges: a `std` row names core and std, a `no_std` one core (and alloc when it
    /// writes `extern crate alloc`), a `no_core` one nothing; an explicit `core` (as
    /// `rustc-std-workspace-core` is named) wins over the implied one.
    #[test]
    fn edges_follow_the_facts_and_explicit_names_win() {
        let units = SysrootUnits { core: 0, alloc: 1, std: 2 };
        let std_row = RootFacts { sysroot: ImplicitSysroot::Std, extern_core: false, extern_alloc: false, extern_std: false };
        assert_eq!(implied_edges(&std_row, units, |_| false), [None, Some(("core", 0)), Some(("std", 2))]);
        let core_row = RootFacts { sysroot: ImplicitSysroot::Core, extern_alloc: true, ..std_row };
        assert_eq!(implied_edges(&core_row, units, |_| false), [Some(("alloc", 1)), Some(("core", 0)), None]);
        assert_eq!(implied_edges(&core_row, units, |n| n == "core"), [Some(("alloc", 1)), None, None]);
        let none_row = RootFacts { sysroot: ImplicitSysroot::None, ..std_row };
        assert_eq!(implied_edges(&none_row, units, |_| false), [None, None, None]);
        // A std edge from a crate std is built from is a cycle: found, by unit.
        let deps: [&[u32]; 3] = [&[], &[0, 2], &[1]];
        let found = first_in_cycle(&deps, blaze_own::testing::heap().alloc()).expect("memory");
        assert!(matches!(found, Some(1 | 2)), "{found:?}");
        let dag: [&[u32]; 3] = [&[], &[0], &[1, 0]];
        assert_eq!(first_in_cycle(&dag, blaze_own::testing::heap().alloc()).expect("memory"), None);
    }

    fn manifest(sysroot: ImplicitSysroot) -> std::vec::Vec<u8> {
        let crates = [CrateIn { name: "core", version: "0.0.0", edition: "2024", lib: "core", root: "src/lib.rs", dir: "core", role: Role::Core, pack_kind: 3, pack_len: 0, pack: [0; 32] }];
        let units = [UnitIn { krate: 0, kind: UnitKind::Lib, for_host: false, root: "src/lib.rs", features: &[], deps: &[], sysroot }];
        let targets = [TargetIn { triple: "aarch64-apple-darwin", units: &units, cfg: &[] }];
        record::write(&crates, &targets, blaze_own::testing::heap().alloc()).expect("a record").to_vec()
    }

    /// The unit row carries its sysroot; a manifest of the form before it (`BLZTCH02`, cut
    /// before the sysroot edge) is a typed refusal naming that form, never read as if its
    /// zero byte meant something. CONTROL: the same bytes under this form open. Prints
    /// `implied record: core; old form refused: BLZTCH02`.
    #[test]
    fn the_row_holds_the_sysroot_and_an_old_manifest_is_refused() {
        let bytes = manifest(ImplicitSysroot::Core);
        let view = ToolchainView::open(&bytes, ToolchainView::id_of_bytes(&bytes)).expect("this form opens");
        let row = view.units("aarch64-apple-darwin").expect("the target").next().expect("a unit");
        assert_eq!(row.sysroot, ImplicitSysroot::Core);
        let mut old = bytes.clone();
        old[..8].copy_from_slice(b"BLZTCH02");
        let body = old.len() - 32;
        let digest = blaze_fetch::Blake3::of(&old[..body]).0;
        old[body..].copy_from_slice(&digest);
        let refused = ToolchainView::open(&old, ToolchainView::id_of_bytes(&old)).err();
        assert_eq!(refused, Some(RecordRefused::OldForm { found: *b"BLZTCH02" }));
        std::println!("implied record: {}; old form refused: BLZTCH02", row.sysroot.name());
    }
}
