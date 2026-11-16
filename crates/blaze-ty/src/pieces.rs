//! The intern path's pieces one at a time, for `examples/cost.rs` to time (rule 16): not an
//! interface. Each function does one piece and nothing else.

use crate::hash;
use crate::index::Probe;
use crate::kind::ty;
use crate::tys::Tys;
use crate::Ty;

/// The row hash alone.
#[inline(always)]
pub fn row_hash(kind: u8, a: u32, b: u32) -> u64 {
    hash::row(kind, a, b)
}

/// The type index's probe for `hash` with a compare that never matches: the group loads
/// and tag matches alone, ending at the first empty slot.
#[inline(always)]
pub fn probe_only(tys: &Tys<'_>, hash: u64) -> bool {
    matches!(tys.ty.index.probe(hash, |_| false), Probe::Found(_))
}

/// The lookup a hit does: hash, probe, compare of the three columns; no insert, no flags.
#[inline(always)]
pub fn find_slice(tys: &Tys<'_>, elem: Ty) -> Option<Ty> {
    match tys.ty.find(ty::SLICE, elem.node().bits(), 0) {
        Probe::Found(id) => Some(Ty(id)),
        Probe::Vacant(_) => None,
    }
}
