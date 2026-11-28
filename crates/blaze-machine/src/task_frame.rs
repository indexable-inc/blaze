//! [`TaskFrame`]: cross-thread movable `(Arena, PausedState)` pairing with zero `Pin` and
//! kernel-DMA cancellation drain safety (`docs/v2/plan/design/sans-io-machines.md`, section 1).
//!
//! Under `docs/v2/plan/design/drop-with.md`, `FrameVec<'id, T>` and `Ref32<'id, T>` store
//! `NonNull<T>` into heap-allocated arena chunks (or 32-bit offsets) and **never** a back-pointer
//! to the arena header struct itself. Moving the `(Arena, PausedState)` pair to another worker
//! thread therefore invalidates zero pointers and needs no `Pin`—`TaskFrame<A, M>` is `Send`
//! whenever `A: Send` and `M: Send`.
//!
//! Safety invariant for `io_uring` / async DMA cancellation:
//! while any DMA buffer lent from this task's arena is still in flight in the kernel (including
//! after external cancellation before the cancel CQE lands), `in_flight_dma > 0`, and
//! [`TaskFrame::try_reset`] / [`TaskFrame::try_into_parts`] refuses with [`FrameBusy`] until
//! every [`DmaTicket`] has been returned via [`TaskFrame::complete_dma`].

use core::fmt;

use blaze_own::planted::{NoPlant, Planted, Plants};

/// Linear proof token representing one DMA buffer from a [`TaskFrame`] currently owned by the
/// kernel / IO driver. Must be returned to [`TaskFrame::complete_dma`] once the operation (or its
/// cancellation) completes.
#[blaze::linear]
#[derive(Debug, PartialEq, Eq)]
pub struct DmaTicket {
    id: u32,
}

impl DmaTicket {
    /// Identifier of this in-flight DMA operation (deterministic per task).
    #[inline]
    pub const fn id(&self) -> u32 {
        self.id
    }
}

/// Refusal returned when a caller tries to reset or dismantle a [`TaskFrame`] while kernel DMA
/// operations referencing its arena memory have not yet completed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrameBusy {
    pub in_flight_dma: u32,
}

impl fmt::Display for FrameBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot reset or release task frame while {} DMA operation(s) are still in flight",
            self.in_flight_dma
        )
    }
}

/// Planted-bug switch for [`TaskFrame`] DST controls (rule 43 & 53).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum TaskFramePlant {
    #[default]
    None,
    /// Planted bug: allow resetting the arena even while `in_flight_dma > 0` (models kernel DMA
    /// use-after-free on cancellation).
    ResetWhileDmaInFlight,
}

impl Plants for TaskFramePlant {}

/// Pairs an owned task arena `A` with a paused machine state `M` and tracks in-flight kernel DMA
/// borrows referencing `A`.
///
/// Movable across threads (`Send` iff `A: Send && M: Send`) with zero `Pin` and zero `unsafe`.
#[derive(Debug)]
pub struct TaskFrame<A, M, P: Planted<TaskFramePlant> = NoPlant> {
    arena: A,
    state: M,
    in_flight_dma: u32,
    next_ticket: u32,
    plant: P,
}

impl<A, M> TaskFrame<A, M, NoPlant> {
    /// Creates a new movable `(arena, state)` pair with zero in-flight DMA operations.
    #[inline]
    pub const fn new(arena: A, state: M) -> Self {
        Self { arena, state, in_flight_dma: 0, next_ticket: 0, plant: NoPlant }
    }
}

impl<A, M, P: Planted<TaskFramePlant>> TaskFrame<A, M, P> {
    /// Creates a `TaskFrame` with a planted-bug seam for DST controls.
    #[inline]
    pub const fn with_plant(arena: A, state: M, plant: P) -> Self {
        Self { arena, state, in_flight_dma: 0, next_ticket: 0, plant }
    }

    /// Borrows the arena and paused state disjointly for one transition step.
    #[inline]
    pub fn parts_mut(&mut self) -> (&mut A, &mut M) {
        (&mut self.arena, &mut self.state)
    }

    /// Read-only access to the paused state.
    #[inline]
    pub const fn state(&self) -> &M {
        &self.state
    }

    /// Read-only access to the owned arena.
    #[inline]
    pub const fn arena(&self) -> &A {
        &self.arena
    }

    /// Number of DMA operations currently referencing memory in this frame.
    #[inline]
    pub const fn in_flight_dma(&self) -> u32 {
        self.in_flight_dma
    }

    /// Records that one DMA buffer from this frame has been submitted to the driver/kernel.
    ///
    /// Fails cleanly on `u32::MAX` overflow (rule 18 `bounded-progress`).
    #[inline]
    pub fn start_dma(&mut self) -> Result<DmaTicket, FrameBusy> {
        let Some(next_count) = self.in_flight_dma.checked_add(1) else {
            return Err(FrameBusy { in_flight_dma: self.in_flight_dma });
        };
        self.in_flight_dma = next_count;
        let id = self.next_ticket;
        self.next_ticket = self.next_ticket.wrapping_add(1);
        Ok(DmaTicket { id })
    }

    /// Records that the kernel/driver has finished with a submitted (or cancelled) DMA buffer,
    /// consuming its linear [`DmaTicket`].
    #[blaze::consumes]
    #[inline]
    pub fn complete_dma(&mut self, _ticket: DmaTicket) {
        debug_assert!(self.in_flight_dma > 0, "DmaTicket completed with zero in-flight DMA");
        self.in_flight_dma = self.in_flight_dma.saturating_sub(1);
    }

    /// Runs `reset_arena` only when ZERO DMA operations remain in flight.
    ///
    /// Refuses with [`FrameBusy`] if any cancelled or pending DMA buffer has not drained yet.
    #[inline]
    pub fn try_reset(&mut self, reset_arena: impl FnOnce(&mut A)) -> Result<(), FrameBusy> {
        if !self.plant.has(TaskFramePlant::ResetWhileDmaInFlight) && self.in_flight_dma != 0 {
            return Err(FrameBusy { in_flight_dma: self.in_flight_dma });
        }
        reset_arena(&mut self.arena);
        Ok(())
    }

    /// Dismantles the `TaskFrame` into `(arena, state)` only when zero DMA operations are in
    /// flight; otherwise returns `(self, FrameBusy)` so the caller can wait for cancelled CQEs to
    /// drain before dropping the arena.
    #[inline]
    pub fn try_into_parts(self) -> Result<(A, M), (Self, FrameBusy)> {
        if !self.plant.has(TaskFramePlant::ResetWhileDmaInFlight) && self.in_flight_dma != 0 {
            let busy = FrameBusy { in_flight_dma: self.in_flight_dma };
            return Err((self, busy));
        }
        Ok((self.arena, self.state))
    }
}
