//! What a crate root, or a module file's own top, says of the code under it: the inner
//! attributes `#![no_std]`, `#![blaze::sans_io]` (a sans-IO core, D170 C5), `#![blaze::driver]`
//! (the thin driver over a core, which may do IO and holds only handles and straight-line calls)
//! and `#![deny(blaze::..)]` / `#![forbid(blaze::..)]` of the lints the rules here are
//! ([`Rule::lint`]). Read from the file's text with the tree's lexer, at the top only: an inner
//! attribute is only ever there.
//!
//! A driver ([`RootMarks::read`] of the crate root, joined with the file's own) turns these into
//! the file's [`Scope`] ([`RootMarks::scope`]), the rules it runs ([`RootMarks::rules`]) and their
//! levels ([`RootMarks::levels`]): a crate that denies `blaze::std_use` gets `ambient` as an
//! error in that crate only. Nothing here turns a rule on for a crate that did not ask.

use core::iter::Peekable;

use blaze_lex::{Cpu, FrontmatterAllowed, TokenKind, Tokens, tokenize};

use crate::{Level, Levels, Rule, RuleSet, Scope};

/// The marks of one file's top: what its inner attributes say.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct RootMarks {
    /// `#![no_std]`.
    pub no_std: bool,
    /// `#![blaze::sans_io]`: the code under it is a sans-IO core.
    pub sans_io: bool,
    /// `#![blaze::driver]`: the code under it is a core's driver, which does the IO.
    pub driver: bool,
    /// `#![blaze::trust(unsafe, reason = "..")]`: the crate root explains why it uses `unsafe`.
    pub trust_unsafe: bool,
    /// `#![blaze::uniq]`: crate uses `&uniq` / `ref uniq`, turning on `blaze::mut_ref` deprecation.
    pub uniq: bool,
    /// The rules whose lint the file denies or forbids, as [`Rule::bit`]s.
    denied: u64,
}

impl RootMarks {
    /// No mark: a file whose top says none of these.
    pub const NONE: RootMarks = RootMarks {
        no_std: false,
        sans_io: false,
        driver: false,
        trust_unsafe: false,
        uniq: false,
        denied: 0,
    };

    /// The marks `text`'s leading inner attributes give. Comments and doc comments between them
    /// are passed over; the first token that does not start an inner attribute ends the read.
    /// An attribute this does not know (`#![feature(..)]`, `#![cfg_attr(..)]`) marks nothing.
    pub fn read(text: &str) -> RootMarks {
        let mut marks = RootMarks::NONE;
        let mut sig = Sig { text, pos: 0, tokens: tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) }.peekable();
        // Bounded: each turn reads one inner attribute of the text, or ends the loop.
        loop {
            let opens = next_is(&mut sig, TokenKind::Pound)
                && next_is(&mut sig, TokenKind::Bang)
                && next_is(&mut sig, TokenKind::OpenBracket);
            if !opens {
                return marks;
            }
            marks.attribute(&mut sig);
        }
    }

    /// The marks of a file under a crate root: the root's, with what the file's own top adds
    /// (a module may be a core in a crate that is not). `no_std` is the root's alone.
    pub const fn join(self, file: RootMarks) -> RootMarks {
        RootMarks {
            no_std: self.no_std,
            sans_io: self.sans_io || file.sans_io,
            driver: self.driver || file.driver,
            trust_unsafe: self.trust_unsafe || file.trust_unsafe,
            uniq: self.uniq || file.uniq,
            denied: self.denied | file.denied,
        }
    }

    /// Everything the marks say as one integer, for a record's key: two marks with one key are
    /// equal. The rules they add ([`RootMarks::rules`]) are a function of it, so a key that has
    /// it covers them.
    pub const fn key(&self) -> u64 {
        const _: () = assert!(Rule::COUNT < 59, "RootMarks::key reserves bits 59..64 for flags");
        self.denied
            | (if self.uniq { 1u64 << 59 } else { 0 })
            | (self.no_std as u64) << 60
            | (self.sans_io as u64) << 61
            | (self.driver as u64) << 62
            | (self.trust_unsafe as u64) << 63
    }

    /// Whether the file denies or forbids `rule`'s lint.
    pub const fn denies(&self, rule: Rule) -> bool {
        self.denied & rule.bit() != 0
    }

    /// `scope` as the marks make it: in a core where the file is one, and with `trust_unsafe`
    /// where the crate root carries a valid `#![blaze::trust(unsafe, reason = "..")]`.
    pub const fn scope(&self, scope: Scope) -> Scope {
        let scope = if self.sans_io { scope.in_core() } else { scope };
        if self.trust_unsafe { scope.with_trust_unsafe() } else { scope }
    }

    /// `base` and the rules the marks ask for: each rule whose lint is denied, and
    /// `sans-io-core` in a core.
    pub const fn rules(&self, base: RuleSet) -> RuleSet {
        let mut rules = base;
        let mut at = 0;
        // Bounded: one turn per rule.
        while at < Rule::ALL.len() {
            if self.denies(Rule::ALL[at]) {
                rules = rules.with(Rule::ALL[at]);
            }
            at += 1;
        }
        if self.sans_io {
            rules = rules.with(Rule::SansIoCore);
        }
        if self.uniq {
            rules = rules.with(Rule::MutRef);
        }
        rules
    }

    /// `base` with each denied rule an error: the crate's own words, for its own findings.
    pub const fn levels(&self, base: Levels) -> Levels {
        let mut levels = base.0;
        let mut at = 0;
        // Bounded: one turn per rule.
        while at < Rule::ALL.len() {
            if self.denies(Rule::ALL[at]) {
                levels[at] = Level::Error;
            }
            at += 1;
        }
        Levels(levels)
    }

    /// One inner attribute, its `#![` read: what it marks, then the rest of it up to its `]`.
    fn attribute(&mut self, sig: &mut Peekable<Sig<'_>>) {
        let head = match sig.next() {
            Some((TokenKind::Ident, head)) => head,
            Some((kind, _)) => {
                close(sig, depth_of(kind, 1));
                return;
            }
            None => return,
        };
        match head {
            "no_std" if peek_is(sig, TokenKind::CloseBracket) => self.no_std = true,
            "blaze" => match blaze_path(sig) {
                Some("sans_io") if peek_is(sig, TokenKind::CloseBracket) => self.sans_io = true,
                Some("driver") if peek_is(sig, TokenKind::CloseBracket) => self.driver = true,
                Some("uniq") if peek_is(sig, TokenKind::CloseBracket) => self.uniq = true,
                Some("trust") if peek_is(sig, TokenKind::OpenParen) => {
                    if trust_unsafe(sig) {
                        self.trust_unsafe = true;
                    }
                }
                _ => {}
            },
            "deny" | "forbid" if peek_is(sig, TokenKind::OpenParen) => self.denied |= denied_list(sig),
            _ => {}
        }
        close(sig, 1);
    }
}

/// The IO module names (engine.rs `IO_MODULES`: fs, net, process, io, os, thread, env) a file makes its own: declared there
/// (`mod io;`, `mod io { .. }`) or imported from anything but `std`, `core` or `alloc`
/// (`use crate::io;`, `use super::{fs, x};`). A bare path head (`io::Plan`) resolves only to a
/// name in scope, so in such a file it is the crate's module, never std's, and `sans-io-core`
/// does not report it; in a file that imports it from std (`use std::io;`), or names none, it is
/// std's. Read from the whole file's tokens with the tree's lexer; a `use` inside a function
/// makes the name the file's own too (the rule then says less, never more). A bit per name, in
/// that list's order; in [`Scope::own_heads`] and so in the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct OwnHeads(pub u8);

impl OwnHeads {
    pub const NONE: OwnHeads = OwnHeads(0);

    /// The IO module names `text` declares or imports as its own.
    pub fn read(text: &str) -> OwnHeads {
        let mut bits = 0u8;
        let mut sig = Sig { text, pos: 0, tokens: tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) }.peekable();
        let bit = |name: &str| crate::engine::IO_MODULES.iter().position(|m| *m == name).map_or(0, |at| 1u8 << at);
        // Bounded: each turn takes one token of the text (a `use` reads to its `;`).
        while let Some((kind, word)) = sig.next() {
            if kind != TokenKind::Ident {
                continue;
            }
            match word {
                "mod" => {
                    if let Some(&(TokenKind::Ident, name)) = sig.peek() {
                        bits |= bit(name);
                    }
                }
                "use" => bits |= own_imports(&mut sig, &bit),
                _ => {}
            }
        }
        OwnHeads(bits)
    }
}

/// The rest of a `use` up to its `;`: the IO module names it imports as leaves (a name before
/// `,`, `}`, `;` or `as`), when its root is not `std`, `core` or `alloc`.
fn own_imports(sig: &mut Peekable<Sig<'_>>, bit: &impl Fn(&str) -> u8) -> u8 {
    let mut bits = 0u8;
    let mut root: Option<bool> = None;
    let mut last: Option<&str> = None;
    // Bounded: each turn takes one token of the text, to the `use`'s `;` at most.
    while let Some((kind, word)) = sig.next() {
        let leaf_end = matches!(kind, TokenKind::Comma | TokenKind::CloseBrace | TokenKind::Semi)
            || (kind == TokenKind::Ident && word == "as");
        if leaf_end && let Some(name) = last.take() {
            bits |= bit(name);
        }
        match kind {
            TokenKind::Semi => break,
            TokenKind::Ident if word != "as" => {
                if root.is_none() {
                    root = Some(matches!(word, "std" | "core" | "alloc"));
                }
                last = Some(word);
            }
            _ => last = None,
        }
    }
    if root == Some(true) { 0 } else { bits }
}

/// The significant tokens of a text: whitespace and comments passed over, each with its text.
struct Sig<'t> {
    text: &'t str,
    pos: usize,
    tokens: Tokens<'t>,
}

impl<'t> Iterator for Sig<'t> {
    type Item = (TokenKind, &'t str);

    fn next(&mut self) -> Option<(TokenKind, &'t str)> {
        // Bounded: each turn takes one token of the text.
        loop {
            let token = self.tokens.next()?;
            let start = self.pos;
            self.pos = start + token.len as usize;
            match token.kind {
                TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. } => continue,
                TokenKind::Eof => return None,
                kind => return Some((kind, self.text.get(start..self.pos).unwrap_or(""))),
            }
        }
    }
}

fn next_is(sig: &mut Peekable<Sig<'_>>, kind: TokenKind) -> bool {
    matches!(sig.next(), Some((k, _)) if k == kind)
}

fn peek_is(sig: &mut Peekable<Sig<'_>>, kind: TokenKind) -> bool {
    matches!(sig.peek(), Some((k, _)) if *k == kind)
}

/// `depth` after `kind`: one deeper for an opening bracket, one shallower for a closing one.
fn depth_of(kind: TokenKind, depth: usize) -> usize {
    match kind {
        TokenKind::OpenBracket | TokenKind::OpenParen | TokenKind::OpenBrace => depth + 1,
        TokenKind::CloseBracket | TokenKind::CloseParen | TokenKind::CloseBrace => depth.saturating_sub(1),
        _ => depth,
    }
}

/// Reads up to and with the bracket that takes `depth` to zero (the text's end at most).
fn close(sig: &mut Peekable<Sig<'_>>, depth: usize) {
    let mut depth = depth;
    // Bounded: each turn takes one token of the text.
    while depth > 0 {
        let Some((kind, _)) = sig.next() else { return };
        depth = depth_of(kind, depth);
    }
}

/// `::name` after a `blaze`: the name, when the path is two segments. Takes only the tokens of
/// the path: a bracket after `blaze` is left for the caller's depth.
fn blaze_path<'t>(sig: &mut Peekable<Sig<'t>>) -> Option<&'t str> {
    for _ in 0..2 {
        if !peek_is(sig, TokenKind::Colon) {
            return None;
        }
        sig.next();
    }
    if !peek_is(sig, TokenKind::Ident) {
        return None;
    }
    sig.next().map(|(_, name)| name)
}

/// `(blaze::a, blaze::b, other, reason = "..")` after a `deny` or `forbid`: the rules of the
/// `blaze::` lints named, as bits; the list read up to and with its `)`.
fn denied_list(sig: &mut Peekable<Sig<'_>>) -> u64 {
    let mut bits = 0;
    let mut depth = 0usize;
    // Bounded: each turn takes one token of the text.
    while let Some((kind, word)) = sig.next() {
        depth = depth_of(kind, depth);
        if depth == 0 {
            return bits;
        }
        if depth == 1
            && kind == TokenKind::Ident
            && word == "blaze"
            && let Some(name) = blaze_path(sig)
            && let Some(rule) = Rule::from_lint(name)
        {
            bits |= rule.bit();
        }
    }
    bits
}

/// `(unsafe, reason = "..")` after `blaze::trust`: consumes from `(` through its matching `)`,
/// returning `true` iff `unsafe` and a non-empty `reason = "<text>"` are both present at depth 1
/// and well-formed.
fn trust_unsafe(sig: &mut Peekable<Sig<'_>>) -> bool {
    let Some((TokenKind::OpenParen, _)) = sig.next() else { return false };
    let mut depth = 1usize;
    let mut has_unsafe = false;
    let mut has_reason = false;
    let mut bad = false;
    // Bounded: each turn consumes one token of the text until the matching `)`.
    while depth > 0 {
        let Some((kind, word)) = sig.next() else { return false };
        if depth == 1 && kind == TokenKind::CloseParen {
            return !bad && has_unsafe && has_reason;
        }
        if depth == 1 && kind == TokenKind::Ident {
            if word == "unsafe" {
                if matches!(sig.peek(), Some(&(TokenKind::Comma | TokenKind::CloseParen, _))) {
                    has_unsafe = true;
                } else {
                    bad = true;
                }
                continue;
            }
            if word == "reason" {
                if peek_is(sig, TokenKind::Eq) {
                    sig.next();
                    if matches!(sig.peek(), Some(&(TokenKind::Literal { .. }, _))) {
                        if let Some((_, val)) = sig.next() {
                            if crate::engine::says_something(val)
                                && matches!(sig.peek(), Some(&(TokenKind::Comma | TokenKind::CloseParen, _)))
                            {
                                has_reason = true;
                            } else {
                                bad = true;
                            }
                        }
                    } else {
                        bad = true;
                    }
                } else {
                    bad = true;
                }
                continue;
            }
        }
        depth = depth_of(kind, depth);
    }
    false
}
