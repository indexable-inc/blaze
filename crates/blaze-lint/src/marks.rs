//! What a crate root, module, item, or block says of the code under it: the inner attributes
//! `#![no_std]`, `#![blaze::sans_io]` (a sans-IO core, D170 C5), `#![blaze::driver]` (the thin
//! driver over a core, which may do IO and holds only handles and straight-line calls), and
//! `#![allow/expect/warn/deny/forbid(..)]` / `#[allow/expect/warn/deny/forbid(..)]` (including
//! active `cfg_attr(..)`) for the lints the rules here are ([`Rule::lint`] and tracked rustc
//! lints such as `deprecated_safe_2024`, `unsafe_op_in_unsafe_fn`, `rust_2024_compatibility`,
//! and `warnings`). Read from the file's text with the tree's lexer.
//!
//! A driver ([`RootMarks::read_with_cfg`] of the crate root, joined with [`RootMarks::read_at_in`]
//! at a finding's byte offset) turns these into the file's [`Scope`] ([`RootMarks::scope`]), the
//! rules it runs ([`RootMarks::rules`]) and their levels ([`RootMarks::levels`]): a crate that
//! denies `blaze::std_use` gets `ambient` as an error in that crate only. Nothing here turns a
//! rule on for a crate that did not ask.

use core::iter::Peekable;

use blaze_lex::{Cpu, FrontmatterAllowed, TokenKind, Tokens, tokenize};
use blaze_own::{AVec, Alloc};

use crate::{Level, Levels, Rule, RuleSet, Scope};

/// Active `cfg` atoms used to evaluate `cfg_attr(predicate, ..)` on lint and crate attributes:
/// `(name, None)` for bare flags like `unix` / `test`, `(key, Some(value))` for key-value pairs
/// like `target_os = "linux"` / `feature = "strict"`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CfgAtoms<'a>(pub &'a [(&'a str, Option<&'a str>)]);

impl<'a> CfgAtoms<'a> {
    /// No atoms active (`all()` is true, `any()` is false, any named atom is false).
    pub const EMPTY: CfgAtoms<'static> = CfgAtoms(&[]);

    fn has(self, prefix: &str, name: &str, value: Option<&str>) -> bool {
        if prefix.is_empty() && value.is_none() {
            match name {
                "true" => return true,
                "false" => return false,
                _ => {}
            }
        }
        self.0.iter().any(|&(n, v)| {
            v == value && n.bytes().eq(prefix.bytes().chain(name.bytes()))
        })
    }
}

/// Rustc lints tracked here whose default level is `Warn` (`Level::Advice`), so `#![forbid(warnings)]`
/// forbids them unless already explicitly allowed before `forbid(warnings)`.
const RUSTC_WARN_BY_DEFAULT: u64 = Rule::UnsafeOpInUnsafeFn.bit();

/// All rustc lints tracked here (subject to the `warnings` meta-lint).
const RUSTC_LINTS: u64 = Rule::DeprecatedSafe2024.bit() | Rule::UnsafeOpInUnsafeFn.bit();

/// Maximum nesting depth of `cfg(...)` / `cfg_attr(...)` predicates (Rule 18).
const MAX_CFG_DEPTH: usize = 32;

/// Effective level of the Rust `warnings` meta-lint (`#[allow/warn/deny/forbid(warnings)]`),
/// applied after resolving a rustc lint's specific level (affects only lints resolved to `Warn`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
enum WarningsLevel {
    #[default]
    Default,
    Allow,
    Warn,
    Deny,
    Forbid,
}

impl WarningsLevel {
    const fn apply(self, next: WarningsLevel) -> WarningsLevel {
        match (self, next) {
            (WarningsLevel::Forbid, _) | (_, WarningsLevel::Forbid) => WarningsLevel::Forbid,
            (_, WarningsLevel::Default) => self,
            (_, level) => level,
        }
    }

    const fn byte(self) -> u8 {
        match self {
            WarningsLevel::Default => 0,
            WarningsLevel::Allow => 1,
            WarningsLevel::Warn => 2,
            WarningsLevel::Deny => 3,
            WarningsLevel::Forbid => 4,
        }
    }
}

/// Lint bits plus the `warnings` meta-lint parsed from one `allow`/`expect`/`warn`/`deny`/`forbid` list.
#[derive(Clone, Copy, Default)]
struct LintItems {
    rules: u64,
    warnings: bool,
}

/// The marks of one file's top (or lexical scope at a byte offset): what its lint and crate
/// attributes say.
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
    /// The rules whose lint the scope denies or forbids, as [`Rule::bit`]s.
    denied: u64,
    /// The rules whose lint the scope explicitly warns on, as [`Rule::bit`]s
    /// (e.g. `#![warn(deprecated_safe_2024)]`).
    warned: u64,
    /// The rules whose lint the scope explicitly allows/expects, as [`Rule::bit`]s
    /// (e.g. `#[allow(deprecated_safe_2024)]`).
    allowed: u64,
    /// The rules whose lint the scope forbids (cannot be downgraded by later/inner `warn`/`allow`),
    /// as [`Rule::bit`]s.
    forbidden: u64,
    /// Effective level of the `warnings` meta-lint in this scope.
    warnings: WarningsLevel,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Delim {
    File,
    Brace,
    Paren,
    Bracket,
}

#[derive(Clone, Copy)]
struct ScopeFrame {
    marks: RootMarks,
    pending: RootMarks,
    delim: Delim,
    in_item: bool,
    angle_depth: u32,
}

impl ScopeFrame {
    const ROOT: ScopeFrame = ScopeFrame {
        marks: RootMarks::NONE,
        pending: RootMarks::NONE,
        delim: Delim::File,
        in_item: false,
        angle_depth: 0,
    };
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
        warned: 0,
        allowed: 0,
        forbidden: 0,
        warnings: WarningsLevel::Default,
    };

    /// The marks `text`'s leading inner attributes give (with no `cfg` atoms active). Comments
    /// and doc comments between them are passed over; the first token that does not start an
    /// inner attribute ends the read.
    pub fn read(text: &str) -> RootMarks {
        Self::read_with_cfg(text, CfgAtoms::EMPTY)
    }

    /// The marks `text`'s leading inner attributes give under `cfg` (evaluating `#![cfg_attr(..)]`).
    pub fn read_with_cfg(text: &str, cfg: CfgAtoms<'_>) -> RootMarks {
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
            marks.attribute(&mut sig, true, cfg, 0);
        }
    }

    /// The effective lint marks at byte offset `at` inside `text` under `cfg`: file-top `#![..]`
    /// attributes joined with every enclosing module, item, block, statement, and expression's
    /// outer `#[..]` and inner `#![..]` lint attributes (including active `cfg_attr(..)`).
    /// Allocates its scope stack in `alloc` so arbitrary valid nesting depth is preserved without
    /// silently dropping attributes (Rule 3, Rule 11).
    pub fn read_at_in(text: &str, at: usize, cfg: CfgAtoms<'_>, alloc: Alloc<'_>) -> RootMarks {
        Self::read_at_impl(None, text, at, cfg, alloc)
    }

    /// Like [`RootMarks::read_at_in`], for a submodule file whose crate root already established
    /// `crate_root` marks. Seeding the root scope frame with `crate_root` ensures crate-root
    /// `#![forbid(warnings)]` / `#![warn(..)]` interact accurately with submodule-level `#![warn]`
    /// and item-level `#[allow]` attributes (and preserves `no_std` as crate-root-only).
    pub fn read_at_under(crate_root: RootMarks, text: &str, at: usize, cfg: CfgAtoms<'_>, alloc: Alloc<'_>) -> RootMarks {
        Self::read_at_impl(Some(crate_root), text, at, cfg, alloc)
    }

    fn read_at_impl(outer: Option<RootMarks>, text: &str, at: usize, cfg: CfgAtoms<'_>, alloc: Alloc<'_>) -> RootMarks {
        let base = outer.unwrap_or(RootMarks::NONE);
        let mut stack: AVec<'_, ScopeFrame> = AVec::with_capacity_in(16, alloc);
        stack.push(ScopeFrame { marks: base, ..ScopeFrame::ROOT });
        let mut sig = Sig { text, pos: 0, tokens: tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) }.peekable();
        let finish = |mut out: RootMarks| {
            if let Some(root) = outer {
                out.no_std = root.no_std;
            }
            out
        };

        // Phase 1: always read all file-top inner `#![..]` attributes into `stack[0].marks`,
        // even when `at == 0` (e.g. an unplaced finding at the start of the file).
        while peek_is(&mut sig, TokenKind::Pound) {
            let Some((_, _, pound_start)) = sig.next() else { break };
            if peek_is(&mut sig, TokenKind::Bang) {
                sig.next();
                if peek_is(&mut sig, TokenKind::OpenBracket) {
                    sig.next();
                    if let Some(root) = stack.first_mut() {
                        root.marks.attribute(&mut sig, true, cfg, 0);
                    }
                    continue;
                }
            } else if peek_is(&mut sig, TokenKind::OpenBracket) {
                // First outer `#[..]` attribute ends file-top inner attributes; if it starts
                // before `at`, apply it as pending on frame 0 and continue to phase 2.
                sig.next();
                if pound_start >= at {
                    return finish(stack.first().map_or(base, |f| f.marks));
                }
                if let Some(root) = stack.first_mut() {
                    root.pending.attribute(&mut sig, false, cfg, 0);
                }
                break;
            }
            if pound_start >= at {
                return finish(stack.first().map_or(base, |f| f.marks));
            }
            break;
        }

        // Phase 2: walk tokens starting strictly before `at`, maintaining the lexical scope stack.
        // Bounded: each turn consumes at least one token of `text`.
        let mut prev_kind: Option<TokenKind> = None;
        while let Some(&(_, _, start)) = sig.peek() {
            if start >= at {
                break;
            }
            let Some((kind, word, _)) = sig.next() else { break };
            match kind {
                TokenKind::Pound => {
                    if peek_is(&mut sig, TokenKind::Bang) {
                        sig.next();
                        if peek_is(&mut sig, TokenKind::OpenBracket) {
                            sig.next();
                            if let Some(top) = stack.last_mut() {
                                top.marks.attribute(&mut sig, false, cfg, 0);
                            }
                        }
                    } else if peek_is(&mut sig, TokenKind::OpenBracket) {
                        sig.next();
                        if let Some(top) = stack.last_mut() {
                            top.pending.attribute(&mut sig, false, cfg, 0);
                        }
                    }
                }
                TokenKind::OpenBrace | TokenKind::OpenParen | TokenKind::OpenBracket => {
                    let delim = match kind {
                        TokenKind::OpenBrace => Delim::Brace,
                        TokenKind::OpenParen => Delim::Paren,
                        _ => Delim::Bracket,
                    };
                    let child_marks = stack.last().map_or(RootMarks::NONE, |f| f.marks.join(f.pending));
                    stack.push(ScopeFrame {
                        marks: child_marks,
                        pending: RootMarks::NONE,
                        delim,
                        in_item: false,
                        angle_depth: 0,
                    });
                }
                TokenKind::CloseBrace | TokenKind::CloseParen | TokenKind::CloseBracket => {
                    if stack.len() > 1 {
                        let closed = stack.pop().map_or(Delim::Brace, |f| f.delim);
                        // Closing `{ .. }` ends an item, module, or block expression in the parent
                        // scope UNLESS it is inside generic angle brackets (`fn f() -> Foo<{ 1 }>`)
                        // or followed by `else` (continuing `if .. { .. } else { .. }`).
                        let continues_if = closed == Delim::Brace
                            && matches!(sig.peek(), Some(&(TokenKind::Ident, "else", _)));
                        if closed == Delim::Brace
                            && !continues_if
                            && let Some(parent) = stack.last_mut()
                            && parent.angle_depth == 0
                        {
                            parent.pending = RootMarks::NONE;
                            parent.in_item = false;
                        }
                    }
                }
                TokenKind::Lt => {
                    if !peek_is(&mut sig, TokenKind::Eq)
                        && let Some(top) = stack.last_mut()
                        && top.in_item
                    {
                        top.angle_depth = top.angle_depth.saturating_add(1);
                    }
                }
                TokenKind::Gt => {
                    let is_arrow = matches!(prev_kind, Some(TokenKind::Minus | TokenKind::Eq));
                    if !is_arrow
                        && !peek_is(&mut sig, TokenKind::Eq)
                        && let Some(top) = stack.last_mut()
                        && top.in_item
                    {
                        top.angle_depth = top.angle_depth.saturating_sub(1);
                    }
                }
                TokenKind::Semi => {
                    if let Some(top) = stack.last_mut() {
                        top.pending = RootMarks::NONE;
                        top.in_item = false;
                        top.angle_depth = 0;
                    }
                }
                TokenKind::Comma => {
                    if let Some(top) = stack.last_mut()
                        && !top.in_item
                    {
                        top.pending = RootMarks::NONE;
                    }
                }
                TokenKind::Ident => {
                    if matches!(
                        word,
                        "pub" | "fn" | "impl" | "trait" | "struct" | "enum" | "union"
                            | "type" | "const" | "static" | "mod" | "extern" | "unsafe"
                            | "async" | "use" | "where"
                    ) && let Some(top) = stack.last_mut()
                        && top.pending != RootMarks::NONE
                    {
                        top.in_item = true;
                    }
                }
                _ => {}
            }
            prev_kind = Some(kind);
        }

        finish(stack.last().map_or(base, |f| f.marks.join(f.pending)))
    }

    /// The marks of a file or inner scope under an outer scope: the outer's, with what the inner
    /// scope overrides (except forbidden lints and rustc lints forbidden by outer `forbid(warnings)`,
    /// which cannot be downgraded). `no_std` is the crate root's alone.
    pub const fn join(self, inner: RootMarks) -> RootMarks {
        // An outer `forbid(warnings)` forbids every rustc lint that is warned in `self` (either
        // explicitly warned or warn-by-default and not allowed in `self`), preventing any inner
        // specific `#[allow(..)]` from lowering it.
        let outer_forbid = if matches!(self.warnings, WarningsLevel::Forbid) {
            self.forbidden | (self.warned & RUSTC_LINTS) | (RUSTC_WARN_BY_DEFAULT & !self.allowed & !self.denied)
        } else {
            self.forbidden
        };
        let warnings = self.warnings.apply(inner.warnings);
        let inner_set = (inner.denied | inner.warned | inner.allowed) & !outer_forbid;
        let mut forbidden = outer_forbid | inner.forbidden;
        let mut denied = ((self.denied & !inner_set) | (inner.denied & !outer_forbid)) | forbidden;
        let mut warned = (self.warned & !inner_set) | (inner.warned & !outer_forbid);
        let allowed = (self.allowed & !inner_set) | (inner.allowed & !outer_forbid);
        // If `warnings` is `Forbid` (either from outer or newly set by inner), any rustc lint
        // currently at `Warn` (explicitly warned, or warn-by-default and neither allowed nor denied)
        // is now forbidden too so deeper scopes cannot lower it.
        if matches!(warnings, WarningsLevel::Forbid) {
            let newly_forbidden = (warned & RUSTC_LINTS) | (RUSTC_WARN_BY_DEFAULT & !allowed & !denied);
            forbidden |= newly_forbidden;
            denied |= newly_forbidden;
            warned &= !newly_forbidden;
        }
        RootMarks {
            no_std: self.no_std,
            sans_io: self.sans_io || inner.sans_io,
            driver: self.driver || inner.driver,
            trust_unsafe: self.trust_unsafe || inner.trust_unsafe,
            uniq: self.uniq || inner.uniq,
            denied,
            warned,
            allowed,
            forbidden,
            warnings,
        }
    }

    /// Everything the marks say as one integer, for a token lint record's key: two marks with one
    /// key are equal for token lints.
    pub const fn key(&self) -> u64 {
        const _: () = assert!(Rule::COUNT < 59, "RootMarks::key reserves bits 59..64 for flags");
        self.denied
            | (if self.uniq { 1u64 << 59 } else { 0 })
            | (if self.no_std { 1u64 << 60 } else { 0 })
            | (if self.sans_io { 1u64 << 61 } else { 0 })
            | (if self.driver { 1u64 << 62 } else { 0 })
            | (if self.trust_unsafe { 1u64 << 63 } else { 0 })
    }

    /// Canonical digest bytes of all lint-level fields (for cache keys of rendered file sections
    /// that depend on crate-root marks).
    pub const fn level_bytes(&self) -> [u8; 33] {
        let d = self.denied.to_le_bytes();
        let w = self.warned.to_le_bytes();
        let a = self.allowed.to_le_bytes();
        let f = self.forbidden.to_le_bytes();
        [
            d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7],
            w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7],
            a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7],
            f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7],
            self.warnings.byte(),
        ]
    }

    /// Whether the scope denies or forbids `rule`'s lint.
    pub const fn denies(&self, rule: Rule) -> bool {
        self.denied & rule.bit() != 0
    }

    /// Whether the scope explicitly warns on `rule`'s lint.
    pub const fn warns(&self, rule: Rule) -> bool {
        self.warned & rule.bit() != 0
    }

    /// Whether the scope explicitly allows/expects `rule`'s lint.
    pub const fn allows(&self, rule: Rule) -> bool {
        self.allowed & rule.bit() != 0
    }

    /// `scope` as the marks make it: in a core where the file is one, and with `trust_unsafe`
    /// where the crate root carries a valid `#![blaze::trust(unsafe, reason = "..")]`.
    pub const fn scope(&self, scope: Scope) -> Scope {
        let scope = if self.sans_io { scope.in_core() } else { scope };
        if self.trust_unsafe { scope.with_trust_unsafe() } else { scope }
    }

    /// `base` and the rules the marks ask for: each non-typed rule whose lint is denied, and
    /// `sans-io-core` in a core.
    pub fn rules(&self, base: RuleSet) -> RuleSet {
        let mut rules = base;
        for rule in Rule::ALL {
            if self.denies(rule) && !rule.typed() {
                rules = rules.with(rule);
            }
        }
        if self.sans_io {
            rules = rules.with(Rule::SansIoCore);
        }
        if self.uniq {
            rules = rules.with(Rule::MutRef);
        }
        rules
    }

    /// `base` with each denied/warned/allowed rule's level applied, followed by the `warnings`
    /// meta-lint for rustc lints whose resolved level is `Advice` (`Warn`).
    pub fn levels(&self, base: Levels) -> Levels {
        let mut levels = base.0;
        for (slot, rule) in levels.iter_mut().zip(Rule::ALL) {
            if self.denies(rule) {
                *slot = Level::Error;
            } else if self.warns(rule) {
                *slot = Level::Advice;
            } else if self.allows(rule) {
                *slot = Level::Off;
            }
            if *slot == Level::Advice && (RUSTC_LINTS & rule.bit() != 0) {
                *slot = match self.warnings {
                    WarningsLevel::Allow => Level::Off,
                    WarningsLevel::Deny | WarningsLevel::Forbid => Level::Error,
                    WarningsLevel::Default | WarningsLevel::Warn => Level::Advice,
                };
            }
        }
        Levels(levels)
    }

    /// Applies `forbid(warnings)`: every rustc lint currently at `Warn` (explicitly warned, or
    /// warn-by-default and not already allowed/denied) becomes forbidden so later/inner specific
    /// `allow` attributes cannot downgrade it.
    fn apply_forbid_warnings(&mut self) {
        self.warnings = WarningsLevel::Forbid;
        let newly_forbidden = (self.warned & RUSTC_LINTS) | (RUSTC_WARN_BY_DEFAULT & !self.allowed & !self.denied);
        self.forbidden |= newly_forbidden;
        self.denied |= newly_forbidden;
        self.warned &= !newly_forbidden;
    }

    /// One attribute, its `#![` or `#[` already read: what it marks, then the rest of it up to
    /// its `]`. `crate_attrs` is `true` only at file top (where `#![no_std]` and `#![blaze::..]`
    /// crate-level marks live).
    fn attribute(&mut self, sig: &mut Peekable<Sig<'_>>, crate_attrs: bool, cfg: CfgAtoms<'_>, cfg_depth: usize) {
        self.attribute_meta(sig, crate_attrs, cfg, cfg_depth, TokenKind::CloseBracket);
        close(sig, 1);
    }

    /// Parses one attribute meta item (either directly inside `#[..]`/`#![..]` with `stop =
    /// CloseBracket`, or as one comma-separated entry inside `cfg_attr(pred, attr1, attr2)` with
    /// `stop = CloseParen`). Does NOT consume the closing `stop` delimiter.
    fn attribute_meta(&mut self, sig: &mut Peekable<Sig<'_>>, crate_attrs: bool, cfg: CfgAtoms<'_>, cfg_depth: usize, stop: TokenKind) {
        let Some(&(kind, head, _)) = sig.peek() else { return };
        if kind == stop || kind == TokenKind::Comma {
            return;
        }
        sig.next();
        if kind != TokenKind::Ident {
            if matches!(kind, TokenKind::OpenBracket | TokenKind::OpenParen | TokenKind::OpenBrace) {
                close(sig, 1);
            }
            return;
        }
        match head {
            "no_std" if crate_attrs && (peek_is(sig, stop) || peek_is(sig, TokenKind::Comma)) => {
                self.no_std = true;
            }
            "blaze" if crate_attrs => match blaze_path(sig) {
                Some("sans_io") if peek_is(sig, stop) || peek_is(sig, TokenKind::Comma) => self.sans_io = true,
                Some("driver") if peek_is(sig, stop) || peek_is(sig, TokenKind::Comma) => self.driver = true,
                Some("uniq") if peek_is(sig, stop) || peek_is(sig, TokenKind::Comma) => self.uniq = true,
                Some("trust") if peek_is(sig, TokenKind::OpenParen) => {
                    if trust_unsafe(sig) {
                        self.trust_unsafe = true;
                    }
                }
                _ => skip_meta_rest(sig, stop),
            },
            "cfg_attr" if peek_is(sig, TokenKind::OpenParen) && cfg_depth < MAX_CFG_DEPTH => {
                sig.next();
                if eval_cfg_pred(sig, cfg, "", 0) == Some(true) {
                    // Consume comma after predicate and apply each comma-separated meta item.
                    while peek_is(sig, TokenKind::Comma) {
                        sig.next();
                        if peek_is(sig, TokenKind::CloseParen) {
                            break;
                        }
                        self.attribute_meta(sig, crate_attrs, cfg, cfg_depth + 1, TokenKind::CloseParen);
                        skip_meta_rest(sig, TokenKind::CloseParen);
                    }
                }
                close(sig, 1);
            }
            "forbid" if peek_is(sig, TokenKind::OpenParen) => {
                let items = lint_list(sig);
                self.forbidden |= items.rules;
                self.denied |= items.rules;
                self.warned &= !items.rules;
                self.allowed &= !items.rules;
                if items.warnings {
                    self.apply_forbid_warnings();
                }
            }
            "deny" if peek_is(sig, TokenKind::OpenParen) => {
                let items = lint_list(sig);
                let bits = items.rules & !self.forbidden;
                self.denied |= bits;
                self.warned &= !bits;
                self.allowed &= !bits;
                if items.warnings {
                    self.warnings = self.warnings.apply(WarningsLevel::Deny);
                }
            }
            "warn" if peek_is(sig, TokenKind::OpenParen) => {
                let items = lint_list(sig);
                let bits = items.rules & !self.forbidden;
                if matches!(self.warnings, WarningsLevel::Forbid) {
                    let forbid_bits = bits & RUSTC_LINTS;
                    let plain_bits = bits & !forbid_bits;
                    self.forbidden |= forbid_bits;
                    self.denied |= forbid_bits;
                    self.warned &= !forbid_bits;
                    self.allowed &= !forbid_bits;
                    self.warned |= plain_bits;
                    self.denied &= !plain_bits;
                    self.allowed &= !plain_bits;
                } else {
                    self.warned |= bits;
                    self.denied &= !bits;
                    self.allowed &= !bits;
                }
                if items.warnings {
                    self.warnings = self.warnings.apply(WarningsLevel::Warn);
                }
            }
            "allow" | "expect" if peek_is(sig, TokenKind::OpenParen) => {
                let items = lint_list(sig);
                let bits = items.rules & !self.forbidden;
                self.allowed |= bits;
                self.denied &= !bits;
                self.warned &= !bits;
                if items.warnings {
                    self.warnings = self.warnings.apply(WarningsLevel::Allow);
                }
            }
            _ => skip_meta_rest(sig, stop),
        }
    }
}

/// Skips any leftover tokens of one meta item up to (but not consuming) `,` or `stop` at depth 0.
fn skip_meta_rest(sig: &mut Peekable<Sig<'_>>, stop: TokenKind) {
    let mut depth = 0usize;
    while let Some(&(kind, _, _)) = sig.peek() {
        if depth == 0 && (kind == stop || kind == TokenKind::Comma) {
            return;
        }
        sig.next();
        depth = depth_of(kind, depth);
    }
}

/// Evaluates one `cfg` predicate from `sig` up to the delimiter/comma that ends it (without
/// consuming that trailing `,` or `)`). Bounded by [`MAX_CFG_DEPTH`].
fn eval_cfg_pred(sig: &mut Peekable<Sig<'_>>, cfg: CfgAtoms<'_>, prefix: &str, depth: usize) -> Option<bool> {
    if depth >= MAX_CFG_DEPTH {
        return None;
    }
    let (TokenKind::Ident, name, _) = sig.next()? else {
        return None;
    };
    if peek_is(sig, TokenKind::OpenParen) {
        sig.next();
        enum Op {
            All,
            Any,
            Not,
        }
        let (op, next_prefix) = match name {
            "all" => (Op::All, prefix),
            "target" => (Op::All, "target_"),
            "any" => (Op::Any, prefix),
            "not" => (Op::Not, prefix),
            _ => {
                close(sig, 1);
                return None;
            }
        };
        let mut acc = matches!(op, Op::All);
        let mut count = 0u32;
        let mut ok = true;
        // Bounded: each turn consumes one sub-predicate or closes `)`.
        loop {
            if peek_is(sig, TokenKind::CloseParen) {
                sig.next();
                break;
            }
            match eval_cfg_pred(sig, cfg, next_prefix, depth + 1) {
                Some(v) => {
                    count = count.saturating_add(1);
                    match op {
                        Op::All => acc = acc && v,
                        Op::Any => acc = acc || v,
                        Op::Not => acc = !v,
                    }
                }
                None => {
                    ok = false;
                    close(sig, 1);
                    break;
                }
            }
            if peek_is(sig, TokenKind::Comma) {
                sig.next();
            } else if peek_is(sig, TokenKind::CloseParen) {
                sig.next();
                break;
            } else {
                ok = false;
                close(sig, 1);
                break;
            }
        }
        if !ok {
            return None;
        }
        match op {
            Op::All | Op::Any => Some(acc),
            Op::Not => (count == 1).then_some(acc),
        }
    } else if peek_is(sig, TokenKind::Eq) {
        sig.next();
        let (TokenKind::Literal { .. }, raw, _) = sig.next()? else {
            return None;
        };
        let val = unquote_cfg_str(raw)?;
        Some(cfg.has(prefix, name, Some(val)))
    } else {
        Some(cfg.has(prefix, name, None))
    }
}

/// Unquotes a plain or raw Rust string literal for `cfg(key = "value")`.
fn unquote_cfg_str(raw: &str) -> Option<&str> {
    if let Some(inner) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return (!inner.contains('\\')).then_some(inner);
    }
    if let Some(rest) = raw.strip_prefix('r') {
        let hashes = rest.bytes().take_while(|&b| b == b'#').count();
        let after = rest.get(hashes..)?;
        let inner = after.strip_prefix('"')?;
        let trimmed = inner.get(..inner.len().checked_sub(1 + hashes)?)?;
        let tail = inner.get(trimmed.len()..)?;
        if tail.starts_with('"') && tail.get(1..).is_some_and(|t| t.bytes().all(|b| b == b'#')) {
            return Some(trimmed);
        }
    }
    None
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
        while let Some((kind, word, _)) = sig.next() {
            if kind != TokenKind::Ident {
                continue;
            }
            match word {
                "mod" => {
                    if let Some(&(TokenKind::Ident, name, _)) = sig.peek() {
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
    while let Some((kind, word, _)) = sig.next() {
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

/// The significant tokens of a text: whitespace and comments passed over, each with its text and
/// starting byte offset.
struct Sig<'t> {
    text: &'t str,
    pos: usize,
    tokens: Tokens<'t>,
}

impl<'t> Iterator for Sig<'t> {
    type Item = (TokenKind, &'t str, usize);

    fn next(&mut self) -> Option<(TokenKind, &'t str, usize)> {
        // Bounded: each turn takes one token of the text.
        loop {
            let token = self.tokens.next()?;
            let start = self.pos;
            self.pos = start + usize::try_from(token.len).unwrap_or(0);
            match token.kind {
                TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. } => continue,
                TokenKind::Eof => return None,
                kind => return Some((kind, self.text.get(start..self.pos).unwrap_or(""), start)),
            }
        }
    }
}

fn next_is(sig: &mut Peekable<Sig<'_>>, kind: TokenKind) -> bool {
    matches!(sig.next(), Some((k, _, _)) if k == kind)
}

fn peek_is(sig: &mut Peekable<Sig<'_>>, kind: TokenKind) -> bool {
    matches!(sig.peek(), Some((k, _, _)) if *k == kind)
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
        let Some((kind, _, _)) = sig.next() else { return };
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
    sig.next().map(|(_, name, _)| name)
}

/// Built-in rustc lint and lint-group names that map to typed rules tracked here.
fn rustc_lint_bits(name: &str) -> u64 {
    match name {
        "deprecated_safe_2024" | "deprecated_safe" => Rule::DeprecatedSafe2024.bit(),
        "unsafe_op_in_unsafe_fn" => Rule::UnsafeOpInUnsafeFn.bit(),
        "rust_2024_compatibility" => Rule::DeprecatedSafe2024.bit() | Rule::UnsafeOpInUnsafeFn.bit(),
        _ => 0,
    }
}

/// `(blaze::a, deprecated_safe_2024, warnings, reason = "..")` after `allow`, `expect`, `warn`,
/// `deny` or `forbid`: the rules of the `blaze::` and tracked rustc lints named plus whether
/// `warnings` was listed; the list read up to and with its `)`.
fn lint_list(sig: &mut Peekable<Sig<'_>>) -> LintItems {
    let mut items = LintItems::default();
    let mut depth = 0usize;
    let mut after_colon = false;
    // Bounded: each turn takes one token of the text.
    while let Some((kind, word, _)) = sig.next() {
        depth = depth_of(kind, depth);
        if depth == 0 {
            return items;
        }
        if depth == 1 && kind == TokenKind::Ident && !after_colon {
            if word == "blaze"
                && let Some(name) = blaze_path(sig)
                && let Some(rule) = Rule::from_lint(name)
            {
                items.rules |= rule.bit();
            } else if !peek_is(sig, TokenKind::Colon) && !peek_is(sig, TokenKind::Eq) {
                if word == "warnings" {
                    items.warnings = true;
                } else {
                    items.rules |= rustc_lint_bits(word);
                }
            }
        }
        after_colon = kind == TokenKind::Colon;
    }
    items
}

/// `(unsafe, reason = "..")` after `blaze::trust`: consumes from `(` through its matching `)`,
/// returning `true` iff `unsafe` and a non-empty `reason = "<text>"` are both present at depth 1
/// and well-formed.
fn trust_unsafe(sig: &mut Peekable<Sig<'_>>) -> bool {
    let Some((TokenKind::OpenParen, _, _)) = sig.next() else { return false };
    let mut depth = 1usize;
    let mut has_unsafe = false;
    let mut has_reason = false;
    let mut bad = false;
    // Bounded: each turn consumes one token of the text until the matching `)`.
    while depth > 0 {
        let Some((kind, word, _)) = sig.next() else { return false };
        if depth == 1 && kind == TokenKind::CloseParen {
            return !bad && has_unsafe && has_reason;
        }
        if depth == 1 && kind == TokenKind::Ident {
            if word == "unsafe" {
                if matches!(sig.peek(), Some(&(TokenKind::Comma | TokenKind::CloseParen, _, _))) {
                    has_unsafe = true;
                } else {
                    bad = true;
                }
                continue;
            }
            if word == "reason" {
                if peek_is(sig, TokenKind::Eq) {
                    sig.next();
                    if matches!(sig.peek(), Some(&(TokenKind::Literal { .. }, _, _))) {
                        if let Some((_, val, _)) = sig.next() {
                            if crate::engine::says_something(val)
                                && matches!(sig.peek(), Some(&(TokenKind::Comma | TokenKind::CloseParen, _, _)))
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
