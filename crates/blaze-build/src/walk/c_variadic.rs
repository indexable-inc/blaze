//! Calls of a C-variadic foreign fn (`extern "C" { fn snprintf(s: *mut c_char, n: usize,
//! format: *const c_char, ...) -> c_int; }`): the declared arguments are passed by the C ABI as
//! any (`Walk::pass`), the arguments written past them are the variable ones.
//!
//! The Rust Reference, "External blocks", [items.extern.variadic.syntax]: "Functions within
//! external blocks may be made variadic by specifying ... as the last parameter." What each
//! variable argument may be is the checker's (rules/c_variadic.rs, E0617: "Certain Rust types
//! must be cast before passing them to a variadic function, because of arcane ABI rules
//! dictated by the C standard"): an `f32`, a `bool` and an integer narrower than `c_int` never
//! reach here, and are refused by name if they do (a broken invariant of the input).
//!
//! The call is built as LIR's variadic form: a signature with `Variadic::Yes` whose `params`
//! are the declared ones only, called with every value (LIR's verify accepts `args.len() >=
//! sig.params.len()` for it); the emitter places the values past the params by the platform's
//! rule (blaze-emit pass/variadic.rs: Apple stacks each in an 8-byte slot, Linux continues the
//! registers). `cabi::assign` with `variadic_from = Some(declared)` states the same places
//! (tests/variadic_call.rs).
//!
//! A "C" callee is called with a plain `call`, never an `invoke`: unwinding out of it is
//! undefined (the Reference, "Behavior considered undefined": "unwinding past a stack frame that
//! does not allow unwinding (e.g. by calling a "C-unwind" function imported or transmuted as a
//! "C" function or function pointer)"), and on Apple the emitter's outgoing area lowers `sp`
//! across the call, which a landing pad would inherit. A variadic fn of an unwinding ABI
//! (`"C-unwind"`, `"system-unwind"`) is refused by name until a rung asks.

use core::alloc::Allocator;

use blaze_lir::Flags;
use blaze_lir::Ty;
use blaze_lir::builder::Variadic;
use blaze_lir::func::{Conv, Ext};
use blaze_typeck::ExprId;

use super::{Callee, Pass, Repr, V, Walk, malformed, unsupported};
use crate::{Abi, BuildError};

/// Why a C-variadic call is refused, by name.
const NOT_C: &str = "`...` on a fn of an ABI other than \"C\" or \"system\" (the Reference, [items.extern.variadic.conventions])";
const UNWINDING: &str = "a call of a C-variadic fn of an unwinding ABI (\"C-unwind\", \"system-unwind\"): built as a plain call with no landing pad, so not built until a rung asks";
const UNSAID: &str = "a call of a C-variadic fn whose unwind category (\"C\" or \"C-unwind\") the build's `Adts::nounwind` does not say";
const UNPROMOTED: &str = "a variable argument narrower than `c_int`, a `bool` or an `f32` (C's default promotions; the checker refuses it, E0617)";
const AGGREGATE: &str = "an aggregate, a wide pointer or a value-less type passed to `...` (not built)";

impl<'x, 'c, 't, A: Allocator + Copy> Walk<'x, 'c, 't, A> {
    /// Whether a C-variadic callee of `abi` whose unwind category is `nounwind`
    /// ([`crate::Adts::nounwind`]) is built (as a plain call: module doc), or its refusal by name.
    /// The builder's `Abi` has no `system` or unwinding form (the front end maps "C", "C-unwind",
    /// "system" and "system-unwind" to [`Abi::C`]), so the unwinding forms are told apart by
    /// `nounwind`: only `Some(true)` (the Reference's "non-unwinding" category) is built.
    pub(super) fn c_variadic_abi(e: ExprId, abi: Abi, nounwind: Option<bool>) -> Result<(), BuildError> {
        match (abi, nounwind) {
            (Abi::C, Some(true)) => Ok(()),
            (Abi::C, Some(false)) => Err(unsupported(e, UNWINDING)),
            (Abi::C, None) => Err(unsupported(e, UNSAID)),
            (Abi::Rust | Abi::RustCall, _) => Err(unsupported(e, NOT_C)),
        }
    }

    /// How one variable argument held as `repr` is passed: one register-sized scalar, never
    /// widened here (C's promotions were the checker's).
    pub(super) fn vararg_pass(e: ExprId, repr: Repr) -> Result<Pass, BuildError> {
        let scalar = |ty: Ty| Pass::Scalar { ty, ext: Ext::None, agg: false };
        match repr {
            Repr::Int { ty, .. } if ty == Ty::I32 || ty == Ty::I64 => Ok(scalar(ty)),
            Repr::Float(ty) if ty == Ty::F64 => Ok(scalar(ty)),
            Repr::Ptr => Ok(scalar(Ty::PTR)),
            Repr::Int { .. } | Repr::Float(_) | Repr::Bool => Err(unsupported(e, UNPROMOTED)),
            Repr::Agg(_) | Repr::None => Err(unsupported(e, AGGREGATE)),
        }
    }

    /// [`Walk::emit_call`] with `declared`: `Some(n)` for a C-variadic callee whose first `n`
    /// of `passes` are its declared parameters (the rest the `...` ones), `None` for any other
    /// call. One home for building a call: `emit_call` is this with `None`.
    pub(super) fn emit_call_from(&mut self, callee: Callee, reprs: &[Repr], passes: &[Pass], declared: Option<usize>, callee_nounwind: bool, rrepr: Repr, rpass: Pass) -> Result<(), BuildError> {
        let named = match declared {
            Some(n) if n <= passes.len() => &passes[..n],
            Some(_) => return Err(malformed("a C-variadic call with fewer arguments than declared parameters (the checker's E0061)")),
            None => passes,
        };
        let (params, rets) = self.sig_of(named, rpass);
        let mut values = Vec::with_capacity_in(params.len(), self.alloc);
        // The result's slot, the callee's to write.
        let result = match rpass {
            Pass::Ref | Pass::Memory { .. } => {
                let at = self.temp(rrepr);
                values.push(self.address(at));
                Some(at)
            }
            _ => None,
        };
        let n = reprs.len();
        let base = self.vals.len().checked_sub(n).ok_or(malformed("a call's arguments missing from the value stack (a build bug)"))?;
        for i in 0..n {
            let v = self.vals[base + i];
            self.pass_arg(v, reprs[i], passes[i], &mut values)?;
        }
        self.truncate_vals(base);
        let r = match declared {
            None if callee_nounwind => {
                // A non-unwinding callee (`extern "C"`, `#[rustc_nounwind]`) cannot unwind: a plain
                // call, no landing pad or terminate block.
                let sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
                match callee {
                    Callee::Direct(reference) => self.b.call(Flags::NONE, reference, sig, &values),
                    Callee::Pointer(p) => self.b.call_indirect(Flags::NONE, p, sig, &values),
                }
            }
            None => {
                let sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
                self.call_unwinding(callee, sig, &values)?
            }
            // A "C" callee does not unwind (module doc): a plain call, no landing pad.
            Some(_) => {
                let sig = self.b.sig(Conv::C, Variadic::Yes, &params, &rets);
                match callee {
                    Callee::Direct(reference) => self.b.call(Flags::NONE, reference, sig, &values),
                    Callee::Pointer(p) => self.b.call_indirect(Flags::NONE, p, sig, &values),
                }
            }
        };
        let v = match (result, rets.len()) {
            (Some(at), _) => V::Place(at.0, at.1),
            (None, 0) => self.take_passed(rrepr, rpass, &mut core::iter::empty())?,
            (None, 1) => self.take_passed(rrepr, rpass, &mut core::iter::once(r))?,
            (None, k) => {
                let mut parts = Vec::with_capacity_in(k, self.alloc);
                for i in 0..k as u32 {
                    parts.push(self.b.project(r, i));
                }
                self.take_passed(rrepr, rpass, &mut parts.iter().copied())?
            }
        };
        self.push_val(v, None);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use blaze_typeck::ExprId;

    use super::Walk;
    use crate::Abi;

    /// A C-variadic callee is built only when its ABI is "C"-family AND non-unwinding: the
    /// Reference, "Functions", [items.fn.extern.unwind.behavior]: "The \"unwinding\" ABI
    /// category refers to \"Rust\" ..., \"C-unwind\", and any other ABI with -unwind in its
    /// name"; a variadic call is a plain `call` (module doc), so an unwinding callee is refused
    /// by name. A fix that kept matching on `Abi` alone builds the `Some(false)` and `None`
    /// cases and fails here.
    ///
    /// Line: `c variadic abi: C nounwind built; C-unwind, unsaid, Rust refused`.
    #[test]
    fn a_c_variadic_callee_of_an_unwinding_abi_is_refused() {
        type W<'x, 'c, 't> = Walk<'x, 'c, 't, std::alloc::Global>;
        let e = ExprId(0);
        assert!(W::c_variadic_abi(e, Abi::C, Some(true)).is_ok(), "\"C\" (nounwind) is built");
        assert!(W::c_variadic_abi(e, Abi::C, Some(false)).is_err(), "\"C-unwind\" is refused");
        assert!(W::c_variadic_abi(e, Abi::C, None).is_err(), "an unsaid category is refused");
        assert!(W::c_variadic_abi(e, Abi::Rust, Some(false)).is_err(), "\"Rust\" is refused");
        println!("c variadic abi: C nounwind built; C-unwind, unsaid, Rust refused");
    }
}
