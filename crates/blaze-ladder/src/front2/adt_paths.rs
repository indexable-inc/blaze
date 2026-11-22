//! A struct's, enum's or union's segment in an expression's or a pattern's path, written with
//! FEWER arguments than the type has parameters where every parameter left out has a default:
//! `Vec::<u8>::with_capacity(n)` against the pinned library's `Vec<T, A: Allocator = Global>`.
//!
//! The Reference, "Paths", "Paths in expressions" writes this very form as its example of the
//! turbofish (`Vec::<u8>::with_capacity(1024);`), so a program with it is Rust; the Reference
//! does not say how the left-out `A` is filled. blaze fills it as a type path fills it (the
//! Reference, "Generic parameters": `Vec<u8>` is `Vec<u8, Global>`; the one home of that is
//! [`super::types::with_defaults`]), as a type alias's segment in a body path already is
//! ([`super::world_paths::AliasPaths::expand`]): the written arguments, then each default with
//! the arguments before it put in. A variant's path is filled the same way, on whichever of the
//! enum's segment and the variant's carries the arguments (`FilterChanges::<T>::Logs(x)`,
//! `E::V::<T>`; `expand`'s `Kind::Variant` branch). A fresh variable in its place would leave `A` unconstrained
//! where no impl's self type fixes it (`impl<T, A: Mk> V<T, A> { fn make() -> Self }`,
//! `V::<u8>::make()`): an ambiguity where the default answers.
//!
//! What is NOT filled here, and stays the checker's count error (E0107, blaze-typeck
//! `explicit_args`, which names the item and both counts): no argument written (every
//! parameter is then inferred, the checker's fresh variables), more arguments than parameters,
//! or a parameter left out that has no default.

use blaze_id::ItemKey as LibKey;
use blaze_surface::Range;
use blaze_typeck::{PTy, PortableTys};

use super::types::{Heads, Why, with_defaults};

/// The arguments of ADT `adt`'s segment `shown` written as `own` in a body path, with the
/// defaults of the parameters left out put in, each given to `out` in order: `Ok(true)` when
/// given, `Ok(false)` when the segment is to stay as written (see the module's doc for which).
/// `Why::Need` while the ADT's head is not made: the body is converted again once it is.
pub(super) fn adt_segment_defaults<H: Heads>(heads: &H, adt: LibKey, own: &[PTy], tys: &mut PortableTys<'_>, shown: &str, r: Range, out: &mut dyn FnMut(PTy)) -> Result<bool, Why> {
    if own.is_empty() {
        return Ok(false);
    }
    let (g, defaults) = heads.adt_generics(adt).ok_or(Why::Need(adt))?;
    if own.len() >= g as usize {
        return Ok(false);
    }
    let every_default = (own.len()..g as usize).all(|i| defaults.get(i).copied().flatten().is_some());
    if !every_default {
        return Ok(false);
    }
    let args = with_defaults(tys, own, g, defaults, shown, r)?;
    for &a in args.iter() {
        out(a);
    }
    Ok(true)
}
