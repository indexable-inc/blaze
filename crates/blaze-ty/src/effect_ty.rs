//! First-Class Function-Type Effects & Effect Polymorphism (`blaze-ty::effect_ty`).
//!
//! Promotes Blaze's positive effect lists (`expect(panic, alloc, block, diverge)`,
//! `docs/v2/plan/design/effects.md`) from item-only attributes into the function type system so
//! `fn()` pointers, closures, and `dyn Fn` trait objects carry exact effect sets and support
//! higher-order effect polymorphism without degrading to "may do anything".
//!
//! - [`EffectSet`]: 4-bit exact bitset (`PANIC | ALLOC | BLOCK | DIVERGE`), packed into bits
//!   `10..15` of [`crate::FnHeader`] with zero size or ABI overhead (`FnHeader::RUST` stays `0`).
//! - **Effect subtyping**: `e1 ⊆ e2` lets a purer `fn() expect()` coerce to `fn() expect(alloc)`,
//!   and rejects passing an allocating callback where `expect()` is required.
//! - **Effect polymorphism**: [`EffectRow`] models `expect(base | e)` on higher-order functions
//!   and trait methods (`fn map<F: Fn(A) -> B expect(e)>(f: F) expect(e)`).

use core::fmt;

use crate::tys::FnHeader;

/// 4-bit exact effect set for Blaze functions, fn pointers, and `dyn Fn` trait objects.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct EffectSet(u8);

impl EffectSet {
    /// No effects (`expect()`): pure, non-panicking, non-allocating, non-blocking, terminating.
    pub const PURE: EffectSet = EffectSet(0);
    /// May panic or abort (`panic`).
    pub const PANIC: EffectSet = EffectSet(1 << 0);
    /// May call the global allocator (`alloc`).
    pub const ALLOC: EffectSet = EffectSet(1 << 1);
    /// May block the current thread (`block`).
    pub const BLOCK: EffectSet = EffectSet(1 << 2);
    /// May not return: unbounded loop or recursion (`diverge`).
    pub const DIVERGE: EffectSet = EffectSet(1 << 3);
    /// Unconstrained ("may do anything": unmarked Rust code / foreign FFI).
    pub const ALL: EffectSet = EffectSet(0b1111);

    /// Constructs an [`EffectSet`] from its 4 low bits.
    #[inline]
    pub const fn from_bits(bits: u8) -> EffectSet {
        EffectSet(bits & 0b1111)
    }

    /// Raw 4-bit mask (`0..=15`).
    #[inline]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Parses one Blaze effect keyword (`"panic"`, `"alloc"`, `"block"`, `"diverge"`).
    pub fn from_word(word: &str) -> Option<EffectSet> {
        match word {
            "panic" => Some(Self::PANIC),
            "alloc" => Some(Self::ALLOC),
            "block" => Some(Self::BLOCK),
            "diverge" => Some(Self::DIVERGE),
            _ => None,
        }
    }

    /// Returns `true` iff this set has no effects (`expect()`).
    #[inline]
    pub const fn is_pure(self) -> bool {
        self.0 == 0
    }

    /// Returns `true` iff every bit of `flag` is present in `self`.
    #[inline]
    pub const fn contains(self, flag: EffectSet) -> bool {
        (self.0 & flag.0) == flag.0
    }

    /// Set union (`self ∪ other`).
    #[inline]
    pub const fn union(self, other: EffectSet) -> EffectSet {
        EffectSet(self.0 | other.0)
    }

    /// Set intersection (`self ∩ other`).
    #[inline]
    pub const fn intersection(self, other: EffectSet) -> EffectSet {
        EffectSet(self.0 & other.0)
    }

    /// Set difference (`self \ other`): bits in `self` not in `other`.
    #[inline]
    pub const fn difference(self, other: EffectSet) -> EffectSet {
        EffectSet(self.0 & !other.0)
    }

    /// Effect subtyping (`self ⊆ allowed`): a function with `self` may be called or coerced where
    /// `allowed` is permitted.
    #[inline]
    pub const fn is_subset_of(self, allowed: EffectSet) -> bool {
        (self.0 & !allowed.0) == 0
    }

    /// Canonical static text spelling of this effect list (`expect()`, `expect(panic, alloc)`, ...).
    pub const fn canonical_label(self) -> &'static str {
        match self.0 & 0b1111 {
            0b0000 => "expect()",
            0b0001 => "expect(panic)",
            0b0010 => "expect(alloc)",
            0b0011 => "expect(panic, alloc)",
            0b0100 => "expect(block)",
            0b0101 => "expect(panic, block)",
            0b0110 => "expect(alloc, block)",
            0b0111 => "expect(panic, alloc, block)",
            0b1000 => "expect(diverge)",
            0b1001 => "expect(panic, diverge)",
            0b1010 => "expect(alloc, diverge)",
            0b1011 => "expect(panic, alloc, diverge)",
            0b1100 => "expect(block, diverge)",
            0b1101 => "expect(panic, block, diverge)",
            0b1110 => "expect(alloc, block, diverge)",
            _ => "expect(panic, alloc, block, diverge)",
        }
    }
}

impl fmt::Display for EffectSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.canonical_label())
    }
}

/// Polymorphic effect variable (`e`) on a higher-order function or trait signature.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EffectVar(pub u32);

/// An effect row on a function signature or callback bound: either a concrete [`EffectSet`] or a
/// polymorphic row `base | var` (`fn map<F: Fn(A) -> B expect(e)>(f: F) expect(alloc | e)`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EffectRow {
    /// Fixed concrete effect set (`expect(alloc)`).
    Concrete(EffectSet),
    /// Polymorphic row: fixed `base` effects plus caller-supplied effect variable `var`.
    Poly { base: EffectSet, var: EffectVar },
}

impl EffectRow {
    /// Pure concrete row (`expect()`).
    pub const PURE: EffectRow = EffectRow::Concrete(EffectSet::PURE);

    /// Fixed concrete row.
    #[inline]
    pub const fn concrete(set: EffectSet) -> Self {
        Self::Concrete(set)
    }

    /// Polymorphic row `base | var`.
    #[inline]
    pub const fn poly(base: EffectSet, var: EffectVar) -> Self {
        Self::Poly { base, var }
    }

    /// Fixed base effects always present regardless of variable instantiation.
    #[inline]
    pub const fn base(self) -> EffectSet {
        match self {
            Self::Concrete(s) | Self::Poly { base: s, .. } => s,
        }
    }

    /// Instantiates `var` with `arg` to produce a concrete [`EffectSet`].
    #[inline]
    pub const fn instantiate(self, target: EffectVar, arg: EffectSet) -> Self {
        match self {
            Self::Concrete(s) => Self::Concrete(s),
            Self::Poly { base, var } if var.0 == target.0 => Self::Concrete(base.union(arg)),
            other => other,
        }
    }

    /// Resolves this row to a concrete [`EffectSet`] given a binding for any effect variable.
    #[inline]
    pub fn resolve(self, lookup: impl FnOnce(EffectVar) -> EffectSet) -> EffectSet {
        match self {
            Self::Concrete(s) => s,
            Self::Poly { base, var } => base.union(lookup(var)),
        }
    }

    /// Checks whether `self` is guaranteed to be a sub-effect of `allowed` for all instantiations
    /// of any free effect variables.
    #[inline]
    pub const fn is_subrow_of(self, allowed: EffectRow) -> bool {
        match (self, allowed) {
            (Self::Concrete(a), Self::Concrete(b)) => a.is_subset_of(b),
            (Self::Concrete(a), Self::Poly { base, .. }) => a.is_subset_of(base),
            (Self::Poly { base: a, var: va }, Self::Poly { base: b, var: vb }) => {
                va.0 == vb.0 && a.is_subset_of(b)
            }
            (Self::Poly { .. }, Self::Concrete(b)) => b.bits() == EffectSet::ALL.bits(),
        }
    }
}

/// Errors produced by exact effect checking and function-pointer / `dyn Fn` effect subtyping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EffectError {
    /// Body or callee performs effects missing from the declared `expect(..)` list.
    UndeclaredEffects { missing: EffectSet },
    /// Declared `expect(..)` list names effects the body is proved not to perform (stale mark).
    UnfulfilledEffects { extra: EffectSet },
    /// Coercing a `fn(...)` pointer or `dyn Fn` would drop required effect guarantees.
    FnPtrEffectMismatch { expected: EffectSet, found: EffectSet },
    /// Function pointer ABI, safety, or C-variadic flag disagrees.
    FnHeaderIncompatible,
}

impl fmt::Display for EffectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            EffectError::UndeclaredEffects { missing } => {
                write!(f, "undeclared effects: add '{missing}' to `expect(..)` or handle them")
            }
            EffectError::UnfulfilledEffects { extra } => {
                write!(f, "unfulfilled effects '{extra}': remove them from `expect(..)`")
            }
            EffectError::FnPtrEffectMismatch { expected, found } => {
                write!(f, "function type effect mismatch: expected subset of '{expected}', found '{found}'")
            }
            EffectError::FnHeaderIncompatible => {
                write!(f, "incompatible function pointer header (ABI, safety, or variadics)")
            }
        }
    }
}

/// Enforces Blaze's exact positive-list rule on a function's own `expect(..)` declaration:
/// every actual effect must be declared, and every declared effect must actually occur.
pub const fn check_exact_effects(declared: EffectSet, actual: EffectSet) -> Result<(), EffectError> {
    let missing = actual.difference(declared);
    if !missing.is_pure() {
        return Err(EffectError::UndeclaredEffects { missing });
    }
    let extra = declared.difference(actual);
    if !extra.is_pure() {
        return Err(EffectError::UnfulfilledEffects { extra });
    }
    Ok(())
}

/// Checks whether a function pointer with header `src` can coerce to a function pointer with header
/// `dst`: ABI/variadics/bound_vars must match, safe may coerce to unsafe, and `src`'s effects must
/// be a subset of `dst`'s allowed effects (`fn() expect() <: fn() expect(alloc)`).
pub const fn fn_ptr_coerces(src: FnHeader, dst: FnHeader) -> Result<(), EffectError> {
    if src.abi() != dst.abi()
        || src.c_variadic() != dst.c_variadic()
        || src.bound_vars() != dst.bound_vars()
        || (src.is_unsafe() && !dst.is_unsafe())
    {
        return Err(EffectError::FnHeaderIncompatible);
    }
    let src_eff = src.effective_effects();
    let dst_eff = dst.effective_effects();
    if !src_eff.is_subset_of(dst_eff) {
        return Err(EffectError::FnPtrEffectMismatch { expected: dst_eff, found: src_eff });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::heap;

    use super::*;
    use crate::cx::TyCx;
    use crate::ids::{Ty, UintTy};
    use crate::print::{DefaultNames, Out, print};

    struct StrOut<'a>(blaze_own::String<blaze_own::Alloc<'a>>);
    impl Out for StrOut<'_> {
        fn str(&mut self, s: &str) {
            self.0.push_str(s);
        }
    }

    #[test]
    fn fn_header_packs_effects_without_changing_rust_default() {
        // Ordinary unmarked Rust fn header stays bit-identical (`0`) and defaults to ALL effects.
        assert_eq!(FnHeader::RUST.0, 0);
        assert_eq!(FnHeader::RUST.declared_effects(), None);
        assert_eq!(FnHeader::RUST.effective_effects(), EffectSet::ALL);

        // Explicit pure `expect()` packs into bits 10..15 cleanly without disturbing ABI/bound_vars.
        let h = FnHeader::new(3, true, false, 2).with_effects(EffectSet::PURE);
        assert_eq!(h.abi(), 3);
        assert!(h.is_unsafe());
        assert!(!h.c_variadic());
        assert_eq!(h.bound_vars(), 2);
        assert_eq!(h.declared_effects(), Some(EffectSet::PURE));
        assert_eq!(h.effective_effects(), EffectSet::PURE);

        let alloc_panic = EffectSet::ALLOC.union(EffectSet::PANIC);
        let h2 = FnHeader::RUST.with_effects(alloc_panic);
        assert_eq!(h2.declared_effects(), Some(alloc_panic));
        assert_eq!(h2.effective_effects(), alloc_panic);
    }

    #[test]
    fn fn_ptr_effect_subtyping_and_exact_check() {
        let pure_fn = FnHeader::RUST.with_effects(EffectSet::PURE);
        let alloc_fn = FnHeader::RUST.with_effects(EffectSet::ALLOC);
        let all_fn = FnHeader::RUST; // unmarked Rust `fn()`

        // Pure coerces to alloc and to unmarked Rust `fn()`.
        assert!(fn_ptr_coerces(pure_fn, alloc_fn).is_ok());
        assert!(fn_ptr_coerces(pure_fn, all_fn).is_ok());
        assert!(fn_ptr_coerces(alloc_fn, all_fn).is_ok());

        // Allocating or unmarked `fn()` cannot coerce to pure `fn() expect()`.
        assert_eq!(
            fn_ptr_coerces(alloc_fn, pure_fn).unwrap_err(),
            EffectError::FnPtrEffectMismatch { expected: EffectSet::PURE, found: EffectSet::ALLOC }
        );
        assert_eq!(
            fn_ptr_coerces(all_fn, alloc_fn).unwrap_err(),
            EffectError::FnPtrEffectMismatch { expected: EffectSet::ALLOC, found: EffectSet::ALL }
        );

        // Exact positive-list checking: under-declaring and stale over-declaring both fail.
        assert!(check_exact_effects(EffectSet::ALLOC, EffectSet::ALLOC).is_ok());
        assert_eq!(
            check_exact_effects(EffectSet::PURE, EffectSet::PANIC).unwrap_err(),
            EffectError::UndeclaredEffects { missing: EffectSet::PANIC }
        );
        assert_eq!(
            check_exact_effects(EffectSet::PANIC.union(EffectSet::ALLOC), EffectSet::ALLOC).unwrap_err(),
            EffectError::UnfulfilledEffects { extra: EffectSet::PANIC }
        );
    }

    #[test]
    fn polymorphic_effect_row_for_higher_order_callbacks() {
        // fn map<F: Fn(u32) -> u32 expect(e)>(f: F) expect(alloc | e)
        let e = EffectVar(0);
        let map_row = EffectRow::poly(EffectSet::ALLOC, e);

        // Passing a pure callback -> `map` has only `alloc`.
        assert_eq!(map_row.instantiate(e, EffectSet::PURE), EffectRow::Concrete(EffectSet::ALLOC));
        // Passing a panicking callback -> `map` has `panic | alloc`.
        assert_eq!(
            map_row.instantiate(e, EffectSet::PANIC),
            EffectRow::Concrete(EffectSet::ALLOC.union(EffectSet::PANIC))
        );

        // Subrow check: `expect(e)` <: `expect(alloc | e)`, not vice versa.
        let cb_row = EffectRow::poly(EffectSet::PURE, e);
        assert!(cb_row.is_subrow_of(map_row));
        assert!(!map_row.is_subrow_of(cb_row));
    }

    #[test]
    fn fn_ptr_prints_declared_effects_and_keeps_rust_unchanged() {
        let h = heap();
        let a = h.alloc();
        let mut cx = TyCx::new_in(a);
        let u32_ty = Ty::uint(UintTy::U32);
        let sig = cx.tys.mk_list(&[u32_ty.arg(), u32_ty.arg()]);

        let rust_fn = cx.tys.fn_ptr(sig, FnHeader::RUST);
        let pure_fn = cx.tys.fn_ptr(sig, FnHeader::RUST.with_effects(EffectSet::PURE));
        let alloc_fn = cx.tys.fn_ptr(sig, FnHeader::RUST.with_effects(EffectSet::ALLOC));

        let mut out = StrOut(blaze_own::String::new_in(a));
        print(&cx.tys, rust_fn.node(), &mut DefaultNames, &mut out);
        assert_eq!(out.0.as_str(), "fn(u32) -> u32");

        out.0.clear();
        print(&cx.tys, pure_fn.node(), &mut DefaultNames, &mut out);
        assert_eq!(out.0.as_str(), "fn(u32) -> u32 expect()");

        out.0.clear();
        print(&cx.tys, alloc_fn.node(), &mut DefaultNames, &mut out);
        assert_eq!(out.0.as_str(), "fn(u32) -> u32 expect(alloc)");
    }
}
