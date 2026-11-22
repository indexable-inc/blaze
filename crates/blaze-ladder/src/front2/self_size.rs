//! The implied sizedness of a trait's `Self`: `Self: MetaSized` in the env of every member of a
//! trait whose header states no sizedness, as `Self: Trait` is there.
//!
//! The sized hierarchy (RFC 3729; the pinned core, library/core/src/marker.rs): `pub trait
//! Sized: MetaSized`, `pub trait MetaSized: PointeeSized`, `pub trait PointeeSized`. A trait's
//! `Self` has no implied `Sized` (the Reference, "Special types and traits", `Sized`: "Type
//! parameters (except `Self` in traits) are `Sized` by default"), and `?Sized` is one level down the hierarchy, `MetaSized`; only a
//! written `PointeeSized` states no size at all. The pinned core says so by what it writes:
//! `pub trait Debug: PointeeSized` (core/src/fmt/mod.rs), `pub const trait PartialEq<Rhs:
//! PointeeSized = Self>: PointeeSized` (core/src/cmp.rs), `pub const trait Destruct:
//! PointeeSized` (marker.rs): a bound that every type meets is written only to relax a
//! default, and that default is `Self: MetaSized`.
//!
//! What needs it, on the hello-world path: core's `fmt::Write::write_fmt` (core/src/fmt/mod.rs)
//! declares `trait SpecWriteFmt` and `impl<W: Write + ?Sized> SpecWriteFmt for &mut W` in its
//! body and calls `self.spec_write_fmt(args)` with `self: &mut Self`. The impl's `W: ?Sized` is
//! `W: MetaSized`; with `W = Self` that is `Self: MetaSized`, which the solver proves for a
//! parameter only from its env (blaze-solve builtin.rs: a `Param` has no builtin sizedness).
//! Without this clause the method probe found no applicable candidate and the body failed
//! with E0599 ("found `&mut Param(0)`"), so `write_fmt` was left out of the program, and with
//! it `bstd::guest::Outcome::status` (stub 6 of port27/hello-1116/stops-3.txt) and every
//! `to_string`. std's `io::Write::write_fmt` failed the same clause by name (E0277
//! `Param(0): MetaSized`: its `Adapter<'_, Self>` has `T: ?Sized`).
//!
//! Which traits get it: every trait but the sizedness traits themselves (`Sized` and the
//! traits above it, found as `Sized`'s supertraits by the heads, as preds.rs `implied_size`
//! finds `MetaSized`), and none whose header (a supertrait or a `where Self:` clause) names
//! one of them: `trait Clone: Sized` is already sized, `trait Debug: PointeeSized` states no
//! size. A core whose `Sized` has no supertrait (a `no_core` rung) gets no clause, as for
//! `?Sized` there.
//!
//! Kept out on purpose: the clause is not filed among the trait's supertraits (`Maps::supers`),
//! which the vtable layout reads (program.rs `ObjectTrait::supers`); so `T: Trait` does not
//! elaborate to `T: MetaSized` in other envs. A parameter bounded by a trait is `Sized` or
//! `?Sized` (`MetaSized`) by its own default, so only `T: PointeeSized + Trait` misses it.

use blaze_id::ItemKey as LibKey;
use blaze_surface::Range;
use blaze_typeck::List;

use super::types::{Heads, Why};

/// How many sizedness traits the walk up from `Sized` takes before refusing (core has three).
pub(super) const MOST_SIZEDNESS: u32 = 8;

/// Refused by name: a `Sized` whose supertraits are not one chain.
pub(super) const MANY_SIZED_SUPERS: &str = "a sizedness trait with more than one supertrait (the sized hierarchy is one chain: `Sized: MetaSized: PointeeSized`), so a trait's implied `Self` sizedness is not known";

/// Refused by name: a chain of sizedness traits longer than the bound.
pub(super) const SIZEDNESS_TOO_DEEP: &str = "a chain of `Sized`'s supertraits longer than the bound, so a trait's implied `Self` sizedness is not known";

/// The trait of the implied clause on trait `key`'s `Self` (`MetaSized`, the one supertrait of
/// `sized`), or `None` when it has none: `key` is a sizedness trait, its header `supers` (its
/// clauses on `Self`, as `trait_members` files them) names one, or there is no `Sized` or no
/// trait above it. A [`Why::Need`] of a sizedness trait whose members are not made yet is
/// passed up. Never asks the heads for `key`'s own supertraits, so making a sizedness trait's
/// members never waits on itself: the walk stops at `key` before reading what is above it.
pub(super) fn implied_self_size<H: Heads>(heads: &H, key: LibKey, supers: &[(LibKey, List)], sized: Option<LibKey>) -> Result<Option<LibKey>, Why> {
    let Some(sized) = sized else { return Ok(None) };
    let mut meta = None;
    let mut t = sized;
    for depth in 0..MOST_SIZEDNESS {
        // `key` is `Sized` or a trait above it: the hierarchy's own traits get no default.
        if t == key {
            return Ok(None);
        }
        // The header names a sizedness trait on `Self`: it states the size itself.
        if supers.iter().any(|&(s, _)| s == t) {
            return Ok(None);
        }
        let next = match heads.trait_supers(t).ok_or(Why::Need(t))? {
            [] => return Ok(meta),
            [(s, _)] => *s,
            _ => return Err(Why::Unsupported(MANY_SIZED_SUPERS, Range::new(0, 0))),
        };
        if depth == 0 {
            meta = Some(next);
        }
        t = next;
    }
    Err(Why::Unsupported(SIZEDNESS_TOO_DEEP, Range::new(0, 0)))
}
