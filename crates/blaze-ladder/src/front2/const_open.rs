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
use blaze_scope::crate_roots::TextMarks;
use blaze_surface::Range;
use blaze_typeck::{ExprId, ItemKey, Name, PNode, PTy, PTys, PathIdx};

use super::body::BodyError;
use super::check::{CheckFail, Records};
use super::consts::ConstFail;
use super::heads::{HeadError, Want};
use super::locals::{LocalKind, LocalNs, Locals, Row};
use super::source::NamesSource;
use super::types::{Env, Heads, SelfTy, Why};
use crate::resolve::{Kind, Ns, Paths, ResolveError, Target};

/// The most constants one evaluation may have evaluated FIRST (`Consts::value_at`'s work
/// list: each is a constant a head named while a body of the evaluation was checked, e.g.
/// explicit enum discriminants such as `nix::errno::consts::Errno`).
pub(super) const MOST_FIRSTS: u32 = 1024;

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
        ConstFail::Body { error: BodyError::Const { item, error }, .. } if matches!(**error, ConstFail::Nested { .. }) => Some(*item),
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
    ground_self_in(tys, env.self_, env.params, text)
}

pub(super) fn ground_self_in(tys: PTys<'_>, self_: SelfTy, params: &[(Name, u32)], text: &str) -> Option<(PTy, PNode)> {
    if !super::anon::mentions(text, &[]) || names_param(text, params) {
        return None;
    }
    let SelfTy::Ty(t, _) = self_ else { return None };
    let node = tys.get(t).copied()?;
    match node {
        PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str => Some((t, node)),
        PNode::Adt(_, args) if args.len == 0 => Some((t, node)),
        _ => None,
    }
}

/// The most parentheses and braces around a repeat length's name that are looked through.
const MOST_WRAPS: u32 = 8;

/// Resolves a block-local `use` binding (`row.kind == LocalKind::Use`) in namespace `ns`,
/// following the same scoping rules as [`super::block_type_uses::through_use`] and
/// [`super::world_paths`]: refuses if a nearer block between `scope` and `row.scope` contains
/// a glob `use`, places the `use` at the written path's site (or `module` when no path is
/// given), redirects to a block-local item's module when `row.path[0]` names one
/// (`LocalKind::in_block_module`), and preserves `$crate` hygiene via `row.first_mark`.
///
/// Returns `Ok(Some(target))` when `row.path` resolves whole (`consumed == row.path.len()`),
/// `Ok(None)` when `row.path` does not resolve in `ns` or stops short.
#[expect(clippy::too_many_arguments, reason = "one lookup of a block-local use binding")]
fn resolve_use_target<'n, S: NamesSource<'n>>(
    resolver: &Paths<S>,
    marks: Option<TextMarks<'_>>,
    locals: &Locals<'_>,
    owner: ItemKey,
    module: LibKey,
    view: &View<'_>,
    scope: u32,
    path: Option<PathIdx>,
    name: &str,
    row: &Row<'_>,
    ns: Ns,
    range: Range,
) -> Result<Option<Target>, Why> {
    super::block_type_uses::glob_nearer(locals, owner, scope, row).map_err(|form| Why::Unsupported(form, range))?;
    let mut at_module = match path {
        Some(p) => super::item_site::written_site(resolver, marks, module, view, p, false, name, range)?.module,
        None => module,
    };
    if let Some(&head) = row.path.first()
        && !row.abs
        && let Some(item) = locals.find(owner, Some(row.scope), head, LocalNs::Type).filter(|i| i.kind.in_block_module())
    {
        at_module = item.module.ok_or(Why::Unsupported(item.kind.unplaced(), range))?;
    }
    let dollar = if row.abs { None } else { super::glob_uses::dollar_of(row.path.first().copied(), row.first_mark) };
    match resolver.path_with(at_module, dollar, row.abs, row.path, ns) {
        Ok(t) if t.consumed as usize == row.path.len() => Ok(Some(t)),
        Ok(_) | Err(ResolveError::Path(_)) => Ok(None),
        Err(e @ ResolveError::NoText { .. }) => Err(Why::Resolve(e, range)),
    }
}

/// Reads the evaluated bits of a `const` item `key` from `heads`, surfacing a recorded
/// evaluation failure by name or requesting evaluation via [`Why::NeedConst`].
fn const_item_bits<H: Heads + ?Sized>(heads: &H, key: LibKey, name: &str, range: Range) -> Result<u128, Why> {
    if let Some(bits) = heads.const_bits(key) {
        return Ok(bits);
    }
    if let Some(why) = heads.anon_failed(key) {
        return Err(Why::ConstPath(std::format!("{name}: {why}").into_boxed_str(), range));
    }
    Err(Why::NeedConst(key, range))
}

/// Whether `local_const_in_scope` is checking an array repeat length (`[x; N]`) or an array
/// type length (`[T; N]`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ArrayLenSite {
    Repeat,
    Type,
}

/// Resolves a single-identifier array length `name` in block `scope` of `owner` when a `const`
/// or `use` item declared in the body takes the name ([`Locals::find`], from `scope` outwards).
/// Shared by [`local_const`] (`[x; N]`) and `types::body_const_len` (`[T; N]`).
#[expect(clippy::too_many_arguments, reason = "one lookup of a body-local constant length")]
pub(super) fn local_const_in_scope<'n, H: Heads + ?Sized, S: NamesSource<'n>>(
    resolver: &Paths<S>,
    marks: Option<TextMarks<'_>>,
    locals: &Locals<'_>,
    owner: ItemKey,
    module: LibKey,
    heads: &H,
    view: &View<'_>,
    scope: u32,
    path: Option<PathIdx>,
    name: &str,
    range: Range,
    site: ArrayLenSite,
) -> Result<Option<u128>, Why> {
    let Some(row) = locals.find(owner, Some(scope), name, LocalNs::Value) else { return Ok(None) };
    let repeat = site == ArrayLenSite::Repeat;
    match row.kind {
        LocalKind::Const => {
            let bits = const_item_bits(heads, row.key, name, range)?;
            if bits > u128::from(u64::MAX) {
                return Err(Why::Unsupported(
                    if repeat {
                        "an array repeat length past `usize` (a constant declared in the body)"
                    } else {
                        "an array length past `usize` (a constant declared in the body)"
                    },
                    range,
                ));
            }
            Ok(Some(bits))
        }
        LocalKind::Use => match resolve_use_target(resolver, marks, locals, owner, module, view, scope, path, name, &row, Ns::Value, range)? {
            Some(target) if target.kind == Kind::Const => {
                let bits = const_item_bits(heads, target.key, name, range)?;
                if bits > u128::from(u64::MAX) {
                    return Err(Why::Unsupported(
                        if repeat {
                            "an array repeat length past `usize` (a constant named through a `use` declared in the body)"
                        } else {
                            "an array length past `usize` (a constant named through a `use` declared in the body)"
                        },
                        range,
                    ));
                }
                Ok(Some(bits))
            }
            Some(_) => Err(Why::Unsupported(
                if repeat {
                    "an array repeat length named through a `use` declared in the body that is not a constant"
                } else {
                    "an array length named through a `use` declared in the body that is not a constant"
                },
                range,
            )),
            None => {
                // If the `use` only imported into the type namespace (e.g. a braced struct),
                // it does not shadow the value namespace: let the caller read the module's
                // value namespace (which resolves any module const or reports unresolved).
                if resolve_use_target(resolver, marks, locals, owner, module, view, scope, path, name, &row, Ns::Type, range)?.is_some() {
                    return Ok(None);
                }
                Err(Why::Unsupported(super::block_type_uses::NAMED_TYPE_TARGET, range))
            }
        },
        _ => Err(Why::Unsupported(
            if repeat {
                "an array repeat length naming an item of the body that is not a constant"
            } else {
                "an array length naming an item of the body that is not a constant"
            },
            range,
        )),
    }
}

/// The value of the array repeat length `len` of the body `owner` (its text `text`, at
/// `range`) when it is ONE name that a `const` or `use` item declared in the body takes
/// ([`Locals::find`], from the block the name is written in outwards: a block's item shadows
/// the module's and the fn's generic parameters, the Reference, "Names", "Scopes"): the
/// constant's bits once evaluated, [`Why::NeedConst`] naming its key until then (the driver
/// evaluates it and converts the body again, as for a module's constant). `Ok(None)`: no item
/// of the body has the name, and the caller reads it as before (a const parameter, the
/// module's names). A body's item (or imported value) of that name that is no constant is
/// refused by name, never looked up at the module, where the name would mean another item.
#[expect(clippy::too_many_arguments, reason = "one lookup of a body-local array repeat length")]
pub(super) fn local_const<'n, H: Heads + ?Sized, S: NamesSource<'n>>(
    resolver: &Paths<S>,
    marks: Option<TextMarks<'_>>,
    locals: &Locals<'_>,
    owner: ItemKey,
    module: LibKey,
    heads: &H,
    view: &View<'_>,
    len: ExprId,
    text: &str,
    range: Range,
) -> Result<Option<u128>, Why> {
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
    let mut found_path = None;
    let mut scope = None;
    for _ in 0..MOST_WRAPS {
        match view.expr(at)? {
            lf::Expr::Path(p) => {
                found_path = Some(p);
                scope = view.path(p)?.scope;
                break;
            }
            lf::Expr::Paren(inner) => at = inner,
            lf::Expr::Block { stmts, tail: Some(tail), .. } if stmts.len == 0 => at = tail,
            _ => return Ok(None),
        }
    }
    let Some((owner, scope)) = locals.scope_or_declared(owner, scope.map(|s| s.0)) else { return Ok(None) };
    local_const_in_scope(resolver, marks, locals, owner, module, heads, view, scope, found_path, name, range, ArrayLenSite::Repeat)
}

/// What a generic argument written as ONE plain name means when an item declared in the body
/// takes the name ([`Locals::find`], from the block the name is written in outwards).
pub(super) enum LocalArg {
    /// A type the body declares (or imports via a block `use`): the argument is a type (the
    /// Reference, "Generic parameters", "Const generics": "When there is ambiguity if a
    /// generic argument could be resolved as either a type or const argument, it is always
    /// resolved as a type.").
    Type,
    /// A `const` the body declares (or imports via a block `use`), by its evaluated bits
    /// (`foo::<CHUNK_SIZE>()` with `const CHUNK_SIZE: usize = ..;` or `use m::CHUNK_SIZE;` in
    /// the body: the pinned core's ptr::swap_nonoverlapping_bytes).
    Const(u128),
}

/// The local item that the plain-name generic argument `t` (its text `name`, at `range`) of
/// the body `owner` names. The type namespace is asked first: [`LocalArg::Type`] when a block
/// around the argument declares or imports a type of that name. Otherwise [`LocalArg::Const`]
/// when one declares or imports a `const`, once that constant is evaluated, and
/// [`Why::NeedConst`] naming its key until then: the driver evaluates it and converts the body
/// again, as [`local_const`] does. `Ok(None)`: no item of the body takes the name, and the
/// caller reads the module's names. A local value item (or imported value) that is no `const`
/// is refused by name, never looked up at the module, where the name would mean another item.
#[expect(clippy::too_many_arguments, reason = "one lookup of a body-local generic argument")]
pub(super) fn local_arg<'n, H: Heads + ?Sized, S: NamesSource<'n>>(
    resolver: &Paths<S>,
    marks: Option<TextMarks<'_>>,
    locals: &Locals<'_>,
    owner: ItemKey,
    module: LibKey,
    heads: &H,
    view: &View<'_>,
    t: lf::TypeId,
    name: &str,
    range: Range,
) -> Result<Option<LocalArg>, Why> {
    let lf::TypeRef::Path(p) = view.ty(t)? else { return Ok(None) };
    let Some((owner, scope)) = locals.scope_or_declared(owner, view.path(p)?.scope.map(|s| s.0)) else { return Ok(None) };
    if let Some(row) = locals.find(owner, Some(scope), name, LocalNs::Type) {
        if row.kind != LocalKind::Use {
            return Ok(Some(LocalArg::Type));
        }
        let ty_target = resolve_use_target(resolver, marks, locals, owner, module, view, scope, Some(p), name, &row, Ns::Type, range)?;
        if let Some(target) = ty_target
            && matches!(target.kind, Kind::Struct | Kind::Enum | Kind::Union | Kind::TypeAlias | Kind::Builtin(_) | Kind::WideInt { .. })
        {
            return Ok(Some(LocalArg::Type));
        }
        return match resolve_use_target(resolver, marks, locals, owner, module, view, scope, Some(p), name, &row, Ns::Value, range)? {
            Some(target) if target.kind == Kind::Const => {
                let bits = const_item_bits(heads, target.key, name, range)?;
                Ok(Some(LocalArg::Const(bits)))
            }
            Some(_) => Err(Why::Unsupported("a const argument named through a `use` declared in the body that is not a constant", range)),
            None => {
                if ty_target.is_some() {
                    Ok(Some(LocalArg::Type))
                } else {
                    Err(Why::Unsupported(super::block_type_uses::NAMED_TYPE_TARGET, range))
                }
            }
        };
    }
    let Some(row) = locals.find(owner, Some(scope), name, LocalNs::Value) else { return Ok(None) };
    match row.kind {
        LocalKind::Const => {
            let bits = const_item_bits(heads, row.key, name, range)?;
            Ok(Some(LocalArg::Const(bits)))
        }
        _ => Err(Why::Unsupported("a const argument naming an item of the body that is not a constant", range)),
    }
}

#[cfg(test)]
mod tests {
    use crate::front2::project::{BodyRef, ProjectWorld};
    use crate::front2::source::NamesSource;
    use crate::front2::tables::head_tests::{a, world_of};
    use std::format;
    use std::string::String;

    fn check_fn(app: &'static str, name: &str) -> Result<std::vec::Vec<&'static str>, String> {
        let (names, _, frame) = world_of(app);
        let mut world = ProjectWorld::from_records(names, frame, &[1], a()).expect("project world");
        let mut target = None;
        NamesSource::each_item(names, 0, &mut |key, _| {
            if NamesSource::name_of(names, key) == Some(name) {
                target = Some(key);
            }
        });
        let lib = target.unwrap_or_else(|| panic!("fn `{name}` not found"));
        let verdict = world.check(BodyRef::Free(lib), a());
        if let Some(refused) = verdict.refused {
            return Err(format!("{refused:?}"));
        }
        assert!(verdict.gaps.is_empty(), "`{name}` had gaps: {:?}", verdict.gaps);
        Ok(verdict.diagnostics.iter().map(|d| d.code).collect())
    }

    const APP: &str = "#![no_std]
pub mod diag {
    pub const MOST_PENDING_SHOWN: usize = 4;
    pub struct Marker;
    pub fn not_a_const() -> usize { 4 }
}
pub struct Wrap<const N: usize>(pub [u8; N]);
pub fn take<const N: usize>() -> [u8; N] { [0u8; N] }
pub fn id<T>(x: T) -> T { x }

pub fn repeat_use_ok() -> [u8; 4] {
    use crate::diag::MOST_PENDING_SHOWN;
    [0u8; MOST_PENDING_SHOWN]
}
pub fn repeat_use_wrong_len() -> [u8; 5] {
    use crate::diag::MOST_PENDING_SHOWN;
    [0u8; MOST_PENDING_SHOWN]
}
pub fn repeat_and_type_use_ok() -> [u8; 4] {
    use crate::diag::MOST_PENDING_SHOWN;
    let buf: [u8; MOST_PENDING_SHOWN] = [0u8; MOST_PENDING_SHOWN];
    buf
}
pub fn repeat_use_renamed_ok() -> [u8; 4] {
    use crate::diag::MOST_PENDING_SHOWN as LIMIT;
    [0u8; LIMIT]
}
pub fn block_mod_use_ok() -> [u8; 3] {
    mod inner { pub const K: usize = 3; }
    use inner::K;
    [0u8; K]
}
pub fn turbofish_use_const_ok() -> [u8; 4] {
    use crate::diag::MOST_PENDING_SHOWN;
    take::<MOST_PENDING_SHOWN>()
}
pub fn turbofish_use_const_wrong_len() -> [u8; 5] {
    use crate::diag::MOST_PENDING_SHOWN;
    take::<MOST_PENDING_SHOWN>()
}
pub fn type_arg_use_const_ok() -> Wrap<4> {
    use crate::diag::MOST_PENDING_SHOWN;
    let w: Wrap<MOST_PENDING_SHOWN> = Wrap([0u8; 4]);
    w
}
pub fn turbofish_use_type_ok() -> crate::diag::Marker {
    use crate::diag::Marker;
    id::<Marker>(Marker)
}
pub fn repeat_use_fn_refused() {
    use crate::diag::not_a_const;
    let _ = [0u8; not_a_const];
}
pub fn type_len_use_fn_refused() {
    use crate::diag::not_a_const;
    let _: [u8; not_a_const] = [0u8; 4];
}
pub fn turbofish_use_fn_refused() {
    use crate::diag::not_a_const;
    let _ = take::<not_a_const>();
}
pub fn local_fn_sig_const_array_len_ok() -> [u8; 4] {
    const MAX_PARTS: usize = 4;
    fn fill(out: &mut [u8; MAX_PARTS]) {
        out[0] = 1;
    }
    let mut buf = [0u8; MAX_PARTS];
    fill(&mut buf);
    buf
}
pub fn local_fn_sig_const_array_len_wrong() -> [u8; 5] {
    const MAX_PARTS: usize = 4;
    fn make() -> [u8; MAX_PARTS] {
        [0u8; MAX_PARTS]
    }
    make()
}
pub fn local_fn_sig_const_generic_arg_ok() -> Wrap<4> {
    const MAX_PARTS: usize = 4;
    fn wrap(w: Wrap<MAX_PARTS>) -> Wrap<MAX_PARTS> {
        w
    }
    wrap(Wrap([0u8; MAX_PARTS]))
}
pub fn local_fn_sig_use_const_array_len_ok() -> [u8; 4] {
    use crate::diag::MOST_PENDING_SHOWN;
    fn make() -> [u8; MOST_PENDING_SHOWN] {
        [0u8; MOST_PENDING_SHOWN]
    }
    make()
}
pub fn local_const_repeat_sibling_const_ok() -> [u8; 4] {
    const A: usize = 4;
    const B: [u8; A] = [0u8; A];
    B
}
";

    #[test]
    fn body_use_const_resolves_for_repeat_type_len_and_const_generic_args() {
        for ok in [
            "repeat_use_ok",
            "repeat_and_type_use_ok",
            "repeat_use_renamed_ok",
            "block_mod_use_ok",
            "turbofish_use_const_ok",
            "type_arg_use_const_ok",
            "turbofish_use_type_ok",
            "local_fn_sig_const_array_len_ok",
            "local_fn_sig_const_generic_arg_ok",
            "local_fn_sig_use_const_array_len_ok",
            "local_const_repeat_sibling_const_ok",
        ] {
            let diags = check_fn(APP, ok).unwrap_or_else(|e| panic!("`{ok}` was refused: {e}"));
            assert!(diags.is_empty(), "`{ok}` had unexpected diagnostics: {diags:?}");
        }
        // Negative type-check controls: verify the evaluated value (4), not a hole, was used.
        let wrong_repeat = check_fn(APP, "repeat_use_wrong_len").expect("repeat_use_wrong_len should typecheck with E0308");
        assert!(wrong_repeat.contains(&"E0308"), "expected E0308, got {wrong_repeat:?}");
        let wrong_arg = check_fn(APP, "turbofish_use_const_wrong_len").expect("turbofish_use_const_wrong_len should typecheck with E0308");
        assert!(wrong_arg.contains(&"E0308"), "expected E0308, got {wrong_arg:?}");
        let wrong_local_sig = check_fn(APP, "local_fn_sig_const_array_len_wrong").expect("local_fn_sig_const_array_len_wrong should typecheck with E0308");
        assert!(wrong_local_sig.contains(&"E0308"), "expected E0308, got {wrong_local_sig:?}");
    }

    #[test]
    fn body_use_non_const_still_refuses_by_name() {
        for bad in ["repeat_use_fn_refused", "type_len_use_fn_refused", "turbofish_use_fn_refused"] {
            let err = check_fn(APP, bad).expect_err(&format!("`{bad}` must be refused"));
            assert!(err.contains("is not a constant"), "`{bad}` refusal did not name non-const: {err}");
        }
    }
}
