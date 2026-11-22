//! The soft-float routines a code record may call (port27/tempo-run/INTRINSICS.md 3.5, P5b):
//! the caller's table of where each one is among its function references ([`RoutinePlaces`]),
//! and what the emitter answers: the routines the record calls ([`Called`]).
//!
//! WHICH routines a function calls is an output of emitting it: only blaze-lir's legalize maps
//! an `f128` (or `f16`) operation to its compiler_builtins routine, and the record emitter
//! runs it (record.rs `emit_record_up_to`). So the caller hands ONE table covering every
//! routine its program's compiler_builtins exports (blaze-ladder front2/builtins_shim.rs), the
//! emitter adds a reference row for each bound routine to its copy of the function, legalizes,
//! and returns [`Called`]: the caller declares exactly those as the record's callees. A caller
//! with no table passes [`RoutinePlaces::NONE`]: an `f128` operation is then refused by name
//! (`legalize: the routine `__addtf3` is not bound`).

use blaze_lir::func::{RefId, RefKind, RefRec};
use blaze_lir::legalize::{self, LegalizeError, QuadRoutines, SoftFloat};
use blaze_lir::{Func, View};
use blaze_own::Alloc;

/// The record emitter's first step (record.rs `emit_record_up_to`): a function holding a vector,
/// `f16` or `f128` value, copied into `alloc` with a reference row per routine `routines` binds,
/// legalized, and the routines it now calls; `None` (and no routine) for a function holding none,
/// which is emitted as it is.
pub(crate) fn legalized<'a>(
    func: View<'_>,
    routines: &RoutinePlaces,
    alloc: Alloc<'a>,
) -> Result<(Option<Func<Alloc<'a>>>, Called), LegalizeError> {
    if !legalize::needed(&func)? {
        return Ok((None, Called::NONE));
    }
    let mut owned = func.to_func_in(alloc);
    let rows = add_rows(routines, &mut owned)?;
    let done = legalize::legalize(&mut owned, &rows, alloc)?;
    Ok((Some(owned), Called::of_bits(done.routines)))
}

/// Whether `func` holds an `f128` value: the one type whose operations become routine calls
/// (an `f16` one is widened to `f32` in place; a vector is scalarized). The Fast tier legalizes
/// exactly such a function before composing (compose/fast.rs) and keeps every other as it is:
/// its `f16` and vector homes are its own (compose/analysis.rs `class_of`), while an `f128`
/// has no class there and no template.
pub(crate) fn holds_quad(func: &View<'_>) -> bool {
    func.tys.iter().any(|&bits| {
        let ty = blaze_lir::Ty(bits);
        matches!(ty.form(), blaze_lir::ty::Form::Scalar) && matches!(ty.elem(), blaze_lir::Scalar::F128)
    })
}

/// The routine table: blaze-tier's, the seam's input (`blaze_tier::Names::routines`).
pub use blaze_tier::RoutinePlaces;

/// How many routines there are: [`SoftFloat::ALL`]'s length.
const COUNT: usize = SoftFloat::ALL.len();

/// No row: the routine is not bound.
const UNBOUND: u32 = u32::MAX;

/// Adds a `RefKind::Func` row to `func` for each routine `routines` binds (the rows legalize's
/// calls name) and gives legalize's view of them.
fn add_rows(routines: &RoutinePlaces, func: &mut Func<Alloc<'_>>) -> Result<Rows, LegalizeError> {
    let mut rows = Rows { rows: [UNBOUND; COUNT] };
    for r in SoftFloat::ALL {
        let Some(place) = routines.place(r) else { continue };
        let row = u32::try_from(func.refs.len()).map_err(|_| LegalizeError::Full { table: "references" })?;
        if row == UNBOUND {
            return Err(LegalizeError::Full { table: "references" });
        }
        func.refs.push(RefRec { place, kind: RefKind::Func as u32 });
        rows.rows[r as usize] = row;
    }
    Ok(rows)
}

/// The function's own reference row of each bound routine: what legalize asks
/// ([`QuadRoutines`]).
pub(crate) struct Rows {
    rows: [u32; COUNT],
}

impl QuadRoutines for Rows {
    fn routine(&self, r: SoftFloat) -> Option<RefId> {
        match self.rows[r as usize] {
            UNBOUND => None,
            row => Some(RefId(row)),
        }
    }
}

/// The routines a record calls: bit [`SoftFloat::bit`] of each (legalize's
/// `Legalized::routines`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Called(u32);

impl Called {
    /// No routine called (a function legalize did not rewrite, or one with no `f128` work).
    pub const NONE: Called = Called(0);

    pub(crate) const fn of_bits(bits: u32) -> Called {
        Called(bits)
    }

    /// The bits, [`SoftFloat::bit`] of each routine called.
    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, r: SoftFloat) -> bool {
        self.0 & r.bit() != 0
    }

    /// Each routine called, in [`SoftFloat::ALL`]'s order (deterministic, rule 25).
    pub fn iter(self) -> impl Iterator<Item = SoftFloat> {
        SoftFloat::ALL.into_iter().filter(move |&r| self.contains(r))
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_lir::builder::{Param, Variadic};
    use blaze_lir::func::Conv;
    use blaze_lir::legalize::{LegalizeError, SoftFloat};
    use blaze_lir::{BlockKind, Builder, Flags, Full, Func, Op, Ty};
    use blaze_own::Alloc;

    use blaze_lir::func::RefKind;

    use super::{Called, RoutinePlaces, legalized};

    /// `fn(a: f128, b: f128) -> f128 { a + b }`, with no reference of its own.
    fn add_f128() -> Func<Global> {
        let mut b = Builder::<Global, Full>::new_in(64, Global);
        let q = Param::of(Ty::F128);
        let sig = b.sig(Conv::C, Variadic::No, &[q, q], &[q]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let (x, y) = (b.param(Ty::F128), b.param(Ty::F128));
        let sum = b.binary(Op::Fadd, Flags::NONE, x, y);
        b.ret(&[sum]);
        b.finish_checked().expect("the f128 add is valid LIR")
    }

    /// INTRINSICS.md 3.5: "every f128 op becomes a call to the pinned compiler_builtins'
    /// `#[no_mangle]` routines (`__addtf3`, ...)". With all 24 routines bound (routine `k` at
    /// function reference place `100 + k`), the record emitter's first step makes a function
    /// holding one `f128` add call `__addtf3` and nothing else of the 24, says so, and the call
    /// names a reference row at `__addtf3`'s place; with none bound, the add is refused naming
    /// `__addtf3`. (Codegen of the call is not reached: the Reference tier takes no `f128`
    /// signature yet, pass.rs `check_sig`.)
    ///
    /// Prints: `test routines::tests::one_f128_add_calls_addtf3_only ... ok`
    #[test]
    fn one_f128_add_calls_addtf3_only() {
        let func = add_f128();
        let mut places = RoutinePlaces::NONE;
        for (k, r) in SoftFloat::ALL.into_iter().enumerate() {
            places.bind(r, 100 + k as u32);
        }
        let heap = blaze_own::testing::heap();
        let alloc: Alloc<'_> = heap.alloc();
        let (legal, called) = legalized(func.view(), &places, alloc).unwrap_or_else(|why| panic!("an f128 add with every routine bound legalizes: {why}"));
        assert_eq!(called.iter().collect::<std::vec::Vec<_>>(), [SoftFloat::AddTf3], "{called:?}");
        assert_eq!(called, Called::of_bits(SoftFloat::AddTf3.bit()));
        let legal = legal.expect("an f128 add is rewritten");
        let view = legal.view();
        let add_place = 100 + SoftFloat::AddTf3 as u32;
        let text = blaze_lir::print(view);
        let row = view
            .refs
            .iter()
            .position(|r| r.kind == RefKind::Func as u32 && r.place == add_place)
            .unwrap_or_else(|| panic!("no reference row at __addtf3's place {add_place}:\n{text}"));
        assert!(text.lines().any(|l| l.contains("= call") && l.contains(&format!("ref{row}"))), "no call of ref{row} (__addtf3):\n{text}");
        assert!(!text.contains("fadd"), "an `fadd` is left:\n{text}");

        let refused = legalized(func.view(), &RoutinePlaces::NONE, alloc).map(|_| ());
        assert_eq!(refused, Err(LegalizeError::NoRoutine(SoftFloat::AddTf3)));
    }
}
