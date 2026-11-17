//! The one home that lowers a body and makes what its lowering needs first (census 14, brief D;
//! rule 30). A lowering stops with a NEED, never a refusal, when a head or a constant it reads
//! is not made yet:
//!
//! - [`Why::Need`]`(k)` from the fn's own generics or bounds ([`BodyError::Generics`]):
//!   `fn f<X: T<A = u8>>()` reads the head of `T` to put the binding `A = u8` against the
//!   trait's associated type, and `T`'s head may not be made yet;
//! - [`Why::NeedConst`]`(c, _)` from an array length naming a constant, in the body's types
//!   ([`BodyError::Convert`]), in a head it needs ([`BodyError::Head`]) or in its generics.
//!
//! Each is made here (a head by [`Store::ensure`] of [`Store::need`], a constant by
//! [`eval_len`]) and the body lowered again, until it lowers or refuses for a reason that is
//! not a need. Before this every driver (the tempo census's `take`, reach.rs, project.rs)
//! carried its own copy of the loop and only the constant case, so a `Need` from the generics
//! was printed as a refusal (`lower Generics: Need <key>`, 21 of tempo's bodies) instead of
//! made.
//!
//! The work is bounded (rule 18): at most [`MOST_MADE`] makes for one body, and a need asked
//! again right after it was made is no progress and is refused by name
//! ([`BodyError::Made`]), never retried. A fn whose own generics need the fn's own head is a
//! cycle, refused as [`HeadError::Cycle`] naming it. No recursion: a head that itself needs a
//! constant has the constant evaluated by the same loop.

use std::alloc::Global;
use std::boxed::Box;
use std::vec::Vec;

use blaze_id::ItemKey as LibKey;
use blaze_lower::scoped::Prepares;
use blaze_typeck::ItemKey;

use super::body::{Body, BodyError, lower_fn, lower_member};
use super::check::{Records, eval_len};
use super::heads::{HeadError, Want};
use super::source::NamesSource;
use super::types::Why;

/// The most heads and constants one body's lowering may have made before it is refused
/// ([`BodyError::Made`]): the drivers' former `MOST_LENS` bound, now over both kinds of need.
pub const MOST_MADE: u32 = 64;

/// Which body to lower: a free fn (or a free constant's or static's initializer) by its
/// library key, or a member fn of a trait or an impl by its checker key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LowerTarget {
    Free(LibKey),
    Member(ItemKey),
}

/// What a lowering stopped for and this loop makes: a head (the want [`Store::need`] gives for
/// a [`Why::Need`]) or a constant an array length names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Needed {
    Head(Want),
    Const(LibKey),
}

/// The need a lowering's error stands for, with the item it names; `None` for every other
/// error (a refusal that stays: a real one). A `Need` of the item itself is the cycle error.
fn need_of(store_need: impl Fn(LibKey) -> Want, e: &BodyError) -> Option<Result<(LibKey, Needed), BodyError>> {
    match e {
        BodyError::Convert { item, why: Some(Why::NeedConst(c, _)), .. } => Some(Ok((*item, Needed::Const(*c)))),
        BodyError::Head { item, error: HeadError::Item(_, Why::NeedConst(c, _)) } => Some(Ok((*item, Needed::Const(*c)))),
        BodyError::Generics { item, why: Why::NeedConst(c, _) } => Some(Ok((*item, Needed::Const(*c)))),
        BodyError::Generics { item, why: Why::Need(k) } if k == item => {
            let mut ks = Vec::with_capacity_in(1, Global);
            ks.push(*item);
            Some(Err(BodyError::Head { item: *item, error: HeadError::Cycle(ks) }))
        }
        BodyError::Generics { item, why: Why::Need(k) } => Some(Ok((*item, Needed::Head(store_need(*k))))),
        _ => None,
    }
}

/// Lowers `target`'s body, making every head ([`Why::Need`]) and evaluating every constant
/// ([`Why::NeedConst`]) its lowering asks for first, then lowering again; at most
/// [`MOST_MADE`] makes. The one loop every driver calls (rule 30): an error it returns is a
/// refusal that stays.
pub fn lower_made<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, target: LowerTarget) -> Result<Body<'n>, BodyError> {
    let mut makes = 0u32;
    // The need made last: asked again at once, it was made and the lowering still reads it
    // as missing (no progress), which is refused by name.
    let mut made_last: Option<Needed> = None;
    loop {
        let lowered = match target {
            LowerTarget::Free(lib) => lower_fn(r.names, prepared, r.keys, r.store, lib),
            LowerTarget::Member(k) => lower_member(r.names, prepared, r.keys, r.store, k),
        };
        let found = match &lowered {
            Err(e) => need_of(|k| r.store.need(k), e),
            Ok(_) => None,
        };
        let (item, first) = match found {
            None => return lowered,
            Some(Err(cycle)) => return Err(cycle),
            Some(Ok(at)) => at,
        };
        drop(lowered);
        // Make `first`; a head that itself needs a constant has the constant evaluated and
        // the body is lowered again (which asks for the head again).
        let mut need = first;
        loop {
            if made_last == Some(need) || makes == MOST_MADE {
                return Err(BodyError::Made { item, need, after: makes });
            }
            makes += 1;
            made_last = Some(need);
            match need {
                Needed::Const(c) => {
                    eval_len(r, c).map_err(|error| BodyError::Const { item: c, error: Box::new_in(error, Global) })?;
                    break;
                }
                Needed::Head(w) => match r.store.ensure(r.names, r.keys, w) {
                    Ok(()) => break,
                    Err(HeadError::Item(_, Why::NeedConst(c, _))) => need = Needed::Const(c),
                    Err(error) => return Err(BodyError::Head { item, error }),
                },
            }
        }
    }
}
