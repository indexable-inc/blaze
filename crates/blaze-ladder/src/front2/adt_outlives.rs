//! OUTLIVES OF A TYPE'S PARTS: an ADT's inferred outlives requirements (RFC 2093, "Infer
//! `T: 'x` outlives requirements on structs") and the bounds a fn's signature types imply
//! (RFC 1214, "a fn assumes its argument and return types well-formed"; an impl its header's
//! types). The sans-IO walk over a type laid out over its region column (adt_variance.rs
//! [`LaidNode`]); the fixed point over an ADT group and its record are adt_facts.rs's.
//!
//! The rules (rustc_hir_analysis outlives/implicit_infer.rs and rustc_type_ir outlives.rs are
//! the reference implementation, read for the rules):
//!
//! - every `&'r T` anywhere in the type requires `T: 'r`;
//! - every ADT `S<..>` in the type requires its own requirements, declared and inferred,
//!   instantiated with its arguments (`struct S<'a, T: 'a>` used as `S<'c, &'b X>` requires
//!   `&'b X: 'c`);
//! - `X: 'r` is split into its components: each region in `X` outlives `'r` (`'b: 'c`), each
//!   type parameter in `X` outlives `'r` (`T: 'c`); slices, arrays, tuples, ADT arguments,
//!   trait objects and fn pointers are walked componentwise; a projection or an opaque type is
//!   one component this record does not hold (fewer bounds: an ADT's `<T as Tr>::A: 'a`);
//! - a region a binder inside the type binds is no component and no target (the subject's
//!   other components still are);
//! - in an ADT (`Mode::Adt`) `'static` is no target (RFC 2093: `T: 'static` is never
//!   inferred; it stays an ADT's own declared bound); in a signature (`Mode::Fn`) it is.

use blaze_own::{AVec, Alloc};
use blaze_typeck::regions::Region;

use super::adt_variance::{LaidNode, child_of, is_late};

/// Whose requirements a walk computes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// An ADT's fields: its inferred requirements.
    Adt,
    /// A fn's signature types or an impl's header types: the bounds they imply.
    Fn,
}

/// Why requirements could not be taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutlivesRefusal {
    /// An ADT's region parameter `index` where it has `count`, or a type argument `index` it
    /// was not given.
    ParamPast { adt: u128, index: u32, count: u32 },
    /// The type names the ADT `adt` whose requirements are not known (its facts refused, or
    /// not asked): never guessed (rule 3).
    Unsolved { adt: u128 },
    /// The fixed point did not settle in `rounds` rounds (a bug, named).
    Rounds { rounds: u32 },
}

/// An ADT's requirements, in its own numbering: `(type parameter, region)` (`T: 'r`) and
/// `(long, short)` (`'long: 'short`).
#[derive(Clone, Copy, Debug)]
pub struct Requirements<'r> {
    pub params: &'r [(u32, Region)],
    pub regions: &'r [(Region, Region)],
}

/// The requirements of the ADTs a walk meets.
pub trait Required {
    fn required(&self, adt: u128) -> Result<Requirements<'_>, OutlivesRefusal>;
}

/// A set of requirements, sorted, no repeats.
pub struct OutlivesBuf<'a> {
    pub params: AVec<'a, (u32, Region)>,
    pub regions: AVec<'a, (Region, Region)>,
}

impl<'a> OutlivesBuf<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Self {
        OutlivesBuf { params: AVec::new_in(alloc), regions: AVec::new_in(alloc) }
    }

    pub fn clear(&mut self) {
        self.params.clear();
        self.regions.clear();
    }

    /// `T_p: r`.
    pub fn param(&mut self, p: u32, r: Region) {
        if let Err(at) = self.params.binary_search(&(p, r)) {
            self.params.insert(at, (p, r));
        }
    }

    /// `long: short`; a trivial one (`'x: 'x`, `'static: 'x`) is not kept.
    pub fn region(&mut self, long: Region, short: Region) {
        if long == short || long == Region::Static {
            return;
        }
        if let Err(at) = self.regions.binary_search(&(long, short)) {
            self.regions.insert(at, (long, short));
        }
    }
}

/// Every requirement of one type laid out as `laid` over `col` (a field's type, a signature's,
/// an impl header's): each reference's and each ADT's, anywhere in it, into `out`.
pub fn requirements_of(laid: &[LaidNode], col: &[Region], mode: Mode, req: &dyn Required, out: &mut OutlivesBuf<'_>) -> Result<(), OutlivesRefusal> {
    for (i, n) in laid.iter().enumerate() {
        match n.node {
            blaze_typeck::PNode::Ref(..) => {
                let Some(&r) = col.get(n.start as usize) else { continue };
                if let Some(c) = child_of(laid, i, 0) {
                    outlives(laid, col, c, r, mode, out);
                }
            }
            blaze_typeck::PNode::Adt(k, _) => apply(laid, col, i, k.0, mode, req, out)?,
            _ => {}
        }
    }
    Ok(())
}

/// A declared `subject: region` (a where-clause or a parameter's bound) of an ADT, its subject
/// laid as `laid` over `col`: its components outlive `region` ('static kept: it is declared).
pub fn declared(laid: &[LaidNode], col: &[Region], region: Region, out: &mut OutlivesBuf<'_>) {
    if !laid.is_empty() {
        outlives(laid, col, 0, region, Mode::Fn, out);
    }
}

/// ADT `adt`'s requirements at node `at`, instantiated with its slots and arguments there.
fn apply(laid: &[LaidNode], col: &[Region], at: usize, adt: u128, mode: Mode, req: &dyn Required, out: &mut OutlivesBuf<'_>) -> Result<(), OutlivesRefusal> {
    let Some(n) = laid.get(at) else { return Ok(()) };
    let r = req.required(adt)?;
    if r.params.is_empty() && r.regions.is_empty() {
        return Ok(());
    }
    let own = col.get(n.start as usize..(n.start + n.own) as usize).unwrap_or(&[]);
    let inst = |x: Region| -> Result<Region, OutlivesRefusal> {
        match x {
            Region::Param(i) => own.get(i as usize).copied().ok_or(OutlivesRefusal::ParamPast { adt, index: i, count: n.own }),
            other => Ok(other),
        }
    };
    for &(p, target) in r.params.iter() {
        let target = inst(target)?;
        let Some(c) = child_of(laid, at, p) else {
            return Err(OutlivesRefusal::ParamPast { adt, index: p, count: n.kids });
        };
        outlives(laid, col, c, target, mode, out);
    }
    for &(long, short) in r.regions.iter() {
        let (long, short) = (inst(long)?, inst(short)?);
        if is_late(long) || !target_taken(short, mode) {
            continue;
        }
        out.region(long, short);
    }
    Ok(())
}

/// Whether `target` can be required in `mode`.
const fn target_taken(target: Region, mode: Mode) -> bool {
    match target {
        Region::Param(_) => true,
        Region::Static => matches!(mode, Mode::Fn),
        Region::Late { .. } => false,
    }
}

/// The subtree at `at` outlives `target`: each of its components does.
fn outlives(laid: &[LaidNode], col: &[Region], at: usize, target: Region, mode: Mode, out: &mut OutlivesBuf<'_>) {
    if !target_taken(target, mode) {
        return;
    }
    let Some(n) = laid.get(at) else { return };
    // A region a binder inside the subject binds (`Region::Late`: the subject is laid from a
    // type's root, so every binder of one is inside it; a fn's own late-bound lifetimes are
    // `Param`s, and a clause under its own `for<..>` is not given) is no component; the
    // subject's other components still are (`&'b for<'x> fn(&'x T)` requires `T: 'b`).
    let mut d = at;
    while d < n.after as usize {
        let Some(m) = laid.get(d) else { return };
        match m.node {
            // A projection, an opaque type: one component this record does not hold.
            blaze_typeck::PNode::Alias { .. } | blaze_typeck::PNode::Opaque(..) => {
                d = (m.after as usize).max(d + 1);
                continue;
            }
            blaze_typeck::PNode::Param(p) => out.param(p, target),
            _ => {}
        }
        for &r in col.get(m.start as usize..(m.start + m.own) as usize).unwrap_or(&[]).iter() {
            if let Region::Param(_) = r {
                out.region(r, target);
            }
        }
        d += 1;
    }
}
