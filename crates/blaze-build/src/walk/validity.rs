//! The validity assertions in the walk (`assert_inhabited`, `assert_zero_valid`,
//! `assert_mem_uninitialized_valid`): decided at the instance by `T`'s layout
//! (intrinsics/validity.rs); when `T` passes, nothing is built; when it does not, a call of
//! the `panic_nounwind` lang fn with the library's message around `T`'s name (type_name.rs)
//! and `unreachable`. The pinned core, library/core/src/intrinsics/mod.rs:753-782: "This will
//! statically either panic, or do nothing"; library/core/src/panicking.rs: `#[lang =
//! "panic_nounwind"] pub const fn panic_nounwind(expr: &'static str) -> !`, a `&str` passed
//! under the Rust ABI as any aggregate, by its address (lib.rs, "Wide pointers").

use core::alloc::Allocator;

use blaze_code::record::TrapKind;
use blaze_lir::builder::{MemAttrs, Param};
use blaze_lir::func::RefKind;
use blaze_lir::{Flags, Ty};
use blaze_own::String;
use blaze_typeck::{ExprId, List, Mutbl, PNode};

use super::{V, Walk, layout_err, malformed};
use crate::intrinsics::Which;
use crate::intrinsics::validity::{Check, permits};
use crate::layout::LayoutCx;
use crate::type_name::type_name_text;
use crate::{BuildError, Instance};

/// The lang fn a failed assertion calls.
const PANIC_NOUNWIND: &str = "panic_nounwind";

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// The call `e` of assertion `check` (intrinsic `which`) with generic arguments `gl` and
    /// `n` arguments (none, by its signature).
    pub(super) fn validity_check(&mut self, e: ExprId, which: Which, check: Check, gl: List, n: usize) -> Result<(), BuildError> {
        let gs = self.ground.view().list(gl).map_err(|_| malformed("a list past the ground table"))?;
        let (&[t], 0) = (gs, n) else {
            return Err(BuildError::Intrinsic { expr: e, which, why: "not one type argument and no arguments", unsupported: false });
        };
        let adts = self.adts;
        let permitted = {
            let mut cx = LayoutCx { ground: &mut *self.ground, facts: adts.facts(), scratch: self.scratch };
            permits(check, t, &mut self.ls, &mut cx)
        };
        if permitted.map_err(|error| Self::ty_err(e, layout_err(error)))? {
            self.push_val(V::None, None);
            return Ok(());
        }
        let refuse = |why| BuildError::Intrinsic { expr: e, which, why, unsupported: true };
        let names = adts.type_names().ok_or(refuse("the name of its type: the front end names no items (`Adts::type_names`)"))?;
        let name = type_name_text(self.ground.view(), t, names, self.alloc).map_err(|error| refuse(error.what()))?;
        let (before, after) = check.message();
        let mut text = String::new_in(self.alloc);
        text.push_str(before);
        text.push_str(name.as_str());
        text.push_str(after);
        let item = adts.lang_fn(PANIC_NOUNWIND).ok_or(BuildError::NoLangFn { expr: Some(e), name: PANIC_NOUNWIND })?;
        // The `&'static str`: the bytes as data, and the pair of their address and length.
        let str_ = self.ground.intern(PNode::Str).map_err(|_| malformed("the ground table is full"))?;
        let ref_str = self.ground.intern(PNode::Ref(Mutbl::Not, str_)).map_err(|_| malformed("the ground table is full"))?;
        let repr = self.repr(ref_str).map_err(|error| Self::ty_err(e, error))?;
        let data = self.b.data(text.as_bytes(), 0, &[]);
        let p = self.b.data_addr(data);
        let at = self.temp(repr);
        self.b.store(Flags::NONE, p, at.0, at.1, MemAttrs::aligned(3));
        let len = self.b.iconst(Ty::I64, text.len() as u128);
        self.b.store(Flags::NONE, len, at.0, at.1 + 8, MemAttrs::aligned(3));
        let arg = self.address(at);
        // A `#[track_caller]` entry point is passed the location last (walk/track_caller.rs).
        let callee = Instance { item, args: List::EMPTY };
        let (sig, values) = self.lang_call_sig(Some(e), callee, &[Param::of(Ty::PTR)], &[arg])?;
        let place = self.ref_place(callee);
        let reference = self.b.reference(RefKind::Func, place);
        // `panic_nounwind` is `#[rustc_nounwind]` and returns `!`: a plain call, then nothing.
        self.b.call(Flags::NONE, reference, sig, &values);
        self.unreachable_of(TrapKind::AfterPanic, Some(e));
        self.dead_block();
        self.push_val(V::None, None);
        Ok(())
    }
}
