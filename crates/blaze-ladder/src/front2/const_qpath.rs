//! Qualified constant leaves of a head's constant expression: `<B4 as Spec>::BITS` in
//! `[u8; ((<B4 as Spec>::BITS + <B1 as Spec>::BITS - 1) / 8) + 1]` (what `modular_bitfield`
//! writes for every bitfield struct: tempo's primitives).
//!
//! The Reference, "Associated Items", "Associated Constants": "An associated constant
//! definition defines a constant associated with a type"; its example "Using default values"
//! (`trait ConstantIdDefault { const ID: i32 = 1; }`, `impl ConstantIdDefault for Struct {}`,
//! `assert_eq!(1, <Struct as ConstantIdDefault>::ID)`): an impl that writes no definition has
//! the trait's default. So the leaf's value is found by SELECTION: the impl of the trait for
//! the written type, then that impl's definition or the trait's default.
//!
//! Why here and not in the expression's own evaluation: a checked body records a constant
//! path as `Def::Const(key)` with no arguments (blaze-typeck rules/lit_path.rs
//! `assoc_item_ty`), so the evaluator reads the TRAIT's constant with no `Self`
//! (`ConstFail::TraitConstNoSelf`). In a HEAD the written type is ground text, so the leaf is
//! answered before the expression is made an anonymous constant ([`super::anon`]):
//!
//! 1. [`Leaves`] finds each `<T as Trait>::NAME` of the text (a scan of its tokens; anything
//!    it does not read as exactly that form is left in the text, for the expression's own
//!    evaluation and its own refusal);
//! 2. [`substitute`] resolves `Trait` and `T` through the names library (`T` is a primitive
//!    or a struct, enum or union with no generic arguments: what `ConstsWorld::impl_const`
//!    selects without a solver), asks the heads for each leaf's value under its need key
//!    ([`need_key`]; every leaf with no value yet is asked in one pass, so a struct of thirty
//!    bitfields is one round), and writes the text with each leaf replaced by its value as a
//!    typed literal (`4usize`);
//! 3. the driver's `check::eval_len` calls [`eval_asked`], which makes the trait's members
//!    and the impl candidates of (trait, type) (`check::make`, the body check's own fills:
//!    `Buckets`), then has `Consts::trait_const_value` select (`ConstsWorld::impl_const`, the
//!    one place an impl's constant is selected by key) and evaluate (the evaluator's `Settle`
//!    work list; a cycle of constants is its typed `Cycle` refusal).
//!
//! Refused, each by name: no impl selected (`ConstFail::NoImplConst`, naming the trait and
//! the type), a trait without that constant (`NoTraitConst`), a constant whose type is not
//! `bool`, `char` or an integer (`QPathType`), a leaf needed while it is being answered
//! (`QPathCycle`), more than [`MOST_NESTED`] leaves answered inside one another
//! (`QPathDepth`). A leaf whose type is a generic parameter never reaches here: the builder
//! refuses the expression first (`anon::mentions`, "a constant expression over a generic
//! parameter").
//!
//! OWED: a qualified path INSIDE a constant's body (`const A: usize = <B4 as Spec>::BITS;`)
//! is still `TraitConstNoSelf`: the checker's record must hold the path's arguments and the
//! evaluator must instance the constant at them (blaze-typeck, blaze-consteval).

use blaze_id::ItemKey as LibKey;
use blaze_ids::ContentId;
use blaze_own::{Alloc, String, Vec};
use blaze_typeck as tc;
use blaze_typeck::{PNode, Shape};

use super::anon::prim_name;
use super::check::{Records, make_for};
use super::consts::{ConstFail, int_of};
use super::heads::Want;
use super::keys::{MemberKind, anon_const_key, checker_of};
use super::source::NamesSource;
use super::types::{Heads, PRIMS};
use super::world_items::Fill;
use crate::resolve::{Kind, ResolveError, Target};

/// The most segments of a leaf's trait or type path (rule 18); a longer one is left in the
/// text.
pub const MOST_SEGS: usize = 8;

/// The most leaves answered inside one another (an impl header that itself needs a leaf):
/// past this the innermost is `ConstFail::QPathDepth` (rule 18).
pub const MOST_NESTED: usize = 16;

/// One `<T as Trait>::NAME` in an expression's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Leaf<'t> {
    /// The byte range of the whole leaf in the text, from its `<` to the end of `NAME`.
    pub start: usize,
    pub end: usize,
    /// `T` as written, trimmed.
    pub self_ty: &'t str,
    /// `Trait` as written, trimmed.
    pub trait_: &'t str,
    pub name: &'t str,
}

/// The leaves of a text, in order. A `<` is the start of one only where an operand may start
/// (not after an operand, where it compares or shifts; not after `::`, where it opens generic
/// arguments; not after `as`, where it opens a type).
pub struct Leaves<'t> {
    text: &'t str,
    at: usize,
    /// The last token ended an operand (a name, a literal, a closing bracket).
    operand: bool,
    after_as: bool,
    after_colons: bool,
}

/// Words after which an operand starts.
const NOT_OPERANDS: [&str; 12] = ["if", "else", "match", "return", "in", "while", "let", "mut", "const", "unsafe", "move", "break"];

fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// The index after the char literal that starts at `text[i]` (a `'`); `None` for a lifetime.
fn char_end(text: &str, i: usize) -> Option<usize> {
    let rest = text.get(i + 1..)?;
    let first = rest.chars().next()?;
    let bytes = rest.as_bytes();
    if first == '\\' {
        let mut j = 2;
        while j < bytes.len() && bytes[j] != b'\'' {
            j += 1;
        }
        return if j < bytes.len() { Some(i + 1 + j + 1) } else { None };
    }
    let n = first.len_utf8();
    if bytes.get(n) == Some(&b'\'') { Some(i + 1 + n + 1) } else { None }
}

/// The `>` that closes the `<` at `text[open]`; `None` when the group holds a block, a
/// string or a `;`, or does not close (such a group is not read).
fn close_angle(text: &str, open: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut depth = 0u32;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'<' => depth += 1,
            // `->` and `=>` close nothing.
            b'>' if i > 0 && (b[i - 1] == b'-' || b[i - 1] == b'=') => {}
            b'>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
            b'{' | b'}' | b';' | b'"' => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

/// `T as Trait` split at its `as` outside every bracket.
fn split_as(inner: &str) -> Option<(&str, &str)> {
    let b = inner.as_bytes();
    let (mut depth, mut i) = (0u32, 0usize);
    while i < b.len() {
        let c = b[i];
        if word(c) {
            let s = i;
            while i < b.len() && word(b[i]) {
                i += 1;
            }
            if depth == 0 && inner.get(s..i) == Some("as") {
                let l = inner.get(..s)?.trim();
                let r = inner.get(i..)?.trim();
                return if l.is_empty() || r.is_empty() { None } else { Some((l, r)) };
            }
            continue;
        }
        match c {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' | b')' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    None
}

/// The leaf whose `<` is at `open` and whose `>` is at `close`: `None` when it is not `<T as
/// Trait>::NAME` used as a value (no `as`; a further segment, a call, a macro or a struct
/// expression after the name).
fn leaf_at(text: &str, open: usize, close: usize) -> Option<Leaf<'_>> {
    let (self_ty, trait_) = split_as(text.get(open + 1..close)?)?;
    let b = text.as_bytes();
    let mut i = skip_ws(b, close + 1);
    if b.get(i) != Some(&b':') || b.get(i + 1) != Some(&b':') {
        return None;
    }
    i = skip_ws(b, i + 2);
    let start = i;
    while i < b.len() && word(b[i]) {
        i += 1;
    }
    let name = text.get(start..i)?;
    if name.is_empty() || b[start].is_ascii_digit() {
        return None;
    }
    if matches!(b.get(skip_ws(b, i)), Some(b':' | b'(' | b'!' | b'{')) {
        return None;
    }
    Some(Leaf { start: open, end: i, self_ty, trait_, name })
}

impl<'t> Leaves<'t> {
    pub fn of(text: &'t str) -> Leaves<'t> {
        Leaves { text, at: 0, operand: false, after_as: false, after_colons: false }
    }
}

impl<'t> Iterator for Leaves<'t> {
    type Item = Leaf<'t>;

    fn next(&mut self) -> Option<Leaf<'t>> {
        let text = self.text;
        let b = text.as_bytes();
        while self.at < b.len() {
            let i = self.at;
            let c = b[i];
            if c.is_ascii_whitespace() {
                self.at = i + 1;
                continue;
            }
            let (was_as, was_colons, was_operand) = (self.after_as, self.after_colons, self.operand);
            self.after_as = false;
            self.after_colons = false;
            if word(c) {
                let mut j = i;
                while j < b.len() && word(b[j]) {
                    j += 1;
                }
                // `word` splits only at ASCII bytes, so both ends are char boundaries.
                let w = text.get(i..j)?;
                self.at = j;
                self.after_as = w == "as";
                self.operand = !(w == "as" || NOT_OPERANDS.contains(&w));
                continue;
            }
            match c {
                b'"' => {
                    let mut j = i + 1;
                    while j < b.len() && b[j] != b'"' {
                        j += if b[j] == b'\\' { 2 } else { 1 };
                    }
                    if j >= b.len() {
                        self.at = b.len();
                        return None;
                    }
                    self.at = j + 1;
                    self.operand = true;
                }
                b'\'' => match char_end(text, i) {
                    Some(j) => {
                        self.at = j;
                        self.operand = true;
                    }
                    // A lifetime or a label.
                    None => {
                        self.at = i + 1;
                        self.operand = false;
                    }
                },
                b':' => {
                    if b.get(i + 1) == Some(&b':') {
                        self.at = i + 2;
                        self.after_colons = true;
                    } else {
                        self.at = i + 1;
                    }
                    self.operand = false;
                }
                // Generic arguments (`size_of::<u8>`) or a type (`x as <T as Tr>::Out`):
                // skipped whole.
                b'<' if was_colons || was_as => match close_angle(text, i) {
                    Some(close) => {
                        self.at = close + 1;
                        self.operand = true;
                    }
                    None => {
                        self.at = b.len();
                        return None;
                    }
                },
                // A comparison or a shift: `<`, `<=`, `<<`, `<<=`.
                b'<' if was_operand => {
                    let mut j = i + 1;
                    while j < b.len() && (b[j] == b'<' || b[j] == b'=') {
                        j += 1;
                    }
                    self.at = j;
                    self.operand = false;
                }
                b'<' => {
                    let Some(close) = close_angle(text, i) else {
                        self.at = b.len();
                        return None;
                    };
                    self.operand = true;
                    match leaf_at(text, i, close) {
                        Some(leaf) => {
                            self.at = leaf.end;
                            return Some(leaf);
                        }
                        None => self.at = close + 1,
                    }
                }
                b')' | b']' | b'}' => {
                    self.at = i + 1;
                    self.operand = true;
                }
                _ => {
                    self.at = i + 1;
                    self.operand = false;
                }
            }
        }
        None
    }
}

/// A plain path's segments (`a::b`, `::a::b`) into `out`: whether it starts with `::` and how
/// many there are; `None` for anything else (generic arguments, more than [`MOST_SEGS`]).
fn segments<'t>(text: &'t str, out: &mut [&'t str; MOST_SEGS]) -> Option<(bool, usize)> {
    let text = text.trim();
    let (abs, rest) = match text.strip_prefix("::") {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let mut n = 0;
    for s in rest.split("::") {
        let s = s.trim();
        let ident = s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && s.chars().all(|c| c.is_alphanumeric() || c == '_');
        if !ident || n == MOST_SEGS {
            return None;
        }
        out[n] = s;
        n += 1;
    }
    Some((abs, n))
}

/// The ground `Self` of a leaf: what `ConstsWorld::impl_const` selects an impl for without a
/// solver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QSelf {
    /// `bool`, `char` or an integer type, with its written name.
    Prim(&'static str, PNode),
    /// A struct, enum or union with no generic arguments.
    Adt(LibKey),
}

/// One leaf the type builder asked for.
pub struct QAsk {
    /// What `Why::NeedConst`, `Maps::const_bits` and `Maps::anon_failed` are keyed by
    /// ([`need_key`]).
    pub need: LibKey,
    pub trait_: LibKey,
    pub self_: QSelf,
    pub name: String<Alloc<'static>>,
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// The need key of `<self_ as trait_>::name`: a function of the three and nothing else (no
/// module, no position: every head that writes the leaf shares one value). Its first 16 bytes
/// are a member key under the TRAIT's checker key (the form of an anonymous constant's key,
/// keys.rs `anon_const_key`, so the names library finds its crate by the prefix), the rest a
/// hash under this module's own domain: no library item and no anonymous constant has it.
pub fn need_key(trait_: LibKey, self_: QSelf, name: &str, scratch: Alloc<'_>) -> LibKey {
    let mut member = String::new_in(scratch);
    member.push_str("qualified <");
    match self_ {
        QSelf::Prim(written, _) => member.push_str(written),
        QSelf::Adt(k) => {
            for &byte in k.0.0.iter() {
                member.push(char::from(HEX[usize::from(byte >> 4)]));
                member.push(char::from(HEX[usize::from(byte & 15)]));
            }
        }
    }
    member.push_str(" as Self>::");
    member.push_str(name);
    let first = anon_const_key(checker_of(trait_), member.as_str()).0.to_be_bytes();
    let rest = ContentId::of("blaze-ladder front2 qualified constant need 1", &first);
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(&first);
    bytes[16..].copy_from_slice(&rest.0[..16]);
    blaze_id::ItemKey(ContentId(bytes))
}

/// The asked leaves and what was learned of them, kept with the heads (`Maps::qpaths`).
pub struct Table {
    /// Every leaf asked, in asking order; one is asked once.
    asked: Vec<QAsk, Alloc<'static>>,
    /// The type of each answered leaf's value (its bits are in `Maps::const_bits`).
    tys: Vec<(LibKey, PNode), Alloc<'static>>,
    /// The leaves being answered now, outermost first.
    running: [Option<LibKey>; MOST_NESTED],
}

impl Default for Table {
    fn default() -> Table {
        Table { asked: Vec::new_in(Alloc::global()), tys: Vec::new_in(Alloc::global()), running: [None; MOST_NESTED] }
    }
}

impl Table {
    /// Keeps `ask` for [`eval_asked`]; one asked twice is kept once.
    pub fn ask(&mut self, ask: QAsk) {
        if !self.asked.as_slice().iter().any(|a| a.need == ask.need) {
            self.asked.push(ask);
        }
    }

    /// The type of the answered leaf `need`.
    pub fn ty(&self, need: LibKey) -> Option<PNode> {
        self.tys.as_slice().iter().find(|(k, _)| *k == need).map(|&(_, t)| t)
    }

    fn has(&self, need: LibKey) -> bool {
        self.asked.as_slice().iter().any(|a| a.need == need)
    }

    /// Whether `need` is the need key of a qualified leaf asked here (answered by
    /// [`eval_asked`], never evaluated by its checker key): what consts.rs `Consts::value_at`
    /// asks before it files a constant an evaluation needs first.
    pub fn is_leaf(&self, need: LibKey) -> bool {
        self.has(need)
    }

    fn at(&self, i: usize) -> Option<(LibKey, LibKey, QSelf, String<Alloc<'static>>)> {
        self.asked.as_slice().get(i).map(|a| (a.need, a.trait_, a.self_, a.name.clone()))
    }

    fn is_running(&self, need: LibKey) -> bool {
        self.running.iter().any(|r| *r == Some(need))
    }

    /// Marks `need` as being answered; `false` when [`MOST_NESTED`] already are.
    fn enter(&mut self, need: LibKey) -> bool {
        match self.running.iter_mut().find(|r| r.is_none()) {
            Some(slot) => {
                *slot = Some(need);
                true
            }
            None => false,
        }
    }

    fn leave(&mut self, need: LibKey) {
        if let Some(slot) = self.running.iter_mut().find(|r| **r == Some(need)) {
            *slot = None;
        }
    }
}

/// Why [`substitute`] has no text yet.
#[derive(Debug)]
pub enum Stop<'h> {
    /// A leaf has no value yet: the driver evaluates this need key (and every other leaf
    /// asked with it) and the head is made again.
    Need(LibKey),
    /// The leaf at `start..end` of the text was tried and refused: `why` is the refusal.
    Failed { start: usize, end: usize, why: &'h str },
}

/// Writes `v` in decimal.
fn push_dec(out: &mut String<Alloc<'_>>, mut v: u128) {
    let mut digits = [0u8; 39];
    let mut n = 0;
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        out.push(char::from(digits[n]));
    }
}

/// Writes the value `bits` of type `ty` (its bits in `ty`'s own width, as `Maps::const_bits`
/// holds them) as a typed literal expression: `4usize`, `true`, `'\u{41}'`, and a negative
/// integer as `(-127i8 - 1i8)` (every literal in it is in range: the Reference, "Integer
/// literal expressions"). `false` for a type that is not `bool`, `char` or an integer.
pub(super) fn push_literal(out: &mut String<Alloc<'_>>, ty: PNode, bits: u128) -> bool {
    let Some(prim) = prim_name(ty) else { return false };
    match ty {
        PNode::Bool => out.push_str(if bits & 1 == 1 { "true" } else { "false" }),
        PNode::Char => {
            out.push_str("'\\u{");
            let mut shift = 32u32;
            let mut started = false;
            while shift > 0 {
                shift -= 4;
                let nibble = ((bits >> shift) & 15) as usize;
                if nibble != 0 || started || shift == 0 {
                    started = true;
                    out.push(char::from(HEX[nibble]));
                }
            }
            out.push_str("}'");
        }
        PNode::Uint(_) => {
            push_dec(out, bits);
            out.push_str(prim);
        }
        PNode::Int(t) => {
            let width = match t {
                tc::IntTy::I8 => 8u32,
                tc::IntTy::I16 => 16,
                tc::IntTy::I32 => 32,
                // `isize` is 64 bits on every target blaze builds for (types.rs `end_value`).
                tc::IntTy::I64 | tc::IntTy::Isize => 64,
                tc::IntTy::I128 => 128,
            };
            let value = if width == 128 { bits as i128 } else { ((bits << (128 - width)) as i128) >> (128 - width) };
            if value < 0 {
                // -(|v| - 1) - 1: `|v| - 1` is at most the type's maximum.
                out.push_str("(-");
                push_dec(out, value.unsigned_abs() - 1);
                out.push_str(prim);
                out.push_str(" - 1");
                out.push_str(prim);
                out.push(')');
            } else {
                push_dec(out, value as u128);
                out.push_str(prim);
            }
        }
        _ => return false,
    }
    true
}

/// The ground `Self` a resolved type path names, if it is one a leaf is answered for.
fn self_of<H: Heads + ?Sized>(heads: &H, found: Target, segs: usize) -> Option<QSelf> {
    if found.consumed as usize != segs {
        return None;
    }
    match found.kind {
        Kind::Builtin(name) => {
            let &(written, node) = PRIMS.iter().find(|(p, _)| *p == name)?;
            prim_name(node).map(|_| QSelf::Prim(written, node))
        }
        Kind::Struct | Kind::Enum | Kind::Union => match heads.adt_generics(found.key) {
            // `<Wrap as Tr>::N` with `Wrap<T>`: not a ground type as written.
            Some((n, _)) if n != 0 => None,
            _ => Some(QSelf::Adt(found.key)),
        },
        _ => None,
    }
}

/// `text` with every qualified constant leaf replaced by its value ([`push_literal`]).
/// `Ok(None)`: the text has no leaf, or one this module does not answer (a trait or type the
/// names library does not resolve to a trait and a ground type, heads with no evaluator
/// behind them): the text is then evaluated as written, and refused by its own name.
/// `resolve` reads a path in the head's module, in the type namespace.
pub fn substitute<'h, H: Heads + ?Sized>(
    heads: &'h H,
    resolve: &dyn Fn(bool, &[&str]) -> Result<Target, ResolveError>,
    text: &str,
    alloc: Alloc<'static>,
) -> Result<Option<String<Alloc<'static>>>, Stop<'h>> {
    let mut out = String::new_in(alloc);
    let mut copied = 0usize;
    let mut first_need: Option<LibKey> = None;
    let mut any = false;
    for leaf in Leaves::of(text) {
        any = true;
        let mut tsegs = [""; MOST_SEGS];
        let Some((tabs, tn)) = segments(leaf.trait_, &mut tsegs) else { return Ok(None) };
        let Ok(tr) = resolve(tabs, &tsegs[..tn]) else { return Ok(None) };
        if tr.kind != Kind::Trait || tr.consumed as usize != tn {
            return Ok(None);
        }
        let mut ssegs = [""; MOST_SEGS];
        let Some((sabs, sn)) = segments(leaf.self_ty, &mut ssegs) else { return Ok(None) };
        let Ok(st) = resolve(sabs, &ssegs[..sn]) else { return Ok(None) };
        let Some(self_) = self_of(heads, st, sn) else { return Ok(None) };
        let need = need_key(tr.key, self_, leaf.name, alloc);
        if let Some(bits) = heads.const_bits(need) {
            let Some(ty) = heads.qpath_ty(need) else { return Ok(None) };
            let Some(before) = text.get(copied..leaf.start) else { return Ok(None) };
            out.push_str(before);
            if !push_literal(&mut out, ty, bits) {
                return Ok(None);
            }
            copied = leaf.end;
            continue;
        }
        if let Some(why) = heads.anon_failed(need) {
            return Err(Stop::Failed { start: leaf.start, end: leaf.end, why });
        }
        let ask = QAsk { need, trait_: tr.key, self_, name: String::from_str_in(leaf.name, alloc) };
        if !heads.ask_qpath(ask) {
            return Ok(None);
        }
        if first_need.is_none() {
            first_need = Some(need);
        }
    }
    if let Some(need) = first_need {
        return Err(Stop::Need(need));
    }
    if !any {
        return Ok(None);
    }
    let Some(tail) = text.get(copied..) else { return Ok(None) };
    out.push_str(tail);
    Ok(Some(out))
}

/// Answers the leaf under need key `c` and every other asked leaf that has no answer yet (one
/// round for a head of many leaves). `false`: `c` is not a leaf's need key, and nothing was
/// done. A value is filed in `Maps::const_bits` with its type in the table; a refusal is
/// filed in `Maps::anon_failed` as printed, and the head that wrote the leaf is refused by
/// name when it is made again ([`substitute`]'s `Stop::Failed`), and only that head.
pub fn eval_asked<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, c: LibKey) -> bool {
    if !r.store.maps.qpaths.borrow().has(c) {
        return false;
    }
    let mut i = 0usize;
    loop {
        let next = r.store.maps.qpaths.borrow().at(i);
        let Some((need, trait_, self_, name)) = next else { break };
        i += 1;
        if r.store.maps.const_bits.contains_key(&need) || r.store.maps.anon_failed.contains_key(&need) {
            continue;
        }
        let running = r.store.maps.qpaths.borrow().is_running(need);
        let out = if running {
            // Another leaf's answer is under way further out: left to its own call. `c`
            // itself under way is a leaf that needs itself.
            if need != c {
                continue;
            }
            Err(ConstFail::QPathCycle { need: checker_of(need) })
        } else if !r.store.maps.qpaths.borrow_mut().enter(need) {
            Err(ConstFail::QPathDepth { need: checker_of(need), most: MOST_NESTED as u32 })
        } else {
            let out = eval_one(r, need, trait_, self_, name.as_str());
            r.store.maps.qpaths.borrow_mut().leave(need);
            out
        };
        // A leaf refused as its own cycle while it was under way keeps that refusal.
        if r.store.maps.anon_failed.contains_key(&need) {
            continue;
        }
        match out {
            Ok((bits, ty)) => {
                r.store.maps.file_bits(need, bits);
                r.store.maps.qpaths.borrow_mut().tys.push((need, ty));
            }
            Err(e) => {
                use core::fmt::Write as _;
                let mut why = String::new_in(r.store.tys.alloc());
                if write!(why, "{e}").is_err() {
                    why.push_str(" (the rest of this refusal could not be written)");
                }
                r.store.maps.noted_const_result(need);
                r.store.maps.anon_failed.insert(need, why.as_str().into());
            }
        }
    }
    true
}

/// Selects and evaluates one leaf: its value's bits and its type.
fn eval_one<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, need_lib: LibKey, trait_lib: LibKey, self_: QSelf, name: &str) -> Result<(u128, PNode), ConstFail> {
    let need = checker_of(need_lib);
    let trait_ = r.keys.item(trait_lib).map_err(|clash| ConstFail::QPathKey { need, clash })?;
    let wrap = |error| ConstFail::Check { item: need, error };
    // The trait's head and members: its constants get their rows (heads.rs `trait_members`).
    make_for(r, Fill::Store(Want::Head(trait_lib)), need).map_err(wrap)?;
    make_for(r, Fill::Store(Want::Members(trait_lib)), need).map_err(wrap)?;
    let declared = r.keys.member(trait_, MemberKind::Const, name).map_err(|clash| ConstFail::QPathKey { need, clash })?;
    let Some(head) = r.store.fns.get(&declared) else { return Err(ConstFail::NoTraitConst { trait_, constant: declared }) };
    let ty = r.store.tys.view().node(head.output).ok().filter(|n| prim_name(*n).is_some()).ok_or(ConstFail::QPathType { constant: declared })?;
    let (shape, self_node) = match self_ {
        QSelf::Prim(_, node) => {
            let shape = match node {
                PNode::Bool => Shape::Bool,
                PNode::Char => Shape::Char,
                PNode::Int(t) => Shape::Int(t),
                PNode::Uint(t) => Shape::Uint(t),
                _ => return Err(ConstFail::QPathType { constant: declared }),
            };
            (shape, node)
        }
        QSelf::Adt(lib) => {
            let adt = r.keys.item(lib).map_err(|clash| ConstFail::QPathKey { need, clash })?;
            (Shape::Adt(adt), PNode::Adt(adt, tc::List::EMPTY))
        }
    };
    // The impl candidates of (trait, type): the body check's own fill (check.rs `make_once`,
    // `Fill::Impls`), so the impls `impl_const` scans are the ones a body's goal would see.
    // A constant's value is evaluated at ground arguments: the whole program's impls.
    make_for(r, Fill::Impls(trait_, shape, super::buckets::Seen::Program), need).map_err(wrap)?;
    let Some((consts, prepared)) = r.consts.take() else { return Err(ConstFail::Nested { item: need, shown: super::check::key_shown(r, need) }) };
    let got = consts.trait_const_value(r, prepared, declared, self_node).map(int_of);
    r.consts = Some((consts, prepared));
    let bits = got?.ok_or(ConstFail::QPathType { constant: declared })?;
    Ok((bits, ty))
}
