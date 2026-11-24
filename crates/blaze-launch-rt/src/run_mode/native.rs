//! Run mode's side of the program's native library (NATIVE.md 3.7; piece W8): the parent's
//! `Down::Native` declares its word pairs like a `Declare` and asks the driver to load the
//! library ([`Act::Native`]); the driver carries that with [`RunMode::load_native`], which runs
//! crate::native_lib's [`NativeLoad::start`] through the [`Memory`] and [`Natives`] seams, so the
//! simulation and the process run the same order.

use blaze_launch::wire::run::Declared;
use blaze_launch::wire::{List, RecordId};
use blaze_own::{AVec, Vec};

use super::{Act, Memory, RunMode, RunModeError};
use crate::native_lib::{Loaded, NativeError, NativeLoad, NativeWordSlot, Natives};

/// Where the run is with its native library.
pub(super) enum NativeState<'a> {
    /// No `Native` yet.
    Waiting,
    /// The parent's `Native` named `record`; `slots[i]` is the local slot of its pair i and
    /// whether it was newly declared (needs stubbing) or already declared earlier in the run.
    /// The driver loads it on the [`Act::Native`] that followed.
    Declared { record: RecordId, slots: AVec<'a, NativeWordSlot> },
    Loaded(Loaded),
}

impl<'a> RunMode<'a> {
    /// `Down::Native { library, words }`: declares each pair (as `Declare` does: the parent's
    /// slot number for the symbol, fixed for the run) and asks the driver to load `library`.
    /// Newly declared slots are stubbed by the load itself (`NativeLoad::start`, before any word
    /// is filled); already-declared slots keep their existing stub or published body.
    pub(super) fn on_native(
        &mut self,
        library: RecordId,
        words: List<'_, Declared>,
        out: &mut AVec<'_, Act>,
    ) -> Result<(), RunModeError> {
        if !matches!(self.native, NativeState::Waiting) {
            return Err(RunModeError::Native(NativeError::Twice { record: library }));
        }
        let mut slots: AVec<'a, NativeWordSlot> = Vec::new_in(self.alloc);
        for Declared { symbol, slot } in words.iter() {
            let (local, is_new) = self.declare_pair(symbol, slot)?;
            slots.push(NativeWordSlot { slot: local, is_new });
        }
        self.native = NativeState::Declared { record: library, slots };
        out.push(Act::Native { record: library });
        Ok(())
    }

    /// Carries [`Act::Native`]: stubs the declared slots, opens the library, checks its
    /// descriptor and fills its words and pointer cells. The init list is the caller's to run
    /// on the first thread, before the entry ([`crate::native_lib::run_inits`]).
    pub fn load_native<N: Natives, M: Memory>(&mut self, natives: &mut N, memory: &mut M) -> Result<Loaded, RunModeError> {
        let NativeState::Declared { record, slots } = &self.native else {
            return Err(RunModeError::NotBuilt { what: "an `Act::Native` with no `Native` declared before it" });
        };
        let loaded = NativeLoad::start(*record, slots, natives, memory)?;
        self.native = NativeState::Loaded(loaded);
        Ok(loaded)
    }

    /// The loaded library, once [`RunMode::load_native`] succeeded.
    pub fn native(&self) -> Option<Loaded> {
        match self.native {
            NativeState::Loaded(loaded) => Some(loaded),
            _ => None,
        }
    }
}
