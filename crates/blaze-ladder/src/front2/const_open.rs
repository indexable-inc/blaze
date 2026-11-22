//! Constants in types: the three states a constant written in a type can be in where the
//! front end builds a signature or converts a body (docs/v2/plan/design/typeck.md, "Constants
//! in types: three states").
//!
//! 1. A VALUE: known bits (`PNode::ConstValue`). A literal, a constant item's value, an
//!    anonymous constant expression over nothing generic ([`super::anon`]), and, here, the two
//!    forms measured on the pinned library that were refused as if they were open:
//!    - an expression that names `Self` where `Self` is a GROUND type ([`ground_self`]). The
//!      pinned core, library/core/src/num/uint_macros.rs, inside `impl u16 { .. }`: "pub const
//!      fn from_le_bytes(bytes: [u8; size_of::<Self>()]) -> Self". `Self` is `u16` there, so
//!      the length is 2 at the head; the anonymous constant is filed with that `Self` in
//!      scope ([`super::anon::Ask::of_self`]) and evaluated as any other;
//!    - an array repeat length that names a `const` item declared in the body
//!      ([`local_const`]): the Reference, "Statements", "Item declarations": "Declaring an
//!      item within a statement block restricts its scope to the block containing the
//!      statement."
//! 2. A PARAMETER: a const generic parameter in scope, alone (`PNode::ConstParam`, made by
//!    `Builder::const_term`). The Reference, "Generic parameters", "Const generics": "Const
//!    parameters can be used anywhere a const item can be used, with the exception that when
//!    used in a type or array repeat expression, it must be standalone".
//! 3. OPEN: an expression over a generic parameter in scope, or over a `Self` that is not
//!    ground (`[u8; N + 1]`, `[u8; size_of::<T>()]`). It has no value until the item is
//!    instantiated. The Reference, "Generic parameters", "Const generics": "As a further
//!    restriction, const parameters may only appear as a standalone argument inside of a type
//!    or array repeat expression. In those contexts, they may only be used as a single
//!    segment path expression, possibly inside a block (such as `N` or `{N}`). That is, they
//!    cannot be combined with other expressions." Its example: "fn bad_function<const N:
//!    usize>() -> [u8; {N + 1}]" is "Not allowed". So an open constant is in no program the
//!    language accepts without the unstable feature `generic_const_exprs`, which the pinned
//!    library does not turn on. It stays a typed refusal naming the constant by its range
//!    (`Why::Unsupported("a constant expression over a generic parameter", range)`: a head's
//!    refusal, a body's `ConstGapWhy::OverGeneric` gap), never a value and never a hole that
//!    passes clean.
//!
//! WHAT CARRYING STATE THREE WOULD NEED (not built: no accepted program needs it yet). The
//! checker's own terms already hold it (blaze-ty `ConstKind::Unevaluated(def, args)`, the
//! solver's `Alias(_, Const, ..)`); the PORTABLE form does not (blaze-typeck `PNode` has
//! `ConstValue` and `ConstParam` only), and a new `PNode` variant is matched exhaustively in
//! other crates, so it is one cross-crate batch: `PNode::ConstExpr { item, args }` (the
//! anonymous constant's key and the enclosing item's generic arguments; equal when both are
//! equal), lowered to `const_unevaluated`, written back, and normalized by asking the world
//! for the value once `args` is ground. The front end's half exists: an anonymous constant is
//! a filed head with a key from text ([`super::anon`]), [`super::anon::Ask::of_self`] shows a
//! head filed with part of its enclosing scope, and `Consts::value_at` evaluates a root at
//! ground arguments.
//!
//! Also here: what the constant evaluator's world needs made when `<T as Trait>::NAME` is
//! reached at a ground `T` inside an evaluation ([`impl_rows`]: `size_of::<usize>()` is
//! `<usize as SizedTypeProperties>::SIZE` in the pinned core).

use blaze_id::ItemKey as LibKey;
use blaze_lower::View;
use blaze_lower::form as lf;
use blaze_surface::Range;
use blaze_typeck::{ExprId, ItemKey, Name, PNode, PTy, PTys};

use super::body::BodyError;
use super::check::{CheckFail, Records};
use super::consts::ConstFail;
use super::heads::{HeadError, Want};
use super::locals::{LocalKind, LocalNs, Locals};
use super::source::NamesSource;
use super::types::{Env, Heads, SelfTy, Why};

/// The most constants one evaluation may have evaluated FIRST (`Consts::value_at`'s work
/// list: each is a constant a head named while a body of the evaluation was checked).
pub(super) const MOST_FIRSTS: u32 = 64;

/// The constant that must have a value before the evaluation that failed with `e` can go on:
/// a head made while one of its bodies was checked names it (`[u8; size_of::<Self>()]` on a
/// method of `u8`, met when `u8::MAX` is checked: the fill's `eval_len` finds the evaluator
/// busy, [`ConstFail::Nested`]), or one of its bodies' conversions does (`[0; LEN]`). `None`
/// for every other failure. `Consts::value_at` evaluates it and then the failed constant
/// again, by a work list: never a nested evaluation, and a constant that needs itself is the
/// refusal it failed with.
pub(super) fn needed_first(e: &ConstFail) -> Option<LibKey> {
    match e {
        ConstFail::Check { error: CheckFail::Const { key, error, .. }, .. } if matches!(**error, ConstFail::Nested { .. }) => Some(*key),
        ConstFail::Body { error: BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }, .. } => Some(*c),
        ConstFail::Body { error: BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }, .. } => Some(*c),
        _ => None,
    }
}

/// Makes the head of the constant `item` when it is declared in a body and has none yet: a
/// body's item has no detail in the names library, so the check of its own body cannot fill
/// its signature from there (the enclosing body's check makes it when a path names it:
/// world_paths.rs, `Fill::Store(Want::Fn(..))`; an array repeat length is no path of the
/// checked body). Any other item: nothing to do.
pub(super) fn local_head<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, item: ItemKey) -> Result<(), ConstFail> {
    let Some(lib) = r.store.locals.head(item).map(|h| h.lib) else { return Ok(()) };
    if r.store.fns.contains_key(&item) {
        return Ok(());
    }
    r.store.ensure(r.names, &mut *r.keys, Want::Fn(lib)).map_err(|error| ConstFail::Body { item, error: BodyError::Head { item: lib, error } })
}

/// Whether `text` names one of `params` (the generic parameters in scope) as an identifier.
/// A number's digits and suffix (`1usize`) are one token, never an identifier; `Self` is not
/// a parameter here ([`ground_self`] decides it).
fn names_param(text: &str, params: &[(Name, u32)]) -> bool {
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
        // `word` splits only at ASCII bytes, so both ends are char boundaries; a text that
        // cannot be cut there is taken as naming a parameter (the careful answer: no value).
        let Some(ident) = text.get(start..i) else { return true };
        if params.iter().any(|(n, _)| *n == Name::of(ident)) {
            return true;
        }
    }
    false
}

/// `Self`'s type and node when the constant expression `text` names `Self`, names no generic
/// parameter in scope, and `Self` is GROUND and nameable by a key from text: a built-in type
/// (`impl u16 { .. size_of::<Self>() .. }`) or an ADT with no generic arguments. Then the
/// expression has a value at the head (state one). `None` for every other expression: one
/// that does not name `Self` (the module's anonymous constant, as before), and an OPEN one
/// (a parameter named; `Self` in a trait, where it is a parameter; a `Self` with generic
/// arguments, whose identity is not one key), which the caller refuses by name.
pub(super) fn ground_self(tys: PTys<'_>, env: &Env<'_>, text: &str) -> Option<(PTy, PNode)> {
    if !super::anon::mentions(text, &[]) || names_param(text, env.params) {
        return None;
    }
    let SelfTy::Ty(t, _) = env.self_ else { return None };
    let node = tys.get(t).copied()?;
    match node {
        PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str => Some((t, node)),
        PNode::Adt(_, args) if args.len == 0 => Some((t, node)),
        _ => None,
    }
}

/// The most parentheses and braces around a repeat length's name that are looked through.
const MOST_WRAPS: u32 = 8;

/// The value of the array repeat length `len` of the body `owner` (its text `text`, at
/// `range`) when it is ONE name that a `const` item declared in the body takes
/// ([`Locals::find`], from the block the name is written in outwards: a block's item shadows
/// the module's and the fn's generic parameters, the Reference, "Names", "Scopes"): the
/// constant's bits once evaluated, [`Why::NeedConst`] naming its key until then (the driver
/// evaluates it and converts the body again, as for a module's constant). `Ok(None)`: no item
/// of the body has the name, and the caller reads it as before (a const parameter, the
/// module's names). A body's item of that name that is no constant is refused by name, never
/// looked up at the module, where the name would mean another item.
pub(super) fn local_const<H: Heads + ?Sized>(locals: &Locals<'_>, owner: ItemKey, heads: &H, view: &View<'_>, len: ExprId, text: &str, range: Range) -> Result<Option<u128>, Why> {
    let mut name = text.trim();
    if let Some(inner) = name.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
        name = inner.trim();
    }
    let ident = name.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && name.chars().all(|c| c.is_alphanumeric() || c == '_');
    if !ident {
        return Ok(None);
    }
    // The name's path, through the parentheses and the block a written `{ NAME }` is.
    let mut at = len;
    let mut scope = None;
    for _ in 0..MOST_WRAPS {
        match view.expr(at)? {
            lf::Expr::Path(p) => {
                scope = view.path(p)?.scope;
                break;
            }
            lf::Expr::Paren(inner) => at = inner,
            lf::Expr::Block { stmts, tail: Some(tail), .. } if stmts.len == 0 => at = tail,
            _ => return Ok(None),
        }
    }
    let Some(scope) = scope else { return Ok(None) };
    let Some(row) = locals.find(owner, Some(scope.0), name, LocalNs::Value) else { return Ok(None) };
    match row.kind {
        LocalKind::Const => match heads.const_bits(row.key) {
            Some(bits) if bits > u128::from(u64::MAX) => Err(Why::Unsupported("an array repeat length past `usize` (a constant declared in the body)", range)),
            Some(bits) => Ok(Some(bits)),
            None => Err(Why::NeedConst(row.key, range)),
        },
        LocalKind::Use => Err(Why::Unsupported("an array repeat length named through a `use` declared in the body", range)),
        _ => Err(Why::Unsupported("an array repeat length naming an item of the body that is not a constant", range)),
    }
}

/// What a generic argument written as ONE plain name means when an item declared in the body
/// takes the name ([`Locals::find`], from the block the name is written in outwards).
pub(super) enum LocalArg {
    /// A type the body declares: the argument is a type (the Reference, "Generic parameters",
    /// "Const generics": "When there is ambiguity if a generic argument could be resolved as
    /// either a type or const argument, it is always resolved as a type.").
    Type,
    /// A `const` the body declares, by its evaluated bits (`foo::<CHUNK_SIZE>()` with `const
    /// CHUNK_SIZE: usize = ..;` in the body: the pinned core's ptr::swap_nonoverlapping_bytes).
    Const(u128),
}

/// The local item that the plain-name generic argument `t` (its text `name`, at `range`) of
/// the body `owner` names. The type namespace is asked first: [`LocalArg::Type`] when a block
/// around the argument declares a type of that name. Otherwise [`LocalArg::Const`] when one
/// declares a `const`, once that constant is evaluated, and [`Why::NeedConst`] naming its key
/// until then: the driver evaluates it and converts the body again, as [`local_const`] does.
/// `Ok(None)`: no item of the body takes the name, and the caller reads the module's names.
/// A local value item that is no `const` is refused by name, never looked up at the module,
/// where the name would mean another item.
pub(super) fn local_arg<H: Heads + ?Sized>(locals: &Locals<'_>, owner: ItemKey, heads: &H, view: &View<'_>, t: lf::TypeId, name: &str, range: Range) -> Result<Option<LocalArg>, Why> {
    let lf::TypeRef::Path(p) = view.ty(t)? else { return Ok(None) };
    let Some(scope) = view.path(p)?.scope else { return Ok(None) };
    if locals.find(owner, Some(scope.0), name, LocalNs::Type).is_some() {
        return Ok(Some(LocalArg::Type));
    }
    let Some(row) = locals.find(owner, Some(scope.0), name, LocalNs::Value) else { return Ok(None) };
    match row.kind {
        LocalKind::Const => match heads.const_bits(row.key) {
            Some(bits) => Ok(Some(LocalArg::Const(bits))),
            None => Err(Why::NeedConst(row.key, range)),
        },
        LocalKind::Use => Err(Why::Unsupported("a const argument named through a `use` declared in the body", range)),
        _ => Err(Why::Unsupported("a const argument naming an item of the body that is not a constant", range)),
    }
}
