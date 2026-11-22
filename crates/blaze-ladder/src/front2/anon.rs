//! Anonymous constants of heads: a constant EXPRESSION written where a head needs a value (an
//! array length `[u8; A + 1]`, an enum discriminant `_Align1Shl0 = 1 << 0`, a const argument
//! `Buf<{ CAP * 2 }>`, a pattern type's range end `u32 is 0..u32::MAX`). The Reference, "Constant
//! evaluation", "Const context": "Array type length expressions", "Array repeat length
//! expressions", "Enumeration discriminants", "Const generic arguments" are const contexts,
//! and their values are computed as a constant item's is.
//!
//! So one is MADE a constant item: the text `const _: TY = EXPR;` in the module the head is
//! written in, with a head in `Store::fns` (no inputs, output `TY`, a body), lowered by
//! `body::lower_member`, checked by `check::check_body` and evaluated by the same machine as
//! any constant item's body (`consts::Consts::value`). Nothing here evaluates.
//!
//! Its key includes the module, type and expression text. A body constant also includes its
//! enclosing item and block when it uses their names, and a marked expression includes the
//! positions and identities of its token marks. Two unmarked module heads that write
//! `1 << 0` at `usize` share one record. Its NEED key is that checker key widened to a
//! library key ([`need_key`]): `Why::NeedConst` carries it through every driver loop that
//! already evaluates a constant item a head names (`check::eval_len`), unchanged.
//!
//! What is NOT taken (each stays the head's refusal, by name):
//! - an expression that mentions a generic parameter in scope, or a `Self` that is not a
//!   ground type ([`mentions`]): it has no value at the head (`[u8; N + 1]`,
//!   `size_of::<Self>()` in `impl<T> G<T>`); blaze-typeck's type form has `ConstParam` and
//!   `ConstValue` only, no node for an unevaluated expression. An expression over a GROUND
//!   `Self` (`size_of::<Self>()` in `impl u16`) IS taken, filed with that `Self` in scope
//!   ([`Ask::of_self`]; front2/const_open.rs, where the three states are told apart);
//! - a type that is not `bool`, `char` or an integer ([`prim_name`]): the item text spells its
//!   type, and only those are spelled the same in every module. A type the caller does not
//!   know is spelled `_` when the expression's type is its own ([`written_ty`]), and refused
//!   otherwise (an unsuffixed number would take a type the context never gave it).
//! A BODY's expression that names an item declared in the body (`[0; 2 + SEP_LEN]`) is filed
//! under the body and the block it is written in ([`Ask::in_body`]), its names read there
//! first; every other expression is the module's. OWED: a block's glob `use` and a block's
//! `use .. as _` trait are not seen from such a constant's body (front2/glob_uses.rs and
//! front2/block_traits.rs walk local items' heads only).

use std::alloc::Global;
use std::boxed::Box;
use std::format;
use std::vec::Vec;

use blaze_id::ItemKey as LibKey;
use blaze_ids::ContentId;
use blaze_lower::mark_site::TextMark;
use blaze_own::{AString, AVec, Alloc};
use blaze_scope::crate_roots::TextMarks;
use blaze_typeck as tc;
use blaze_typeck::{ItemKey, Name, PNode};

use super::heads::{FnHead, FnQualifiers, Store};
use super::keys::{KeyClash, Keys, MemberKind, anon_const_key, checker_of};
use super::preds::Preds;
use super::types::{PRIMS, SelfTy, Why, intern};

/// One anonymous constant a head asked for: filed as a head by [`file_asked`] before the
/// evaluator is asked its value.
#[derive(Clone, Debug)]
pub struct Ask {
    /// What `Why::NeedConst` and `Maps::const_bits` are keyed by ([`need_key`]).
    pub need: LibKey,
    /// The module the head is written in: the scope its names are read in.
    pub module: LibKey,
    /// Its type: `bool`, `char` or an integer.
    pub ty: PNode,
    /// Its member name under the module: the type's name, then the text.
    pub name: Box<str, Global>,
    /// The expression as written, trimmed.
    pub text: Box<str, Global>,
    /// What `Self` is in the expression, for one that names it where `Self` is a ground type
    /// ([`Ask::of_self`]): a type of the store's table (the enclosing head's own self type,
    /// made before the ask). `None`: the expression names no `Self`.
    pub self_ty: Option<tc::PTy>,
    /// The body and the block (its `ScopeId`) the expression is written in, for one that names
    /// an item declared in the body ([`Ask::in_body`]): its names are read there first, then
    /// at the module. `None`: a constant of the module.
    pub body: Option<(ItemKey, u32)>,
    /// Marks are relative to the expression, independent of its position in the enclosing text.
    marked: Option<Marked>,
}

#[derive(Clone, Debug)]
struct Marked {
    member: AString<'static>,
    rows: AVec<'static, (u32, TextMark)>,
}

/// The synthetic members whose expressions contain marked tokens. Both the synthetic text
/// and the opened column live in the command frame.
pub(super) type Columns<'n> = blaze_hash::HashMap<ItemKey, TextMarks<'n>, &'n blaze_mem::Frame>;

/// Why an asked anonymous constant could not be filed.
#[derive(Debug)]
pub enum FileFail {
    /// Its key is another item's (rule 21).
    Clash(KeyClash),
    /// Its type could not be put in the records' table.
    Type(Why),
    /// Its type has no name to write (never: [`Ask::of`] refuses such a type).
    NoName(PNode),
    Marks { item: ItemKey, error: blaze_expand::origin_column::ColumnError },
}

impl core::fmt::Display for FileFail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FileFail::Clash(c) => write!(f, "an anonymous constant's key: {c}"),
            FileFail::Type(why) => write!(f, "an anonymous constant's type: {why:?}"),
            FileFail::NoName(n) => write!(f, "an anonymous constant of the type {n:?}, which has no name to write"),
            FileFail::Marks { item, error } => write!(f, "origin marks of anonymous constant {item:?}: {error}"),
        }
    }
}

/// The written name of `ty` if it is `bool`, `char` or an integer type.
pub fn prim_name(ty: PNode) -> Option<&'static str> {
    if !matches!(ty, PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_)) {
        return None;
    }
    PRIMS.iter().find(|(_, n)| *n == ty).map(|&(name, _)| name)
}

/// The type an anonymous constant of type `ty` with the expression `text` is written with:
/// the name of `bool`, `char` or an integer type ([`prim_name`]), or `_` for a type the
/// caller does not know (`PNode::Infer`: a const argument whose parameter is not known where
/// a body is converted, alloy-sol-types' `<U96>::from_be_bytes::<{ U96::BYTES }>(..)`) when
/// the expression's type is its own ([`self_typed`]): the checker infers it from the
/// expression alone, as for `let n = U96::BYTES;`, and the value is in that type's width.
/// `None` for every other type, and for an unknown type of an expression that is not
/// self-typed (`1 << 3` would be an `i32` without the parameter's type: a wrong width).
pub fn written_ty(ty: PNode, text: &str) -> Option<&'static str> {
    match ty {
        PNode::Infer if self_typed(text) => Some("_"),
        _ => prim_name(ty),
    }
}

/// The integer suffixes (the Reference, "Literal expressions", "Integer literal
/// expressions").
const INT_SUFFIXES: [&str; 12] = ["u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize"];

/// Whether the constant expression `text` has a type that is a function of the expression
/// alone: it holds no number without an integer suffix and no `_` as a word. The Reference,
/// "Literal expressions", "Integer literal expressions": "If the token has no suffix, the
/// expression's type is determined by type inference", from the type the context expects; an
/// anonymous constant with no expected type would make it `i32`. A `_` (`as _`, `::<_>`) asks
/// the same of the context. Every other part of an expression (a path to a constant, a call,
/// a method call, an operator over such parts) has its type from what its names resolve to.
/// Careful by construction: a tuple index (`x.0`) or a number inside a string literal is
/// taken for an unsuffixed number, and the expression is not self-typed.
pub fn self_typed(text: &str) -> bool {
    let bytes = text.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80;
    let mut i = 0;
    while i < bytes.len() {
        if !word(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && word(bytes[i]) {
            i += 1;
        }
        // `word` splits only at ASCII bytes, so both ends are char boundaries.
        let Some(w) = text.get(start..i) else { return false };
        if w == "_" {
            return false;
        }
        if bytes[start].is_ascii_digit() && !INT_SUFFIXES.iter().any(|s| w.len() > s.len() && w.ends_with(s)) {
            return false;
        }
    }
    true
}

/// Whether `text` names `Self` or one of `params` (the generic parameters in scope) as an
/// identifier: such an expression has no value at the head. A number's suffix (`1usize`) and
/// its digits are one token, never an identifier.
pub fn mentions(text: &str, params: &[(Name, u32)]) -> bool {
    let bytes = text.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80;
    let mut i = 0;
    while i < bytes.len() {
        if !word(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && word(bytes[i]) {
            i += 1;
        }
        if bytes[start].is_ascii_digit() {
            continue;
        }
        // `word` splits only at ASCII bytes, so both ends are char boundaries.
        let Some(ident) = text.get(start..i) else { return true };
        if ident == "Self" || params.iter().any(|(n, _)| *n == Name::of(ident)) {
            return true;
        }
    }
    false
}

/// The need key of the anonymous constant whose checker key is `key`: its first 16 bytes are
/// the checker key (so `checker_of` gives it back and the names library finds its crate by
/// the prefix), the rest a hash under this module's own domain, so no library item has it.
pub fn need_key(key: ItemKey) -> LibKey {
    request_key(key, "blaze-ladder front2 anonymous constant need 1")
}

// Head discovery may probe a candidate that the checker never selects. Its
// evaluator files a refusal for that candidate. A lexical body's request is
// mandatory: eval_len and Consts::first must return its typed error to the
// requesting body. Both request classes project to the same checker key.
fn body_need_key(key: ItemKey) -> LibKey {
    request_key(key, "blaze-ladder front2 required body constant 1")
}

fn request_key(key: ItemKey, domain: &'static str) -> LibKey {
    let first = key.0.to_be_bytes();
    let rest = ContentId::of(domain, &first);
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(&first);
    bytes[16..].copy_from_slice(&rest.0[..16]);
    blaze_id::ItemKey(ContentId(bytes))
}

impl Ask {
    /// The anonymous constant `text` of type `ty` written in `module`; `None` when `ty` has no
    /// written name ([`written_ty`]) or the text is empty.
    pub fn of(module: LibKey, ty: PNode, text: &str) -> Option<Ask> {
        let text = text.trim();
        let prim = written_ty(ty, text)?;
        if text.is_empty() {
            return None;
        }
        let name = format!("{prim}: {text}").into_boxed_str();
        let need = need_key(anon_const_key(checker_of(module), &name));
        Some(Ask { need, module, ty, name, text: Box::from(text), self_ty: None, body: None, marked: None })
    }

    /// The anonymous constant `text` of type `ty` written in block `scope` of the body `owner`
    /// (in `module`), for an expression that names an item declared in the body (alloy's
    /// `FixedBytes` `Display`: "const SEP_LEN: usize = '…'.len_utf8(); let mut buf = [0; 2 + 4 +
    /// SEP_LEN + 4];"). The Reference, "Statements", "Item declarations": "Declaring an item
    /// within a statement block restricts its scope to the block containing the statement",
    /// so the constant's names are read in that block and the blocks around it before the
    /// module ([`super::locals::Locals::find`], through `Locals::file_anon`). Its key is a
    /// function of (the module, the type, the text, the body, the block): the block's id is
    /// an ordinal of the body's lowering, a function of the body's text alone, as a local
    /// item's key is. `self_` is `Self` where the expression names it and `Self` is ground
    /// (front2/const_open.rs `ground_self`; der's `impl DecodeValue for u128`: "let mut buf =
    /// [0u8; (Self::BITS as usize / 8) + UNSIGNED_HEADROOM];" beside a body's `const
    /// UNSIGNED_HEADROOM`): the head is filed with both the block and that `Self` in scope,
    /// and the key says what `Self` is as [`Ask::of_self`]'s does. `None` as [`Ask::of`], or
    /// when `self_`'s node is neither a built-in type nor an ADT with no arguments.
    pub fn in_body(module: LibKey, ty: PNode, text: &str, owner: ItemKey, scope: u32, self_: Option<(tc::PTy, PNode)>) -> Option<Ask> {
        let text = text.trim();
        let prim = written_ty(ty, text)?;
        if text.is_empty() {
            return None;
        }
        let name = match self_ {
            None => format!("{prim}: {text} in the body {:x} at block {scope}", owner.0),
            Some((_, PNode::Adt(adt, args))) if args.len == 0 => format!("{prim}: {text} in the body {:x} at block {scope} where Self is the type {:x}", owner.0, adt.0),
            Some((_, other)) => format!("{prim}: {text} in the body {:x} at block {scope} where Self is {}", owner.0, PRIMS.iter().find(|(_, n)| *n == other)?.0),
        }
        .into_boxed_str();
        let need = body_need_key(anon_const_key(checker_of(module), &name));
        Some(Ask { need, module, ty, name, text: Box::from(text), self_ty: self_.map(|(t, _)| t), body: Some((owner, scope)), marked: None })
    }

    /// The anonymous constant `text` of type `ty` written in `module` where `Self` is the
    /// ground type `self_ty` (its node `self_node`: a built-in type, or an ADT with no generic
    /// arguments: front2/const_open.rs `ground_self`). The pinned core, library/core/src/num/
    /// uint_macros.rs, in `impl u16`: "pub const fn from_le_bytes(bytes: [u8;
    /// size_of::<Self>()]) -> Self". Its head is filed with that `Self` in scope
    /// ([`file_asked`]), so the expression is lowered and checked as written. Its key is a
    /// function of (the module, the type, the text, WHAT `Self` IS): `Self` by its built-in
    /// name or its ADT's key (a key from text), so `size_of::<Self>()` of `impl u16` and of
    /// `impl u32` in one module are two records (rule 21), and one head asked from two
    /// places is one. `None` as [`Ask::of`], or when `self_node` is neither.
    pub fn of_self(module: LibKey, ty: PNode, text: &str, self_ty: tc::PTy, self_node: PNode) -> Option<Ask> {
        let text = text.trim();
        let prim = written_ty(ty, text)?;
        if text.is_empty() {
            return None;
        }
        let name = match self_node {
            PNode::Adt(adt, args) if args.len == 0 => format!("{prim}: {text} where Self is the type {:x}", adt.0),
            other => format!("{prim}: {text} where Self is {}", PRIMS.iter().find(|(_, n)| *n == other)?.0),
        }
        .into_boxed_str();
        let need = need_key(anon_const_key(checker_of(module), &name));
        Some(Ask { need, module, ty, name, text: Box::from(text), self_ty: Some(self_ty), body: None, marked: None })
    }

    pub(super) fn carry_marks(mut self, rows: AVec<'static, (u32, TextMark)>) -> Self {
        if !rows.is_empty() {
            let mut hash = blaze_ids::Hasher::new("blaze-ladder marked anonymous constant 1");
            hash.part(self.name.as_bytes());
            for &(offset, mark) in &rows {
                hash.update(&offset.to_le_bytes()).update(&mark.to_bytes());
            }
            let member = blaze_own::format_in!(*rows.allocator(), "marked {:x}", hash.finish());
            let key = anon_const_key(checker_of(self.module), &member);
            self.need = if self.body.is_some() { body_need_key(key) } else { need_key(key) };
            self.marked = Some(Marked { member, rows });
        }
        self
    }

    fn member_name(&self) -> &str {
        self.marked.as_ref().map_or(&self.name, |m| m.member.as_str())
    }
}

/// Move the expression's marks past the synthetic declaration prefix before passing the
/// text to the ordinary marked-item lowering.
fn file_marks<'n>(
    columns: &mut Columns<'n>,
    item: ItemKey,
    text: &'n str,
    prefix: usize,
    marked: &Marked,
    alloc: Alloc<'_>,
) -> Result<(), blaze_expand::origin_column::ColumnError> {
    use blaze_expand::origin_column::{ColumnError, OriginsRef};
    let mut rows = AVec::with_capacity_in(marked.rows.len(), alloc);
    for &(offset, mark) in &marked.rows {
        let at = prefix + offset as usize;
        rows.push((u32::try_from(at).map_err(|_| ColumnError::TooLarge { at })?, mark));
    }
    let bytes = blaze_lower::mark_site::column_of_copy(text, &rows, alloc)?;
    let frame = *columns.allocator();
    let (column, _) = OriginsRef::open(frame.alloc_slice_copy(&bytes))?;
    columns.insert(item, TextMarks { text, column });
    Ok(())
}

/// The value of the constant expression `text` of type `ty` that names a ground `Self`
/// ([`Ask::of_self`]), read exactly as `Builder::const_value_bits` reads an anonymous
/// constant of the module: its bits once evaluated (`Heads::const_bits`), the evaluator's
/// refusal by the expression's name once it has refused (`Heads::anon_failed`),
/// [`Why::NeedConst`] naming it until the driver's `eval_len` has filed and evaluated it.
/// An expression whose type is unknown or is not `bool`, `char` or an integer is
/// [`Why::ConstPath`] naming its text.
pub fn self_value<H: super::types::Heads + ?Sized>(heads: &H, module: LibKey, ty: Option<PNode>, text: &str, self_ty: tc::PTy, self_node: PNode, r: blaze_surface::Range) -> Result<u128, Why> {
    let text = text.trim();
    let Some(ask) = ty.and_then(|ty| Ask::of_self(module, ty, text, self_ty, self_node)) else { return Err(Why::ConstPath(Box::from(text), r)) };
    if let Some(bits) = heads.const_bits(ask.need) {
        return Ok(bits);
    }
    if let Some(why) = heads.anon_failed(ask.need) {
        return Err(Why::ConstPath(format!("{text}: {why}").into_boxed_str(), r));
    }
    let need = ask.need;
    if heads.ask_anon(ask) { Err(Why::NeedConst(need, r)) } else { Err(Why::ConstPath(Box::from(text), r)) }
}

/// The token marks of a HEAD's constant expression `source` (a slice of the text `marks` are
/// of: a head a macro wrote), each at its place in `asked`, the text the constant is asked
/// with: `source` itself, or `source` with every qualified constant leaf replaced by its
/// value ([`super::const_qpath::substitute`]; a leaf's own marks go with it, its value being a
/// literal). Rust's rule: a path in the expression resolves where ITS tokens' hygiene says
/// (the Reference, "Macros By Example", "Hygiene": `$crate` "refers to the crate defining the
/// macro"), so the constant made of it is lowered with them as a body's is
/// ([`Ask::carry_marks`]). zerocopy's `cryptocorrosion_derive_traits!` writes `(): PaddingFree<
/// Self, { $crate::cryptocorrosion_derive_traits!(@struct_padding_check ..) }>` into an impl
/// it expands in ppv-lite86; without them `crate` named ppv-lite86 (tempo t1500, rand_chacha
/// `refill_wide_impl`: "segment 0: not found"). `Ok(None)`: `source` is no slice of the marks'
/// text (a type macro's expansion lowered inside the head's view, or a text the inherent
/// leaves rewrote: `Builder::const_term`), whose marks this does not hold; the caller decides.
/// An `asked` that does not follow from `source` leaf by leaf is refused by name.
pub(super) fn head_marks(
    marks: blaze_scope::crate_roots::TextMarks<'_>,
    source: &str,
    asked: Option<&str>,
    alloc: Alloc<'static>,
    r: blaze_surface::Range,
) -> Result<Option<AVec<'static, (u32, TextMark)>>, Why> {
    let Ok(item) = blaze_lower::mark_site::ItemMarks::of(source, marks) else { return Ok(None) };
    let mut rows = AVec::new_in(alloc);
    let Some(asked) = asked else {
        for row in item.marked() {
            rows.push(row);
        }
        return Ok(Some(rows));
    };
    let off = |why: &'static str| Why::Program(format!("a marked constant expression `{source}` asked as `{asked}`: {why}").into_boxed_str(), r);
    let at = |i: usize| u32::try_from(i).map_err(|_| off("past 4 GiB"));
    let mut marked = item.marked().peekable();
    // Copies `source[from..to]`, found at `asked[out..]`, with its marks.
    let mut copy = |from: usize, to: usize, out: usize, rows: &mut AVec<'static, (u32, TextMark)>| -> Result<usize, Why> {
        let seg = source.get(from..to).ok_or_else(|| off("a leaf off a char boundary"))?;
        if !asked.get(out..).is_some_and(|rest| rest.starts_with(seg)) {
            return Err(off("the text between two leaves is not copied"));
        }
        while let Some(&(start, mark)) = marked.peek() {
            let s = start as usize;
            if s >= to {
                break;
            }
            marked.next();
            if s >= from {
                rows.push((at(s - from + out)?, mark));
            }
        }
        Ok(out + seg.len())
    };
    let (mut from, mut out) = (0usize, 0usize);
    for leaf in super::const_qpath::Leaves::of(source) {
        out = copy(from, leaf.start, out, &mut rows)?;
        out += literal_len(asked.get(out..).unwrap_or("")).ok_or_else(|| off("a leaf's value is no literal"))?;
        from = leaf.end;
    }
    out = copy(from, source.len(), out, &mut rows)?;
    if out != asked.len() {
        return Err(off("text past the last leaf"));
    }
    Ok(Some(rows))
}

/// Whether `text` has the word `crate` (a `crate::` path, or the `crate` an expansion wrote for
/// another crate's `$crate`): what a constant expression's missing marks would misread.
pub(super) fn names_crate(text: &str) -> bool {
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let b = text.as_bytes();
    let mut from = 0usize;
    while let Some(i) = text.get(from..).and_then(|t| t.find("crate")) {
        let (s, e) = (from + i, from + i + 5);
        if (s == 0 || !word(b[s - 1])) && b.get(e).is_none_or(|&c| !word(c)) {
            return true;
        }
        from = e;
    }
    false
}

/// The length of the literal `text` starts with, as [`super::const_qpath::push_literal`]
/// writes one: `true`/`false` or digits with a suffix (one word), `'\u{..}'`, or a negative
/// integer `(-N - 1T)`.
fn literal_len(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    match *b.first()? {
        b'(' => text.find(')').map(|i| i + 1),
        b'\'' => text.find("}'").map(|i| i + 2),
        _ => {
            let n = b.iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'_').count();
            (n > 0).then_some(n)
        }
    }
}

/// A body's anonymous constant is required. Return its bits or ask the driver
/// to evaluate it. A failure goes directly to the requesting body.
pub fn body_value<H: super::types::Heads + ?Sized>(heads: &H, ask: Ask, r: blaze_surface::Range) -> Result<u128, Why> {
    if let Some(bits) = heads.const_bits(ask.need) {
        return Ok(bits);
    }
    let need = ask.need;
    let text = ask.text.clone();
    if heads.ask_anon(ask) { Err(Why::NeedConst(need, r)) } else { Err(Why::ConstPath(text, r)) }
}

/// Files every anonymous constant the type builder asked for since the last call
/// (`Maps::anon_asked`) as a constant item's head in `store.fns`, its key in `keys`: after
/// this, `Consts::value(checker_of(ask.need))` lowers, checks and evaluates it. One already
/// filed is left as it is (its key says its type and text). Returns the head probes
/// eligible for batch evaluation, in asking order (e.g. enum discriminants).
/// A mandatory body need is evaluated by the driver that requested it.
pub fn file_asked<'n>(store: &mut Store<'n>, keys: &mut Keys) -> Result<Vec<LibKey, Global>, FileFail> {
    let asked = store.maps.anon_asked.take();
    let mut needs = Vec::with_capacity_in(asked.len(), Global);
    for a in asked.iter() {
        if a.body.is_none() {
            needs.push(a.need);
        }
        let module = keys.item(a.module).map_err(FileFail::Clash)?;
        let key = keys.member(module, MemberKind::Anon, a.member_name()).map_err(FileFail::Clash)?;
        // A body's constant reads its names in the block it is written in first.
        if let Some((owner, scope)) = a.body {
            store.locals.file_anon(key, owner, scope);
        }
        if store.fns.contains_key(&key) {
            continue;
        }
        let prim = written_ty(a.ty, &a.text).ok_or(FileFail::NoName(a.ty))?;
        let alloc = store.tys.alloc();
        let mut declaration = blaze_own::format_in!(alloc, "const _: {prim} = ");
        let prefix = declaration.len();
        declaration.push_str(&a.text);
        declaration.push(';');
        let text = store.locals.keep_text(&declaration);
        if let Some(marked) = &a.marked {
            file_marks(&mut store.anon_marks, key, text, prefix, marked, alloc)
                .map_err(|error| FileFail::Marks { item: key, error })?;
        }
        let output = intern(&mut store.tys, a.ty).map_err(FileFail::Type)?;
        let header = tc::FnPtrHeader::RUST;
        // An array length or a const argument's constant: no clause, no lifetime in scope.
        store.preds.insert(key, Preds::new_in(super::heads_regions::Scope::new_in(alloc)));
        store.fns.insert(
            key,
            FnHead {
                lib: a.module,
                text,
                text_prefix: None,
                container: None,
                parent: 0,
                own: 0,
                declared: 0,
                self_param: false,
                header,
                c_variadic: false,
                qualifiers: FnQualifiers::CONST.note_lifetimes(text),
                legacy_const_generics: Vec::new_in(Global),
                inputs: Vec::new_in(Global),
                output,
                has_body: true,
                module: a.module,
                params: Vec::new_in(Global),
                consts: Vec::new_in(Global),
                bounds: Vec::new_in(Global),
                // `Self` for an expression that names it ([`Ask::of_self`]): the ground type
                // itself, with no trait (the expression is not a member of the impl).
                self_: match a.self_ty {
                    Some(t) => SelfTy::Ty(t, None),
                    None => SelfTy::None,
                },
                self_assoc: Vec::new_in(Global),
            },
        );
    }
    Ok(needs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_expand::origin_column::TextMark;
    use blaze_expand::{DefSite, Edition, Transparency};
    use blaze_id::CrateItemKey;

    #[test]
    fn anonymous_const_key_distinguishes_marked_roots_and_their_positions() {
        // A rendered foreign `$crate` is the word `crate` with an opaque root mark.
        // Either of these roots can be foreign, and the other can belong to the caller.
        let expr = "crate::A == crate::B";
        let ask = Ask::in_body(LibKey(ContentId([3; 32])), PNode::Bool, expr, ItemKey(7), 1, None).expect("a bool constant in block 1");
        let key = |offset: u32, defining_crate: u8| {
            let mut marks = blaze_own::AVec::new_in(blaze_own::testing::heap().alloc());
            marks.push((
                offset,
                TextMark {
                    site: DefSite { krate: CrateItemKey(ContentId([defining_crate; 32])), module: None },
                    transparency: Transparency::Opaque,
                    edition: Edition::E2021,
                },
            ));
            ask.clone().carry_marks(marks).need
        };
        let left = key(0, 4);
        assert_eq!(left, key(0, 4), "identical semantic inputs share the value");
        assert_ne!(left, ask.need, "the unmarked root selects the calling crate");
        assert_ne!(left, key(0, 5), "the left root selects a different defining crate");
        assert_ne!(left, key(expr.rfind("crate").expect("second root") as u32, 4), "the foreign root now qualifies B");
    }
}
