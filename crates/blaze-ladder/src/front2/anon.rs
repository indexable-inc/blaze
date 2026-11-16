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
//! - an expression that mentions a generic parameter in scope, or `Self` ([`mentions`]): it has
//!   no value at the head (`[u8; N + 1]`, `size_of::<Self>()`); blaze-typeck's type form has
//!   `ConstParam` and `ConstValue` only, no node for an unevaluated expression;
//! - a type that is not `bool`, `char` or an integer ([`prim_name`]): the item text spells its
//!   type, and only those are spelled the same in every module.
//! OWED: the scope is the MODULE's, so an expression naming an item declared in a block is
//! resolved at the module (as a plain constant path already is, types.rs `const_value_bits`).

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
    /// written name ([`prim_name`]) or the text is empty.
    pub fn of(module: LibKey, ty: PNode, text: &str) -> Option<Ask> {
        let text = text.trim();
        let prim = prim_name(ty)?;
        if text.is_empty() {
            return None;
        }
        let name = format!("{prim}: {text}").into_boxed_str();
        let need = need_key(anon_const_key(checker_of(module), &name));
        Some(Ask { need, module, ty, name, text: Box::from(text) })
    }
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
        if store.fns.contains_key(&key) {
            continue;
        }
        let prim = prim_name(a.ty).ok_or(FileFail::NoName(a.ty))?;
        let text: &'n str = store.locals.keep_text(&format!("const _: {prim} = {};", a.text));
        let output = intern(&mut store.tys, a.ty).map_err(FileFail::Type)?;
        let header = tc::FnPtrHeader { abi: tc::Abi::Rust, safety: tc::Safety::Safe };
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
                inputs: Vec::new_in(Global),
                output,
                has_body: true,
                module: a.module,
                params: Vec::new_in(Global),
                consts: Vec::new_in(Global),
                bounds: Vec::new_in(Global),
                self_: SelfTy::None,
                self_assoc: Vec::new_in(Global),
            },
        );
    }
    Ok(needs)
}
