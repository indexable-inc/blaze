//! A type alias whose type writes `impl Trait`: TYPE ALIAS IMPL TRAIT (RFC 2515, unstable
//! `type_alias_impl_trait`, as the pinned nightly implements it with `#[define_opaque]`).
//!
//! The case (tempo census, 34 heads and about 90 bodies): the pinned std writes `type
//! LazyResolve = impl (FnOnce() -> Capture) + Send + Sync + UnwindSafe;` (library/std/src/
//! backtrace.rs) and `struct LazyLock<Capture, LazyResolve>` in `Inner`; the alias's head was
//! refused "an `impl Trait` type outside a fn's parameters and return type" (types.rs, the
//! `Impl` arm: no `impl` of the alias was in `Env::anon`), and with it every head and body that
//! reads `std::backtrace::Backtrace`.
//!
//! THE RULE. Each `impl Trait` written in the alias's type is ONE opaque type of the alias (the
//! RFC: "a type alias ... `type Foo = impl Bar;` ... `Foo` is an opaque type"), numbered as a
//! fn's return opaques are (super::types `return_impls`: preorder, left to right), keyed by the
//! alias's key and that number (super::keys `opaque_key`), its identity the alias's generic
//! parameters (`type A<T> = impl Tr<T>;`: `A<u8>` and `A<u16>` are two types). Its bounds are
//! the written bounds with the opaque as `Self`, and its implied `Sized` (preds.rs
//! `impl_bounds`, the one home for an opaque's bounds). It is the SAME representation as a
//! fn's return-position `impl Trait` (`PNode::Opaque(key, identity)` with its bounds as the
//! opaque key's predicates, `Store::opaques` naming its owner: rule 30); only its owner is an
//! alias, which [`ALIAS_OPAQUE`] says in the owner row's index.
//!
//! Its HIDDEN type is fixed by its defining uses: the fns written `#[define_opaque(Alias)]`.
//! Everywhere else it is opaque: a body sees only its bounds (a method through a bound
//! resolves), and an AUTO TRAIT of it leaks from the hidden type (the Reference, "Impl trait",
//! "Auto trait leakage"). That leak needs the defining fns' checks, which this front end does
//! not make for an alias yet (the checker's opaque definition, blaze-typeck rules/opaque_def.rs,
//! knows a fn's own opaques only): `Send`, `Sync` or any auto trait asked of an alias's opaque
//! is refused BY NAME ([`HIDDEN_OWED`], front2/opaque_auto.rs `run`), never answered by a guess.

use std::vec::Vec;

use blaze_id::ItemKey as LibKey;
use blaze_lower::{View, form as lf};
use blaze_own::Alloc;
use blaze_surface::Range;
use blaze_typeck::{ItemKey, List, PNode, PTy, PortableTys};

use super::keys::opaque_key;
use super::preds::Preds;
use super::source::NamesSource;
use super::types::{Builder, Env, Heads, Why, intern, intern_list, return_impls};

/// The bit an opaque's owner row (`Store::opaques`: owner key, index) sets in its index when
/// the owner is a type alias, not a fn: the `j`th `impl Trait` of alias `a` is `(checker_of(a),
/// ALIAS_OPAQUE | j)`. A fn has far fewer than 2^31 return opaques, so no fn's index has it.
pub const ALIAS_OPAQUE: u32 = 1 << 31;

/// Why an auto trait of an alias's opaque is not answered: its hidden type is its defining
/// fns', which are not checked for it here (module doc). The words front2/opaque_auto.rs's
/// refusal carries.
pub const HIDDEN_OWED: &str = "an auto trait of a type alias's `impl Trait` (type_alias_impl_trait): its hidden type is fixed by the fns written `#[define_opaque(..)]` with the alias, which this front end does not check for it yet";

/// An alias whose type writes [`ALIAS_OPAQUE`] or more `impl`s (rule 18: the numbering's cap).
const TOO_MANY: &str = "a type alias with more than 2^31 `impl Trait` types";

/// Whether an opaque owner row's `index` is an alias's ([`ALIAS_OPAQUE`]).
pub fn of_alias(index: u32) -> bool {
    index & ALIAS_OPAQUE != 0
}

/// The opaques an alias's written type `t` (in `v`) has, each with the type id it is written
/// at and the opaque type it is: `Opaque(opaque_key(alias, j), identity)`, `identity` the
/// alias's own generic parameters (`generics` of them, the const ones `consts` by index, as
/// heads.rs `own_generics` lists them). Empty for an alias that writes no `impl`. The list is
/// the `Env::anon` the alias's type is built with, so each `impl` is its opaque.
pub(super) fn alias_opaques<'a>(tys: &mut PortableTys<'_>, v: &View<'_>, t: lf::TypeId, alias: ItemKey, generics: u32, consts: &[(u32, PTy)], alloc: Alloc<'a>) -> Result<Vec<(lf::TypeId, PTy), Alloc<'a>>, Why> {
    let mut written: Vec<lf::TypeId, Alloc<'a>> = Vec::new_in(alloc);
    // A type macro in the alias's type is expanded by its own reader (pattern_ty.rs
    // `macro_ty`), which refuses an `impl` its expansion writes by name: only the `impl`s
    // written in the alias's own text are its opaques here.
    return_impls(v, t, &mut written)?;
    let mut out: Vec<(lf::TypeId, PTy), Alloc<'a>> = Vec::with_capacity_in(written.len(), alloc);
    if written.is_empty() {
        return Ok(out);
    }
    let identity = identity(tys, generics, consts, alloc)?;
    for (j, &at) in written.iter().enumerate() {
        let j = u32::try_from(j).ok().filter(|&j| j < ALIAS_OPAQUE).ok_or(Why::Unsupported(TOO_MANY, Range::new(0, 0)))?;
        out.push((at, intern(tys, PNode::Opaque(opaque_key(alias, j), identity))?));
    }
    Ok(out)
}

/// `Param(0)..Param(generics)`, a const parameter's as `ConstParam`: the alias's parameters
/// as its opaques' identity (every parameter in scope is captured, as a fn's return opaque
/// captures its fn's: heads.rs `fn_head`'s `ident`).
fn identity(tys: &mut PortableTys<'_>, generics: u32, consts: &[(u32, PTy)], alloc: Alloc<'_>) -> Result<List, Why> {
    let mut l: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(generics as usize, alloc);
    for i in 0..generics {
        let node = if consts.iter().any(|&(c, _)| c == i) { PNode::ConstParam(i) } else { PNode::Param(i) };
        l.push(intern(tys, node)?);
    }
    intern_list(tys, &l)
}

/// One opaque of an alias, ready to file: its key, its owner row's index (with
/// [`ALIAS_OPAQUE`]) and its predicates.
pub(super) struct AliasOpaque {
    pub key: ItemKey,
    pub index: u32,
    pub preds: Preds,
}

/// The bounds of each of `opaques` ([`alias_opaques`]) of alias `alias`, read in the alias's
/// `env` (whose `anon` is `opaques`, so a bound naming another of the alias's `impl`s reads it
/// as that opaque), with `sized` the program's `Sized` (the implied bound). The one reading of
/// an opaque's bounds (preds.rs `impl_bounds`), as a fn's return opaques' are read.
pub(super) fn alias_opaque_preds<'n, 'a, H: Heads, S: NamesSource<'n>>(
    b: &Builder<'_, H, S>,
    tys: &mut PortableTys<'_>,
    env: &Env<'_>,
    v: &View<'_>,
    alias: ItemKey,
    opaques: &[(lf::TypeId, PTy)],
    sized: Option<LibKey>,
    alloc: Alloc<'a>,
) -> Result<Vec<AliasOpaque, Alloc<'a>>, Why> {
    let mut out: Vec<AliasOpaque, Alloc<'a>> = Vec::with_capacity_in(opaques.len(), alloc);
    for (j, &entry) in opaques.iter().enumerate() {
        // `alias_opaques` numbered them below `ALIAS_OPAQUE`.
        let j = j as u32;
        let mut preds = Preds::new();
        b.impl_bounds(tys, env, v, &[entry], sized, &mut preds)?;
        out.push(AliasOpaque { key: opaque_key(alias, j), index: ALIAS_OPAQUE | j, preds });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{ALIAS_OPAQUE, of_alias};

    /// An alias's opaque row says so and keeps its number; a fn's never does. A row that lost
    /// the bit would send the auto-trait question to a fn body that does not exist (the alias
    /// has none); one that lost the number would make two `impl`s of one alias one type.
    #[test]
    fn an_alias_opaque_row_is_told_from_a_fn_one() {
        for j in [0u32, 1, 7, 1000] {
            assert!(of_alias(ALIAS_OPAQUE | j), "alias opaque {j}");
            assert_eq!((ALIAS_OPAQUE | j) & !ALIAS_OPAQUE, j, "alias opaque {j} keeps its number");
            assert!(!of_alias(j), "fn opaque {j}");
        }
        println!("tait rows: alias opaques 0, 1, 7, 1000 told from fn opaques, numbers kept");
    }
}
