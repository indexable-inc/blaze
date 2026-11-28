//! First-Class Function-Type Effects & Effect Polymorphism (`blaze-ty::effect_ty`).
//!
//! Promotes Blaze's positive effect lists (`expect(panic, alloc, block, diverge)`,
//! `docs/v2/plan/design/effects.md`) from item-only attributes into the function type system so
//! `fn()` pointers, closures, and `dyn Fn` trait objects carry exact effect sets and support
//! higher-order effect polymorphism without degrading to "may do anything".
//!
//! - [`EffectSet`]: 5-bit exact bitset (`PANIC | ALLOC | BLOCK | DIVERGE | SUSPEND`), packed into
//!   bits `10..=15` of [`crate::FnHeader`] with zero size or ABI overhead (`FnHeader::RUST` stays `0`).
//! - **Effect subtyping**: `e1 ⊆ e2` lets a purer `fn() expect()` coerce to `fn() expect(alloc)` or
//!   `fn() expect(suspend)`, and rejects passing a suspending or allocating callback where `expect()`
//!   is required.
//! - **Effect polymorphism**: [`EffectRow`] and multi-variable [`EffectRowSet`] model
//!   `expect(base | e1 | e2)` on higher-order functions and trait methods
//!   (`fn map<F: Fn(A) -> B expect(e)>(f: F) expect(e)`), removing `sync`/`async` function coloring
//!   across `bstd`.
//! - **Tail-resumptive specialization**: [`LoweringMode`] elides coroutine state-machine overhead
//!   (`PlainFn`) whenever an instantiated effect set omits `SUSPEND`, lowering to
//!   `MachineCoroutine` only when `SUSPEND` is present.

use core::cmp::Ordering;
use core::fmt;

use crate::tys::FnHeader;

/// 5-bit exact effect set for Blaze functions, fn pointers, and `dyn Fn` trait objects.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct EffectSet(u8);

impl EffectSet {
    /// No effects (`expect()`): pure, non-panicking, non-allocating, non-blocking, terminating, non-suspending.
    pub const PURE: EffectSet = EffectSet(0);
    /// May panic or abort (`panic`).
    pub const PANIC: EffectSet = EffectSet(1 << 0);
    /// May call the global allocator (`alloc`).
    pub const ALLOC: EffectSet = EffectSet(1 << 1);
    /// May block the current thread (`block`).
    pub const BLOCK: EffectSet = EffectSet(1 << 2);
    /// May not return: unbounded loop or recursion (`diverge`).
    pub const DIVERGE: EffectSet = EffectSet(1 << 3);
    /// May suspend to a lexical algebraic effect handler / sans-IO driver (`suspend`).
    pub const SUSPEND: EffectSet = EffectSet(1 << 4);
    /// Unconstrained ("may do anything": unmarked Rust code / foreign FFI).
    pub const ALL: EffectSet = EffectSet(0b1_1111);

    /// Constructs an [`EffectSet`] from its 5 low bits.
    #[inline]
    pub const fn from_bits(bits: u8) -> EffectSet {
        EffectSet(bits & 0b1_1111)
    }

    /// Raw 5-bit mask (`0..=31`).
    #[inline]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Parses one Blaze effect keyword (`"panic"`, `"alloc"`, `"block"`, `"diverge"`, `"suspend"`).
    pub fn from_word(word: &str) -> Option<EffectSet> {
        match word {
            "panic" => Some(Self::PANIC),
            "alloc" => Some(Self::ALLOC),
            "block" => Some(Self::BLOCK),
            "diverge" => Some(Self::DIVERGE),
            "suspend" => Some(Self::SUSPEND),
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
        match self.0 & 0b1_1111 {
            0b0_0000 => "expect()",
            0b0_0001 => "expect(panic)",
            0b0_0010 => "expect(alloc)",
            0b0_0011 => "expect(panic, alloc)",
            0b0_0100 => "expect(block)",
            0b0_0101 => "expect(panic, block)",
            0b0_0110 => "expect(alloc, block)",
            0b0_0111 => "expect(panic, alloc, block)",
            0b0_1000 => "expect(diverge)",
            0b0_1001 => "expect(panic, diverge)",
            0b0_1010 => "expect(alloc, diverge)",
            0b0_1011 => "expect(panic, alloc, diverge)",
            0b0_1100 => "expect(block, diverge)",
            0b0_1101 => "expect(panic, block, diverge)",
            0b0_1110 => "expect(alloc, block, diverge)",
            0b0_1111 => "expect(panic, alloc, block, diverge)",
            0b1_0000 => "expect(suspend)",
            0b1_0001 => "expect(panic, suspend)",
            0b1_0010 => "expect(alloc, suspend)",
            0b1_0011 => "expect(panic, alloc, suspend)",
            0b1_0100 => "expect(block, suspend)",
            0b1_0101 => "expect(panic, block, suspend)",
            0b1_0110 => "expect(alloc, block, suspend)",
            0b1_0111 => "expect(panic, alloc, block, suspend)",
            0b1_1000 => "expect(diverge, suspend)",
            0b1_1001 => "expect(panic, diverge, suspend)",
            0b1_1010 => "expect(alloc, diverge, suspend)",
            0b1_1011 => "expect(panic, alloc, diverge, suspend)",
            0b1_1100 => "expect(block, diverge, suspend)",
            0b1_1101 => "expect(panic, block, diverge, suspend)",
            0b1_1110 => "expect(alloc, block, diverge, suspend)",
            _ => "expect(panic, alloc, block, diverge, suspend)",
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

    /// Determines the lowering mode if already fixed by `self`:
    /// - `Some(LoweringMode::MachineCoroutine)` when `SUSPEND` is already in `base` (monotone:
    ///   instantiation can only add effects, never remove `SUSPEND`),
    /// - `Some(LoweringMode::PlainFn)` when concrete without `SUSPEND`,
    /// - `None` when still polymorphic over an uninstantiated variable without `SUSPEND` in `base`.
    #[inline]
    pub const fn lowering_mode(&self) -> Option<LoweringMode> {
        match *self {
            Self::Concrete(set) => Some(LoweringMode::for_effects(set)),
            Self::Poly { base, .. } => {
                if base.contains(EffectSet::SUSPEND) {
                    Some(LoweringMode::MachineCoroutine)
                } else {
                    None
                }
            }
        }
    }
}

/// Monomorphization lowering strategy chosen from an instantiated effect set.
///
/// Higher-order functions whose instantiated effect set omits [`EffectSet::SUSPEND`] compile to a
/// plain direct function (`PlainFn`) with zero coroutine state-enum overhead; only instantiations
/// carrying `SUSPEND` lower to a resumable `blaze-machine` coroutine.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LoweringMode {
    /// Direct non-suspending function: zero coroutine state-machine overhead.
    PlainFn,
    /// Resumable coroutine lowered through `blaze-machine` because execution may `suspend`.
    MachineCoroutine,
}

impl LoweringMode {
    /// Selects [`LoweringMode::PlainFn`] when `!set.contains(EffectSet::SUSPEND)` and
    /// [`LoweringMode::MachineCoroutine`] when `set.contains(EffectSet::SUSPEND)`.
    #[inline]
    pub const fn for_effects(set: EffectSet) -> Self {
        if set.contains(EffectSet::SUSPEND) {
            Self::MachineCoroutine
        } else {
            Self::PlainFn
        }
    }
}

/// Maximum distinct effect variables tracked inline in an [`EffectRowSet`] (zero heap allocation).
pub const MAX_ROW_VARS: usize = 4;

/// Multi-variable polymorphic effect row (`base | e1 | e2 | ...`) stored inline with zero heap
/// allocation (`#![no_std]`).
///
/// Active variables occupy `vars[..var_count]` in strictly ascending [`EffectVar`] order, with
/// unused trailing slots zeroed (`EffectVar(0)`), so equality and hashing are canonical and
/// independent of union or instantiation order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EffectRowSet {
    vars: [EffectVar; MAX_ROW_VARS],
    var_count: u8,
    base: EffectSet,
}

impl EffectRowSet {
    /// Pure concrete row set (`expect()`).
    pub const PURE: EffectRowSet = Self::concrete(EffectSet::PURE);

    /// Fixed concrete row set with no polymorphic effect variables.
    #[inline]
    pub const fn concrete(base: EffectSet) -> Self {
        Self { vars: [EffectVar(0); MAX_ROW_VARS], var_count: 0, base }
    }

    /// Lifts a single-variable [`EffectRow`] into an [`EffectRowSet`].
    #[inline]
    pub const fn from_row(row: EffectRow) -> Self {
        match row {
            EffectRow::Concrete(base) => Self::concrete(base),
            EffectRow::Poly { base, var } => Self::poly(base, var),
        }
    }

    /// Single-variable polymorphic row set `base | var`.
    #[inline]
    pub const fn poly(base: EffectSet, var: EffectVar) -> Self {
        let mut vars = [EffectVar(0); MAX_ROW_VARS];
        vars[0] = var;
        Self { vars, var_count: 1, base }
    }

    /// Fixed base effects always present regardless of variable instantiation.
    #[inline]
    pub const fn base(self) -> EffectSet {
        self.base
    }

    /// Active unbound effect variables in canonical ascending order.
    #[inline]
    pub fn vars(&self) -> &[EffectVar] {
        &self.vars[..usize::from(self.var_count)]
    }

    /// Number of active unbound effect variables.
    #[inline]
    pub fn var_count(self) -> usize {
        usize::from(self.var_count)
    }

    /// Combines two polymorphic effect rows (`self ∪ other`), deduplicating shared [`EffectVar`]s.
    ///
    /// Returns [`EffectPolyError::TooManyEffectVars`] if the union would hold more than
    /// [`MAX_ROW_VARS`] distinct unbound effect variables.
    pub fn union(self, other: EffectRowSet) -> Result<EffectRowSet, EffectPolyError> {
        let a = self.vars();
        let b = other.vars();
        let mut out = [EffectVar(0); MAX_ROW_VARS];
        let mut out_len = 0usize;
        let mut i = 0usize;
        let mut j = 0usize;

        while i < a.len() || j < b.len() {
            let next = match (a.get(i), b.get(j)) {
                (Some(&va), Some(&vb)) => match va.cmp(&vb) {
                    Ordering::Less => {
                        i += 1;
                        va
                    }
                    Ordering::Greater => {
                        j += 1;
                        vb
                    }
                    Ordering::Equal => {
                        i += 1;
                        j += 1;
                        va
                    }
                },
                (Some(&va), None) => {
                    i += 1;
                    va
                }
                (None, Some(&vb)) => {
                    j += 1;
                    vb
                }
                (None, None) => break,
            };
            if out_len >= MAX_ROW_VARS {
                return Err(EffectPolyError::TooManyEffectVars { max: MAX_ROW_VARS });
            }
            out[out_len] = next;
            out_len += 1;
        }

        let var_count = u8::try_from(out_len).unwrap_or(u8::MAX);
        Ok(Self { vars: out, var_count, base: self.base.union(other.base) })
    }

    /// Instantiates a single effect variable `var` with concrete effects `arg`.
    #[inline]
    pub fn instantiate_one(self, var: EffectVar, arg: EffectSet) -> EffectRowSet {
        self.instantiate_all(|v| if v == var { Some(arg) } else { None })
    }

    /// Instantiates every effect variable for which `lookup` returns `Some(arg)`, merging its
    /// concrete effects into `base` and preserving any remaining unbound variables.
    pub fn instantiate_all(self, lookup: impl Fn(EffectVar) -> Option<EffectSet>) -> EffectRowSet {
        let mut base = self.base;
        let mut out = [EffectVar(0); MAX_ROW_VARS];
        let mut out_len = 0usize;
        for &v in self.vars() {
            if let Some(arg) = lookup(v) {
                base = base.union(arg);
            } else {
                out[out_len] = v;
                out_len += 1;
            }
        }
        let var_count = u8::try_from(out_len).unwrap_or(u8::MAX);
        Self { vars: out, var_count, base }
    }

    /// Returns the concrete [`EffectSet`] iff no unbound effect variables remain (`var_count == 0`).
    #[inline]
    pub const fn as_concrete(self) -> Option<EffectSet> {
        if self.var_count == 0 {
            Some(self.base)
        } else {
            None
        }
    }

    /// Returns `true` if `SUSPEND` is already in `base` or an unbound effect variable may later be
    /// instantiated with `SUSPEND`.
    #[inline]
    pub const fn may_suspend(self) -> bool {
        self.base.contains(EffectSet::SUSPEND) || self.var_count > 0
    }

    /// Determines the lowering mode if already fixed by `self`:
    /// - `Some(LoweringMode::MachineCoroutine)` when `SUSPEND` is already in `base`,
    /// - `Some(LoweringMode::PlainFn)` when concrete (`var_count == 0`) without `SUSPEND`,
    /// - `None` when still polymorphic over an uninstantiated variable without `SUSPEND` in `base`.
    #[inline]
    pub const fn lowering_mode(&self) -> Option<LoweringMode> {
        if self.base.contains(EffectSet::SUSPEND) {
            Some(LoweringMode::MachineCoroutine)
        } else if self.var_count == 0 {
            Some(LoweringMode::PlainFn)
        } else {
            None
        }
    }
}

impl From<EffectRow> for EffectRowSet {
    #[inline]
    fn from(row: EffectRow) -> Self {
        Self::from_row(row)
    }
}

impl From<EffectSet> for EffectRowSet {
    #[inline]
    fn from(set: EffectSet) -> Self {
        Self::concrete(set)
    }
}

/// Errors produced by effect-polymorphic call specialization, multi-variable row unions, and
/// `dyn Fn` object-safety checking.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EffectPolyError {
    /// Instantiated call or callback requires effects outside the caller's allowed `expect(..)` set.
    EffectExceedsAllowed { required: EffectSet, allowed: EffectSet },
    /// Suspending callbacks (`expect(suspend)`) cannot be erased behind unboxed `dyn Fn`; they
    /// must be statically monomorphized so `blaze-machine` can build their state machine.
    SuspendBehindDyn { callback_effects: EffectSet },
    /// A polymorphic effect variable remained unbound when specializing a call site.
    UnboundEffectVar(EffectVar),
    /// Combining polymorphic rows exceeded the inline capacity [`MAX_ROW_VARS`].
    TooManyEffectVars { max: usize },
}

impl fmt::Display for EffectPolyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            EffectPolyError::EffectExceedsAllowed { required, allowed } => {
                write!(f, "effect set '{required}' exceeds caller allowance '{allowed}'")
            }
            EffectPolyError::SuspendBehindDyn { callback_effects } => {
                write!(
                    f,
                    "cannot coerce suspending callback '{callback_effects}' to `dyn Fn`: `suspend` requires static monomorphization"
                )
            }
            EffectPolyError::UnboundEffectVar(EffectVar(id)) => {
                write!(f, "unbound polymorphic effect variable `e{id}` at call site")
            }
            EffectPolyError::TooManyEffectVars { max } => {
                write!(f, "polymorphic effect row exceeds inline capacity of {max} distinct effect variables")
            }
        }
    }
}

/// Object-safety and effect-subtyping check for coercing a closure or function with `callback`
/// effects into a `dyn Fn(...) expect(allowed)` trait object.
///
/// Suspending callbacks (`EffectSet::SUSPEND`) cannot be erased behind unboxed `dyn Fn` because
/// coroutine lowering requires static monomorphization; they are refused with
/// [`EffectPolyError::SuspendBehindDyn`]. Non-suspending callbacks must satisfy
/// `callback ⊆ allowed`.
pub const fn check_dyn_fn_effects(callback: EffectSet, allowed: EffectSet) -> Result<(), EffectPolyError> {
    if callback.contains(EffectSet::SUSPEND) {
        return Err(EffectPolyError::SuspendBehindDyn { callback_effects: callback });
    }
    if !callback.is_subset_of(allowed) {
        return Err(EffectPolyError::EffectExceedsAllowed { required: callback, allowed });
    }
    Ok(())
}

/// Instantiates a polymorphic callee's [`EffectRowSet`] at a call site using `bindings`, verifies
/// that no unbound effect variables remain and that `instantiated ⊆ caller_allowed`, and returns
/// the concrete instantiated [`EffectSet`] together with its specialized [`LoweringMode`].
pub fn specialize_call(
    sig_row: EffectRowSet,
    bindings: &[(EffectVar, EffectSet)],
    caller_allowed: EffectSet,
) -> Result<(EffectSet, LoweringMode), EffectPolyError> {
    let resolved = sig_row.instantiate_all(|var| {
        let mut combined: Option<EffectSet> = None;
        for &(bound_var, eff) in bindings {
            if bound_var == var {
                combined = Some(combined.map_or(eff, |acc| acc.union(eff)));
            }
        }
        combined
    });

    if let Some(&unbound) = resolved.vars().first() {
        return Err(EffectPolyError::UnboundEffectVar(unbound));
    }

    let instantiated = resolved.base();
    if !instantiated.is_subset_of(caller_allowed) {
        return Err(EffectPolyError::EffectExceedsAllowed { required: instantiated, allowed: caller_allowed });
    }

    Ok((instantiated, LoweringMode::for_effects(instantiated)))
}

/// Instantiates and checks a single-callback higher-order call (`EffectRow`) with `callback_effects`
/// against `caller_allowed`, returning the concrete [`EffectSet`] and specialized [`LoweringMode`].
pub fn check_higher_order_call(
    sig_row: EffectRow,
    callback_effects: EffectSet,
    caller_allowed: EffectSet,
) -> Result<(EffectSet, LoweringMode), EffectPolyError> {
    match sig_row {
        EffectRow::Concrete(base) => specialize_call(EffectRowSet::concrete(base), &[], caller_allowed),
        EffectRow::Poly { base, var } => {
            specialize_call(EffectRowSet::poly(base, var), &[(var, callback_effects)], caller_allowed)
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

        let suspend_fn = cx.tys.fn_ptr(sig, FnHeader::RUST.with_effects(EffectSet::SUSPEND));
        out.0.clear();
        print(&cx.tys, suspend_fn.node(), &mut DefaultNames, &mut out);
        assert_eq!(out.0.as_str(), "fn(u32) -> u32 expect(suspend)");
    }

    #[test]
    fn suspend_effect_packs_into_fn_header_bit_15_and_enforces_subtyping() {
        assert_eq!(EffectSet::from_word("suspend"), Some(EffectSet::SUSPEND));
        let h = FnHeader::new(7, false, true, 0xBEEF).with_effects(EffectSet::SUSPEND);
        assert_eq!(h.abi(), 7);
        assert!(!h.is_unsafe());
        assert!(h.c_variadic());
        assert_eq!(h.bound_vars(), 0xBEEF, "bit 15 must not clobber bound_vars in bits 16..31");
        assert_eq!(h.declared_effects(), Some(EffectSet::SUSPEND));

        let pure_fn = FnHeader::RUST.with_effects(EffectSet::PURE);
        let suspend_fn = FnHeader::RUST.with_effects(EffectSet::SUSPEND);
        let alloc_suspend = FnHeader::RUST.with_effects(EffectSet::ALLOC.union(EffectSet::SUSPEND));
        assert!(fn_ptr_coerces(pure_fn, suspend_fn).is_ok());
        assert!(fn_ptr_coerces(suspend_fn, alloc_suspend).is_ok());
        assert!(fn_ptr_coerces(suspend_fn, FnHeader::RUST).is_ok());
        assert_eq!(
            fn_ptr_coerces(suspend_fn, pure_fn).unwrap_err(),
            EffectError::FnPtrEffectMismatch { expected: EffectSet::PURE, found: EffectSet::SUSPEND }
        );

        let e = EffectVar(1);
        let map_row = EffectRow::poly(EffectSet::ALLOC, e);
        assert_eq!(
            map_row.instantiate(e, EffectSet::SUSPEND),
            EffectRow::Concrete(EffectSet::ALLOC.union(EffectSet::SUSPEND))
        );
    }
}
