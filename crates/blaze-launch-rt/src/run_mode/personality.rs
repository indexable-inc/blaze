//! The run mode's personality routine and its one unwind registry (port27/tempo-run/UNWIND.md,
//! 3.3; piece P3).
//!
//! **The personality.** A record whose unwind rows land (a cleanup, a catch, a terminate) gets
//! an entry whose CIE names a personality routine (blaze-code eh.rs, the `zPLR` augmentation):
//! the program's `#[lang = "eh_personality"]` fn, which no record holds. The parent names its
//! slot once, after the `Declare` that numbers it and before any `Publish`
//! (`Down::Personality { slot }`); [`RunMode::bind_personality`] keeps the local slot, and
//! [`RunMode::personality_at`] gives its ENTRY THUNK's address (blaze-code place.rs: "its entry
//! thunk's address, never a body"; fixed while the run lives), which every later entry names.
//! The personality's body is compiled on its first call, like any fn: the first throw calls the
//! thunk, the thunk loads the slot's word, the stub fires, the control thread places the body.
//! A record whose rows land placed while no personality is bound stays refused by name
//! (`EhError::NoPersonality`, through `RunModeError::Eh`). A second `Personality` is
//! [`RunModeError::PersonalityTwice`]: a fn's entry names one routine for the run.
//!
//! **The registry.** Every placed record's entry is registered through piece U's
//! [`Registry`] (unwind_reg.rs): one finder on macOS (installed once, here), libgcc's
//! `__register_frame` per record on Linux (inside [`Writer::add`]). It is sized by the run's
//! slot count and made from an allocator that lives for the process, because the system's
//! unwinder keeps the finder for the process.

use blaze_launch::Slot;
use blaze_own::{Box, SharedAlloc};
use blaze_run::SlotId;

use super::{MOST_SLOTS, Memory, Placing, RunMode, RunModeError};
use crate::unwind_reg::{Registry, Span, Writer};

impl RunMode<'_> {
    /// `Down::Personality { slot }`: binds the program's personality routine to the local slot
    /// the parent's `slot` was declared as. A slot never declared is
    /// [`RunModeError::Undeclared`]; a second message is [`RunModeError::PersonalityTwice`].
    pub(super) fn bind_personality(&mut self, slot: Slot) -> Result<(), RunModeError> {
        if self.personality.is_some() {
            return Err(RunModeError::PersonalityTwice { slot });
        }
        let local = self.slots.local(slot)?;
        self.personality = Some(local);
        Ok(())
    }

    /// The address every entry made now names as its personality routine: the bound slot's
    /// entry thunk, or `None` before the parent named one.
    pub(super) fn personality_at(&self, memory: &impl Memory) -> Option<u64> {
        self.personality.map(|slot: SlotId| memory.thunk(slot))
    }
}

/// The run's one unwind registry: made in `alloc` (which lives for the process, as the
/// registry must: the system's unwinder keeps the finder), sized by [`MOST_SLOTS`] (a record
/// per slot at the tier the run mode places), installed as the system's finder on macOS, and
/// its one writer taken for the control thread, which is the one writer of code and words.
pub(super) fn run_registry(alloc: SharedAlloc<'static>) -> Result<Writer<'static>, RunModeError> {
    let made = Registry::new_in(MOST_SLOTS as usize, alloc).map_err(RunModeError::Unwinder)?;
    let registry: &'static Registry = Box::leak_static(Box::new_in(made, alloc));
    #[cfg(target_os = "macos")]
    crate::unwind_reg::install(registry).map_err(RunModeError::Unwinder)?;
    registry.writer().map_err(RunModeError::Unwinder)
}

/// The registry's row for a record on its way in: its code's range, its entry's `.eh_frame`
/// part (the CIE, the FDE and the ending zero: up to the LSDA, or the whole entry when it has
/// none, as unwind_reg.rs `eh_bytes` measures it), and its slot as the placer's name for it.
/// `None` for a record with no entry (nothing to unwind through).
pub(super) fn span_of(placing: &Placing<'_>) -> Option<Span> {
    let (eh_at, bytes, parts) = placing.entry?;
    let eh_len = parts.lsda.unwrap_or(bytes.len()) as u64;
    Some(Span {
        code_lo: placing.code_at,
        code_hi: placing.code_at + placing.code.len() as u64,
        eh_at,
        eh_len,
        id: u64::from(placing.slot.0),
    })
}
