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
//! Its key is a function of (the module, the type, the expression's text): computed from text
//! before the work (rule 65), with no position and no ordinal, so two heads of one module that
//! write `1 << 0` at `usize` share one record. Its NEED key is that checker key widened to a
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
use blaze_typeck as tc;
use blaze_typeck::{ItemKey, Name, PNode};

use super::heads::{FnHead, Store};
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
}

/// Why an asked anonymous constant could not be filed.
#[derive(Debug)]
pub enum FileFail {
    /// Its key is another item's (rule 21).
    Clash(KeyClash),
    /// Its type could not be put in the records' table.
    Type(Why),
    /// Its type has no name to write (never: [`Ask::of`] refuses such a type).
    NoName(PNode),
}

impl core::fmt::Display for FileFail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FileFail::Clash(c) => write!(f, "an anonymous constant's key: {c}"),
            FileFail::Type(why) => write!(f, "an anonymous constant's type: {why:?}"),
            FileFail::NoName(n) => write!(f, "an anonymous constant of the type {n:?}, which has no name to write"),
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
    let first = key.0.to_be_bytes();
    let rest = ContentId::of("blaze-ladder front2 anonymous constant need 1", &first);
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
        Some(Ask { need, module, ty, name, text: Box::from(text), self_ty: None, body: None })
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
        let need = need_key(anon_const_key(checker_of(module), &name));
        Some(Ask { need, module, ty, name, text: Box::from(text), self_ty: self_.map(|(t, _)| t), body: Some((owner, scope)) })
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
        Some(Ask { need, module, ty, name, text: Box::from(text), self_ty: Some(self_ty), body: None })
    }
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

/// The value of the body's anonymous constant `ask` ([`Ask::in_body`]), read as
/// [`self_value`] reads one: its bits once evaluated, the evaluator's refusal by the
/// expression's name, [`Why::NeedConst`] naming it until the driver has filed and evaluated it.
pub fn body_value<H: super::types::Heads + ?Sized>(heads: &H, ask: Ask, r: blaze_surface::Range) -> Result<u128, Why> {
    if let Some(bits) = heads.const_bits(ask.need) {
        return Ok(bits);
    }
    if let Some(why) = heads.anon_failed(ask.need) {
        return Err(Why::ConstPath(format!("{}: {why}", ask.text).into_boxed_str(), r));
    }
    let need = ask.need;
    let text = ask.text.clone();
    if heads.ask_anon(ask) { Err(Why::NeedConst(need, r)) } else { Err(Why::ConstPath(text, r)) }
}

/// Files every anonymous constant the type builder asked for since the last call
/// (`Maps::anon_asked`) as a constant item's head in `store.fns`, its key in `keys`: after
/// this, `Consts::value(checker_of(ask.need))` lowers, checks and evaluates it. One already
/// filed is left as it is (its key says its type and text). Returns the need key of each, in
/// asking order: the caller evaluates them in one round (an enum of sixty-four written
/// discriminants, core's `AlignmentEnum`, is then one round, not sixty-four).
pub fn file_asked<'n>(store: &mut Store<'n>, keys: &mut Keys) -> Result<Vec<LibKey, Global>, FileFail> {
    let asked = store.maps.anon_asked.take();
    let mut needs = Vec::with_capacity_in(asked.len(), Global);
    for a in asked.iter() {
        needs.push(a.need);
        let module = keys.item(a.module).map_err(FileFail::Clash)?;
        let key = keys.member(module, MemberKind::Anon, &a.name).map_err(FileFail::Clash)?;
        // A body's constant reads its names in the block it is written in first.
        if let Some((owner, scope)) = a.body {
            store.locals.file_anon(key, owner, scope);
        }
        if store.fns.contains_key(&key) {
            continue;
        }
        let prim = written_ty(a.ty, &a.text).ok_or(FileFail::NoName(a.ty))?;
        let text: &'n str = store.locals.keep_text(&format!("const _: {prim} = {};", a.text));
        let output = intern(&mut store.tys, a.ty).map_err(FileFail::Type)?;
        let header = tc::FnPtrHeader::RUST;
        store.preds.insert(key, Preds::new());
        store.fns.insert(
            key,
            FnHead {
                lib: a.module,
                text,
                container: None,
                parent: 0,
                own: 0,
                declared: 0,
                self_param: false,
                header,
                c_variadic: false,
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
