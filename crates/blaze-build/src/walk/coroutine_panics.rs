//! The blocks a coroutine's `poll` goes to on a finished state (blaze-lir's
//! `coroutine::Panics`; ASYNC.md D-A5): each calls the library's panic for that case.
//!
//! core/src/panicking.rs:199 and :203, in the library's `panic_const!` list:
//! `panic_const_async_fn_resumed = "`async fn` resumed after completion"` and
//! `panic_const_async_fn_resumed_panic = "`async fn` resumed after panicking"`, each a
//! `#[lang]` `const fn() -> !`. A program whose library declares neither (`no_core`) gets no
//! block, and the split's `poll` traps there instead; one whose library declares an item
//! always gets its block, so the trap never stands in for a panic the library has.
//!
//! The call is a plain `call`, not an `invoke`: rustc's coroutine transform adds these
//! panics after it poisons the body's unwind edges, so the panic unwinds out of `poll` and
//! leaves the tag as it was (`Returned`, or `Panicked`).

use core::alloc::Allocator;

use blaze_code::record::TrapKind;
use blaze_lir::coroutine::Panics;
use blaze_lir::func::RefKind;
use blaze_lir::{Block, Flags};
use blaze_typeck::List;

use super::Walk;
use crate::{BuildError, Instance};

/// The library's panic of a poll after completion.
pub(crate) const RESUMED: &str = "panic_const_async_fn_resumed";
/// The library's panic of a poll after a poll that unwound.
pub(crate) const RESUMED_PANIC: &str = "panic_const_async_fn_resumed_panic";

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// The two finished-state blocks of the coroutine body being built, after
    /// [`Self::run_coroutine`]: no edge of the body leads to either.
    pub(super) fn coroutine_panics(&mut self) -> Result<Panics, BuildError> {
        let returned = self.finished_block(RESUMED)?;
        let panicked = self.finished_block(RESUMED_PANIC)?;
        Ok(Panics { returned, panicked })
    }

    /// A block calling the library's lang fn `name` and never returning, if the library
    /// declares it.
    fn finished_block(&mut self, name: &'static str) -> Result<Option<Block>, BuildError> {
        let Some(item) = self.adts.lang_fn(name) else {
            return Ok(None);
        };
        let block = self.create();
        self.reached[block.index()] = true;
        self.begin(block);
        // The pinned entry points are `#[track_caller]`; the place is the coroutine body's, as
        // rustc's transform gives (walk/track_caller.rs).
        let callee = Instance { item, args: List::EMPTY };
        let root = self.body.root;
        let (sig, values) = self.lang_call_sig(Some(root), callee, &[], &[])?;
        let place = self.ref_place(callee);
        let reference = self.b.reference(RefKind::Func, place);
        self.b.call(Flags::NONE, reference, sig, &values);
        self.unreachable_of(TrapKind::AfterPanic, Some(root));
        if let Some(error) = self.b.error() {
            return Err(BuildError::Lir(error.clone()));
        }
        Ok(Some(block))
    }
}
