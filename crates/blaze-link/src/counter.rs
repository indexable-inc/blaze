//! Tier-up counter pairs (`blaze_stencil::tier`): the two words a function's own code counts
//! its calls and back edges in ([`blaze_codegen::Target::Counter`]), in a dev run's cells
//! reservation ([`crate::DevRun::cells`]), which every run process maps shared both ways and
//! the daemon reads in place.
//!
//! Pair `n` is the 16 bytes at `16 * n` from the reservation's start, given to instance keys in
//! the order first seen ([`pair`]; the lazy placer's from the top, [`placed_pair`]) and kept in
//! the image's state ([`crate::state::Pairs`]). A
//! pair is never freed or given to another key while the image lives: a removed function's
//! code, or a tier-up's old code, may still be running in a thread and adding to it. The
//! reservation has no file bytes and is zero when mapped, so nothing writes the pairs.

#![forbid(unsafe_code)]

use blaze_codegen::InstanceKey;

use crate::model::Apply;
use crate::state::State;
use crate::symbols::IdNo;
use crate::{LinkError, Output};

/// Bytes of one pair: the calls word, then the back edges word, 8-aligned.
pub(crate) const PAIR: u64 = 16;

/// How many pairs a dev run's cells hold; 0 for an image that is not a dev run.
pub(crate) fn capacity(output: &Output) -> u32 {
    output.dev_run.map_or(0, |it| u32::try_from(it.cells / PAIR).unwrap_or(u32::MAX))
}

/// The pair of `key` (interned as `id`): the one it was given before, else the next one.
/// Counting code in an image that is not a dev run, and a dev run whose cells are all given
/// out, are errors naming `key`.
pub(crate) fn pair(output: &Output, state: &mut State, id: IdNo, key: &InstanceKey) -> Result<u32, LinkError> {
    give(output, state, id, key, false)
}

/// [`pair`] for the lazy placer ([`crate::lazy`]), which places functions while a stage may be
/// in flight on a copy of the state: a new pair is the next from the top of the cells, never
/// one a stage gives from the bottom, and the commit of that stage carries it over
/// ([`crate::Image::commit`]). A key has one pair whichever gave it.
pub(crate) fn placed_pair(output: &Output, state: &mut State, id: IdNo, key: &InstanceKey) -> Result<u32, LinkError> {
    give(output, state, id, key, true)
}

fn give(output: &Output, state: &mut State, id: IdNo, key: &InstanceKey, placed: bool) -> Result<u32, LinkError> {
    if let Some(pair) = state.pairs.of(id) {
        return Ok(pair);
    }
    let Some(dev_run) = &output.dev_run else {
        return Err(LinkError {
            function: Some(*key),
            refusal: None,
            what: "its code counts calls in tier-up counter cells, and the image is not a dev run: counting code is \
                   only for runs the daemon tiers"
                .into(),
        });
    };
    let capacity = capacity(output);
    if state.pairs.given() + state.pairs.placed() >= capacity {
        return Err(LinkError {
            function: Some(*key),
            refusal: None,
            // alloc: error path
            what: format!(
                "its code counts calls in tier-up counter cells, and the dev run's {} bytes of cells hold {capacity} \
                 pairs, all given out: link it with more cells",
                dev_run.cells
            ),
        });
    }
    Ok(if placed { state.pairs.give_placed(id, capacity) } else { state.pairs.give(id) })
}

/// The byte offset of pair `pair` from the cells reservation's start.
#[inline]
pub(crate) fn offset(pair: u32) -> u64 {
    PAIR * u64::from(pair)
}

/// Whether code reaches its counter cells by a relocation of this kind: what stencil glue and
/// Cranelift write for a colocated data symbol (x86-64 `rip`-relative, AArch64 `ADRP` with an
/// `ADD` or a 64-bit `LDR`/`STR` of the page offset).
pub(crate) fn reaches(apply: Apply) -> bool {
    matches!(apply, Apply::X86Branch32 | Apply::A64Page21 | Apply::A64Lo12 { .. })
}
