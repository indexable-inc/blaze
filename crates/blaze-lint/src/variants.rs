//! `variant-count`: an integer written where the number of an enum's variants is meant.
//!
//! `pub const KINDS: usize = 21;` beside an `enum Kind` of 21 variants goes stale the day a
//! variant is added; `core::mem::variant_count::<Kind>()` (feature `variant_count`, a `const fn`)
//! cannot. The rule is the one of this crate that reads more than its item: whether `21` is a
//! count depends on the enums of the FILE. So it is two halves:
//!
//! - [`Variants::gather`]: one lex of the whole file, once, into two small tables in the caller's
//!   allocator, each capped (rule 18): every enum with at least [`MIN_COUNT`] variants and no
//!   `#[cfg]` variant (its name, its count, its module, whether a bare name reaches it), and every
//!   site (an integer literal of value at least [`MIN_COUNT`] in one of the three positions below,
//!   with its module and whether `usize` is its type).
//! - [`Variants::find`]: per unit, the sites in its own bytes (a binary search, then the run of
//!   them) matched against the enums by count. The per-item engine is not touched: its hot path
//!   costs nothing more.
//!
//! What a unit's findings of this rule are a function of beyond its own bytes is
//! [`Variants::unit_key`]: the matching enums' names, counts and places, item-relative and
//! position-free, so a driver joins it to the unit's key (rule 65) and an enum's edit relints
//! only the units with a literal of its count.
//!
//! The positions: (a) the whole initializer of a `const` or `static` (`const KINDS: usize = 21;`);
//! (b) an array type's or repeat expression's length (`[T; 21]`, `[x; 21]`); (c) the end of a
//! `..` range (`0..21`; `0..=20` is not one). A literal inside an attribute, inside an enum's own
//! braces (an explicit discriminant) or inside a `macro_rules!` body is no site, nor one that is
//! part of a larger expression (`21 + 1`), a float, or a tuple index.
//!
//! The message names each matching enum, never its line: a line is a position of the file, which
//! no record holds (rule 71), and the record would move with every line added above the enum.

use blaze_lex::{Cpu, FrontmatterAllowed, LiteralKind, TokenKind, tokenize};
use blaze_own::{AVec, Alloc};

use crate::{Arg, Edit, Finding, Findings, Fix, LintError, Run, Span, Template, Unit, count};

/// The smallest count the rule reads: 0, 1 and 2 are too common as plain numbers.
pub(crate) const MIN_COUNT: u32 = 3;
/// The most enums one file's table holds.
pub(crate) const MAX_ENUMS: u32 = 1 << 14;
/// The most sites one file's table holds.
pub(crate) const MAX_SITES: u32 = 1 << 16;
/// The deepest bracket nesting a file may have.
pub(crate) const MAX_DEPTH: u32 = 1 << 10;
/// How many tokens before a `=` the `const` or `static` of an initializer is looked for.
const LOOK_BACK: usize = 64;

/// Why a site's one enum is not fixed: the first [`Arg::Int`] of its finding.
pub(crate) const FIXED: u32 = 0;
/// The enum is in another module, or in a block: its bare name does not reach the site.
pub(crate) const ELSEWHERE: u32 = 1;
/// The enum has generic parameters, which the call would have to name.
pub(crate) const GENERIC: u32 = 2;
/// The site's type is not known to be `usize` (a range, a `u32` constant, a suffix).
pub(crate) const UNTYPED: u32 = 3;

/// The module of the file's root.
const ROOT: u64 = 0;

/// One enum of the file that has a count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EnumFact {
    /// Its name, in [`Variants::names`].
    name: Run,
    count: u32,
    /// The chain of inline `mod`s it is in, as a hash of their names ([`mix`]).
    module: u64,
    /// It is declared in a block (a function body, an `impl`, a `const` block), where a bare name
    /// from the module does not reach it.
    in_block: bool,
    generic: bool,
}

/// One integer literal in a counting position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Site {
    /// File-relative: [`Variants::find`] makes it item-relative.
    start: u32,
    end: u32,
    value: u32,
    module: u64,
    /// The literal's type is `usize`, so the call (a `usize`) keeps the program's types.
    usize_typed: bool,
}

/// A file's enums with counts and its counting sites: the fact `variant-count` reads, gathered
/// once per file ([`Variants::gather`]).
pub struct Variants<'a> {
    enums: AVec<'a, EnumFact>,
    names: AVec<'a, u8>,
    /// In file order.
    sites: AVec<'a, Site>,
    /// Bit `c` is set when some enum in `enums` has `count == c` (`c < 64`).
    count_mask: u64,
    /// Whether some enum in `enums` has `count >= 64`.
    has_large: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum G {
    Ident,
    Int,
    /// Any other literal, a lifetime, anything else.
    Other,
    P(u8),
}

#[derive(Clone, Copy)]
struct T {
    start: u32,
    end: u32,
    g: G,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum F {
    Mod,
    /// The enum open at this index of `open`.
    Enum(u32),
    Macro,
    Block,
    Paren,
    Bracket,
    Attr,
}

#[derive(Clone, Copy)]
struct Frame {
    f: F,
    module: u64,
    /// Inside a block (not reset by a `mod`, which starts its own module level).
    in_block: bool,
    /// No site in it: an attribute, an enum's braces, a `macro_rules!` body.
    quiet: bool,
}

/// An enum whose braces are open.
#[derive(Clone, Copy)]
struct Open {
    name: Span,
    generic: bool,
    count: u32,
    /// The current variant has its name.
    named: bool,
    /// A variant has `#[cfg(..)]` (its count is not one number), or the enum is in an attribute,
    /// a macro body or another enum's braces: no fact.
    skip: bool,
    module: u64,
    in_block: bool,
}

#[derive(Clone, Copy)]
enum Pending {
    None,
    Mod(Span),
    Enum(Span, bool),
    Macro,
}

/// One step of the module hash: FNV-1a over the parent's hash and the name, a separator after.
fn mix(parent: u64, name: &[u8]) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = parent ^ 0xcbf2_9ce4_8422_2325;
    for byte in name {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(PRIME);
    }
    (h ^ 0xff).wrapping_mul(PRIME)
}

/// A parsed integer literal and whether its suffix leaves its type as `usize`.
#[derive(Clone, Copy)]
struct IntLiteral {
    value: u32,
    suffix_usize: bool,
}

/// An integer literal's value and whether its type is `usize` by its suffix (none or `usize`).
/// `None` for a value past `u32` or a malformed literal.
fn int_value(lit: &str) -> Option<IntLiteral> {
    let b = lit.as_bytes();
    let (radix, mut at) = match b {
        [b'0', b'x', ..] => (16u32, 2usize),
        [b'0', b'o', ..] => (8, 2),
        [b'0', b'b', ..] => (2, 2),
        _ => (10, 0),
    };
    let mut value: u32 = 0;
    let mut digits = 0usize;
    // Bounded: one turn per byte of the literal.
    while let Some(c) = b.get(at).copied() {
        let digit = match c {
            b'_' => {
                at += 1;
                continue;
            }
            b'0'..=b'9' => u32::from(c - b'0'),
            b'a'..=b'f' if radix == 16 => u32::from(c - b'a') + 10,
            b'A'..=b'F' if radix == 16 => u32::from(c - b'A') + 10,
            _ => break,
        };
        if digit >= radix {
            return None;
        }
        value = value.checked_mul(radix)?.checked_add(digit)?;
        digits += 1;
        at += 1;
    }
    if digits == 0 {
        return None;
    }
    let suffix = &lit[at..];
    Some(IntLiteral { value, suffix_usize: suffix.is_empty() || suffix == "usize" })
}

impl<'a> Variants<'a> {
    /// No enum, no site: what a file without the word `enum` has.
    pub fn new_in(alloc: Alloc<'a>) -> Variants<'a> {
        Variants {
            enums: AVec::new_in(alloc),
            names: AVec::new_in(alloc),
            sites: AVec::new_in(alloc),
            count_mask: 0,
            has_large: false,
        }
    }

    /// The fact of the whole file `text`: one lex of it, its tables and scratch in `alloc`. A
    /// file without the bytes `enum` has no enum to count, so it is not lexed.
    pub fn gather(text: &str, alloc: Alloc<'a>) -> Result<Variants<'a>, LintError> {
        if u32::try_from(text.len()).is_err() {
            return Err(LintError::TooLong { len: text.len() });
        }
        let mut made = Variants::new_in(alloc);
        if !text.contains("enum") {
            return Ok(made);
        }
        let toks = lex(text, alloc);
        let mut frames: AVec<'_, Frame> = AVec::new_in(alloc);
        let mut open: AVec<'_, Open> = AVec::new_in(alloc);
        let mut pending = Pending::None;
        let root = Frame { f: F::Block, module: ROOT, in_block: false, quiet: false };
        let word = |t: T| &text[t.start as usize..t.end as usize];
        let is = |i: usize, g: G| toks.get(i).is_some_and(|t| t.g == g);
        for i in 0..toks.len() {
            let t = toks[i];
            let top = frames.last().copied().unwrap_or(root);
            match t.g {
                G::Ident => {
                    if let F::Enum(at) = top.f {
                        open[at as usize].named = true;
                    }
                    let wlen = t.end - t.start;
                    if wlen == 3 || wlen == 4 || wlen == 11 {
                        let next = toks.get(i + 1).copied();
                        match (word(t), next) {
                            ("mod", Some(n)) if n.g == G::Ident => pending = Pending::Mod(Span::new(n.start, n.end)),
                            ("enum", Some(n)) if n.g == G::Ident => {
                                pending = Pending::Enum(Span::new(n.start, n.end), is(i + 2, G::P(b'<')));
                            }
                            ("macro_rules", Some(n)) if n.g == G::P(b'!') => pending = Pending::Macro,
                            _ => {}
                        }
                    }
                }
                G::Int if !top.quiet => {
                    if let Some(site) = site_at(text, &toks, i, top) {
                        if count(made.sites.len())? >= MAX_SITES {
                            return Err(LintError::Capped { what: "variant-count sites in one file", cap: MAX_SITES });
                        }
                        made.sites.push(site);
                    }
                }
                G::P(b';') => pending = Pending::None,
                G::P(b',') => {
                    if let F::Enum(at) = top.f {
                        let e = &mut open[at as usize];
                        if e.named {
                            e.count += 1;
                        }
                        e.named = false;
                    }
                }
                G::P(open_char @ (b'{' | b'(' | b'[')) => {
                    if count(frames.len())? >= MAX_DEPTH {
                        return Err(LintError::Capped { what: "bracket nesting in one file", cap: MAX_DEPTH });
                    }
                    let frame = match open_char {
                        b'{' => {
                            let made_frame = match pending {
                                Pending::Mod(name) => Frame {
                                    f: F::Mod,
                                    module: mix(top.module, name.of(text).as_bytes()),
                                    in_block: false,
                                    quiet: top.quiet,
                                },
                                Pending::Enum(name, generic) => {
                                    let at = count(open.len())?;
                                    // An enum in an attribute, a macro body or another enum's
                                    // braces is no fact either.
                                    open.push(Open {
                                        name,
                                        generic,
                                        count: 0,
                                        named: false,
                                        skip: top.quiet,
                                        module: top.module,
                                        in_block: top.in_block,
                                    });
                                    Frame { f: F::Enum(at), module: top.module, in_block: top.in_block, quiet: true }
                                }
                                Pending::Macro => Frame { f: F::Macro, module: top.module, in_block: true, quiet: true },
                                Pending::None => Frame { f: F::Block, module: top.module, in_block: true, quiet: top.quiet },
                            };
                            pending = Pending::None;
                            made_frame
                        }
                        b'(' => Frame { f: F::Paren, ..top },
                        _ => {
                            let pound = i >= 1 && is(i - 1, G::P(b'#'));
                            let inner = i >= 2 && is(i - 1, G::P(b'!')) && is(i - 2, G::P(b'#'));
                            if pound || inner {
                                // `#[cfg(..)]` or `#[cfg_attr(..)]` on a variant: its count is not
                                // one number, so the enum is no fact.
                                if let F::Enum(at) = top.f {
                                    let cfg = toks.get(i + 1).is_some_and(|n| matches!(word(*n), "cfg" | "cfg_attr"));
                                    if cfg {
                                        open[at as usize].skip = true;
                                    }
                                }
                                Frame { f: F::Attr, quiet: true, ..top }
                            } else {
                                Frame { f: F::Bracket, ..top }
                            }
                        }
                    };
                    frames.push(frame);
                }
                G::P(b'}' | b')' | b']') => {
                    // A closer with nothing open is a syntax error the outline reports; this
                    // fact reads past it.
                    let Some(frame) = frames.pop() else { continue };
                    if let F::Enum(at) = frame.f {
                        let e = open[at as usize];
                        let total = e.count + u32::from(e.named);
                        if !e.skip && total >= MIN_COUNT {
                            if count(made.enums.len())? >= MAX_ENUMS {
                                return Err(LintError::Capped { what: "variant-count enums in one file", cap: MAX_ENUMS });
                            }
                            let from = count(made.names.len())?;
                            made.names.extend_from_slice(e.name.of(text).as_bytes());
                            let name = Run { from, to: count(made.names.len())? };
                            made.enums.push(EnumFact {
                                name,
                                count: total,
                                module: e.module,
                                in_block: e.in_block,
                                generic: e.generic,
                            });
                            if total < 64 {
                                made.count_mask |= 1u64 << total;
                            } else {
                                made.has_large = true;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(made)
    }

    /// The fact as borrowed columns: `Copy` and `Sync`, so a file's units linted on several
    /// workers read the one fact gathered for the file ([`VariantsView`]).
    pub fn view(&self) -> VariantsView<'_> {
        VariantsView {
            enums: &self.enums,
            names: &self.names,
            sites: &self.sites,
            count_mask: self.count_mask,
            has_large: self.has_large,
        }
    }

    /// What this rule's findings in the unit at `base` (its `len` bytes, its `holes`) read of the
    /// rest of the file, as a digest: each site's item-relative place, value and module with the
    /// name, count and place of every enum of that count. `None` when no site of the unit has an
    /// enum of its count: then nothing outside the unit moves its findings. A driver joins it to
    /// the unit's key.
    pub fn unit_key(&self, base: u32, len: u32, holes: &[Span]) -> Option<[u8; 32]> {
        self.view().unit_key(base, len, holes)
    }

    /// The findings of `variant-count` in `unit`, which starts at `base` of the file this fact
    /// was gathered from, pushed onto `out`.
    pub(crate) fn find(&self, unit: &Unit<'_>, base: u32, out: &mut Findings<'_>) -> Result<(), LintError> {
        self.view().find(unit, base, out)
    }
}

/// A unit's place in its file, as [`crate::Units`] plans it: its start, its length and its
/// children (unit-relative, in order). What `variant-count` finds a unit's sites by when the unit
/// is linted as its own text ([`crate::Units::own`]).
#[derive(Clone, Copy, Debug)]
pub struct Geometry<'h> {
    pub base: u32,
    pub len: u32,
    pub holes: &'h [Span],
}

/// [`Variants`] borrowed ([`Variants::view`]).
#[derive(Clone, Copy)]
pub struct VariantsView<'a> {
    enums: &'a [EnumFact],
    names: &'a [u8],
    sites: &'a [Site],
    count_mask: u64,
    has_large: bool,
}

impl<'a> VariantsView<'a> {
    #[inline(always)]
    fn may_match(&self, value: u32) -> bool {
        if value < 64 {
            (self.count_mask & (1u64 << value)) != 0
        } else {
            self.has_large
        }
    }
    /// The digest [`Variants::unit_key`] makes, its sites' places own places ([`crate::own_place`]):
    /// what a unit linted as its own text reads of the rest of the file, with no child's length.
    pub fn unit_key_own(&self, at: Geometry<'_>) -> Option<[u8; 32]> {
        self.key_of(at.base, at.len, at.holes, |range| Span::new(crate::own_place(at.holes, range.start), crate::own_place(at.holes, range.end)))
    }

    /// The findings of `variant-count` in the unit at `at`, linted as its own text, each placed
    /// in that text ([`crate::own_place`]), pushed onto `out`.
    pub(crate) fn find_own(&self, at: Geometry<'_>, out: &mut Findings<'_>) -> Result<(), LintError> {
        self.find_in(at.base, at.len, at.holes, |range| Span::new(crate::own_place(at.holes, range.start), crate::own_place(at.holes, range.end)), out)
    }

    fn unit_key(&self, base: u32, len: u32, holes: &[Span]) -> Option<[u8; 32]> {
        self.key_of(base, len, holes, |range| range)
    }

    fn find(&self, unit: &Unit<'_>, base: u32, out: &mut Findings<'_>) -> Result<(), LintError> {
        let len = count(unit.text.len())?;
        self.find_in(base, len, unit.holes, |range| range, out)
    }

    fn name(&self, e: &EnumFact) -> &'a [u8] {
        &self.names[e.name.from as usize..e.name.to as usize]
    }

    /// The sites in the own bytes of the unit at `base` of `len` bytes with `holes`
    /// (item-relative, in order), each with its item-relative range.
    fn sites_of<'s>(&'s self, base: u32, len: u32, holes: &'s [Span]) -> impl Iterator<Item = (Span, Site)> + 's {
        let end = base.saturating_add(len);
        let from = self.sites.partition_point(|s| s.start < base);
        self.sites[from..].iter().take_while(move |s| s.end <= end).filter_map(move |s| {
            let range = Span::new(s.start - base, s.end - base);
            let at = holes.partition_point(|h| h.end <= range.start);
            match holes.get(at) {
                Some(h) if h.start < range.end => None,
                _ => Some((range, *s)),
            }
        })
    }

    /// [`Variants::unit_key`], each site's place made by `place` from its item-relative range.
    fn key_of(&self, base: u32, len: u32, holes: &[Span], place: impl Fn(Span) -> Span) -> Option<[u8; 32]> {
        if self.enums.is_empty() {
            return None;
        }
        let mut hasher: Option<blaze_blake3::Hasher> = None;
        for (range, site) in self.sites_of(base, len, holes) {
            if !self.may_match(site.value) {
                continue;
            }
            let range = place(range);
            for e in self.enums.iter().filter(|e| e.count == site.value) {
                let h = hasher.get_or_insert_with(|| blaze_blake3::Hasher::new_derive_key("blaze-lint variant-count unit 1"));
                h.update(&range.start.to_le_bytes());
                h.update(&site.value.to_le_bytes());
                h.update(&site.module.to_le_bytes());
                h.update(&[u8::from(site.usize_typed)]);
                h.update(&(e.name.to - e.name.from).to_le_bytes());
                h.update(self.name(e));
                h.update(&e.module.to_le_bytes());
                h.update(&[u8::from(e.in_block), u8::from(e.generic)]);
            }
        }
        hasher.map(|h| h.finalize())
    }

    /// The findings of `variant-count` in the unit at `base` of `len` bytes with `holes`, each
    /// placed by `place` from its item-relative range, pushed onto `out`.
    fn find_in(&self, base: u32, len: u32, holes: &[Span], place: impl Fn(Span) -> Span, out: &mut Findings<'_>) -> Result<(), LintError> {
        if self.enums.is_empty() {
            return Ok(());
        }
        for (range, site) in self.sites_of(base, len, holes) {
            if !self.may_match(site.value) {
                continue;
            }
            let range = place(range);
            let mut matching = self.enums.iter().filter(|e| e.count == site.value);
            let Some(first) = matching.next() else { continue };
            let many = matching.next().is_some();
            // Two enums of the count: which one it counts is for a person, so no reason applies
            // and no fix is made.
            let reason = if many {
                FIXED
            } else if first.in_block || first.module != site.module {
                ELSEWHERE
            } else if first.generic {
                GENERIC
            } else if !site.usize_typed {
                UNTYPED
            } else {
                FIXED
            };
            let args_from = count(out.args.len())?;
            out.args.push(Arg::Int(reason));
            for e in self.enums.iter().filter(|e| e.count == site.value) {
                let from = count(out.text.len())?;
                out.text.extend_from_slice(self.name(e));
                out.args.push(Arg::Text(Run { from, to: count(out.text.len())? }));
            }
            let args = Run { from: args_from, to: count(out.args.len())? };
            let fixes_from = count(out.fixes.len())?;
            if !many && reason == FIXED {
                let from = count(out.text.len())?;
                out.text.extend_from_slice(b"core::mem::variant_count::<");
                out.text.extend_from_slice(self.name(first));
                out.text.extend_from_slice(b">()");
                let text = Run { from, to: count(out.text.len())? };
                let edits_from = count(out.edits.len())?;
                out.edits.push(Edit { range, text });
                out.fixes.push(Fix { edits: Run { from: edits_from, to: count(out.edits.len())? } });
            }
            let fixes = Run { from: fixes_from, to: count(out.fixes.len())? };
            let template = if many { Template::VariantCountMany } else { Template::VariantCount };
            out.list.push(Finding { template, range, args, fixes });
        }
        Ok(())
    }
}

/// The significant tokens of `text`: no whitespace, no comment.
fn lex<'a>(text: &str, alloc: Alloc<'a>) -> AVec<'a, T> {
    let mut toks = AVec::with_capacity_in(text.len() / 5 + 8, alloc);
    let mut pos = 0u32;
    for token in tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) {
        let (start, end) = (pos, pos + token.len);
        pos = end;
        let g = match token.kind {
            TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. } => continue,
            TokenKind::Eof => break,
            TokenKind::Ident => G::Ident,
            TokenKind::Literal { kind: LiteralKind::Int { .. }, .. } => G::Int,
            TokenKind::Semi => G::P(b';'),
            TokenKind::Comma => G::P(b','),
            TokenKind::Dot => G::P(b'.'),
            TokenKind::OpenParen => G::P(b'('),
            TokenKind::CloseParen => G::P(b')'),
            TokenKind::OpenBrace => G::P(b'{'),
            TokenKind::CloseBrace => G::P(b'}'),
            TokenKind::OpenBracket => G::P(b'['),
            TokenKind::CloseBracket => G::P(b']'),
            TokenKind::Pound => G::P(b'#'),
            TokenKind::Colon => G::P(b':'),
            TokenKind::Eq => G::P(b'='),
            TokenKind::Bang => G::P(b'!'),
            TokenKind::Lt => G::P(b'<'),
            TokenKind::Gt => G::P(b'>'),
            TokenKind::Minus => G::P(b'-'),
            TokenKind::And => G::P(b'&'),
            TokenKind::Or => G::P(b'|'),
            TokenKind::Plus => G::P(b'+'),
            TokenKind::Star => G::P(b'*'),
            TokenKind::Slash => G::P(b'/'),
            TokenKind::Caret => G::P(b'^'),
            TokenKind::Percent => G::P(b'%'),
            _ => G::Other,
        };
        toks.push(T { start, end, g });
    }
    toks
}

/// The site the integer literal at token `i` is, if it is one: its value at least
/// [`MIN_COUNT`], in one of the three positions, alone there.
fn site_at(text: &str, toks: &[T], i: usize, top: Frame) -> Option<Site> {
    let at = |j: usize| toks.get(j).copied();
    let back = |n: usize| i.checked_sub(n).and_then(at);
    let prev = back(1);
    let p = |tok: Option<T>, c: u8| tok.is_some_and(|t| t.g == G::P(c));
    // Fast pre-check before parsing the integer literal: every site is preceded by `;`, `.`, or `=`.
    if !matches!(prev, Some(T { g: G::P(b';' | b'.' | b'='), .. })) {
        return None;
    }
    let t = toks[i];
    let IntLiteral { value, suffix_usize } = int_value(&text[t.start as usize..t.end as usize])?;
    if value < MIN_COUNT {
        return None;
    }
    let joined = |a: Option<T>, b: Option<T>| matches!((a, b), (Some(a), Some(b)) if a.end == b.start);
    let next = at(i + 1);
    let site = |usize_typed: bool| Site { start: t.start, end: t.end, value, module: top.module, usize_typed };
    // (b) `[T; 21]`, `[x; 21]`: always a `usize`.
    if p(prev, b';') && p(next, b']') && top.f == F::Bracket {
        return Some(site(suffix_usize));
    }
    // (c) `a..21`, `..21`, the range alone up to a closer: not `..=`, not `...`.
    if p(prev, b'.') && p(back(2), b'.') && joined(back(2), prev) && !(p(back(3), b'.') && joined(back(3), back(2))) {
        let ends = match next {
            None => true,
            Some(n) => matches!(n.g, G::P(b')' | b']' | b'}' | b',' | b';' | b'{')),
        };
        // The range's type comes from its start or its use: not known from text.
        return if ends { Some(site(false)) } else { None };
    }
    // (a) `const N: T = 21;`, `static N: T = 21;`: the `=` alone (not `==`, `<=`, `+=`), and
    // the `const` or `static` of the item within a short look back at depth zero.
    if p(prev, b'=') && p(next, b';') {
        let before = back(2);
        let compound = joined(before, prev)
            && before.is_some_and(|b| matches!(b.g, G::P(b'=' | b'<' | b'>' | b'!' | b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b'&' | b'|')));
        if compound {
            return None;
        }
        let word = |tok: Option<T>| match tok {
            Some(t) if t.g == G::Ident => Some(&text[t.start as usize..t.end as usize]),
            _ => None,
        };
        // `: usize =`, not `::usize =`.
        let typed = word(before) == Some("usize") && p(back(3), b':') && !(p(back(4), b':') && joined(back(4), back(3)));
        let mut depth = 0usize;
        for n in 2..=LOOK_BACK + 1 {
            let Some(b) = back(n) else { return None };
            match b.g {
                G::P(b')' | b']') => depth += 1,
                G::P(b'(' | b'[') => depth = depth.checked_sub(1)?,
                G::P(b';' | b'{' | b'}' | b'=') if depth == 0 => return None,
                G::Ident if depth == 0 => match word(Some(b)) {
                    Some("const" | "static") => return Some(site(typed && suffix_usize)),
                    Some("let") => return None,
                    _ => {}
                },
                _ => {}
            }
        }
    }
    None
}
