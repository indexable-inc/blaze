//! Kernel provided-buffer rings (`io_uring` `IORING_REGISTER_PBUF_RING` zero-copy Rx rings).
//!
//! Pure `#![no_std]`, zero global heap (`Alloc` / explicit `Allocator`), `#![forbid(unsafe_code)]`:
//!
//! - [`LeasedBuf<'io>`]: compact (`<= 12` bytes), `#[blaze::handle]`, `#[blaze::linear]`, NO `Drop`,
//!   branded by invariant [`Brand<'io>`] so a lease can never be forged, dropped silently, or
//!   returned to the wrong ring.
//! - [`ReturnBuf<'io>`]: linear [`Request`] returning a [`LeasedBuf`] to the ring.
//! - [`BufferRing<'io, A>`]: fixed-capacity bitset ring with O(1) slot tracking, double-return
//!   detection, drain verification, and [`LeasePlant`] planted-bug controls (rules 43 & 53).

use core::alloc::Allocator;
use core::convert::Infallible;
use core::fmt;
use core::mem::size_of;

use blaze_own::planted::{NoPlant, Planted, Plants};
use blaze_own::{Alloc, Brand, Vec};

use crate::{Handler, Perform, Request};

/// Buffer group identifier (`bgid`) for a kernel provided-buffer ring.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RingGroupId(pub u16);

/// Buffer slot identifier (`bid`) within a [`BufferRing`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RingBufId(pub u16);

/// Zero-copy Rx buffer lease acquired from a [`BufferRing`].
///
/// Compact (`8` bytes), `#[blaze::handle]`, `#[blaze::linear]`, with NO `Drop`: must be returned
/// to its originating ring via [`ReturnBuf`].
#[blaze::handle]
#[blaze::linear]
#[derive(Debug, PartialEq, Eq)]
pub struct LeasedBuf<'io> {
    group: RingGroupId,
    buf_id: RingBufId,
    len: u32,
    _brand: Brand<'io>,
}

const _: () = assert!(size_of::<LeasedBuf<'_>>() <= 12);

impl<'io> LeasedBuf<'io> {
    /// Buffer group (`bgid`) that owns this leased Rx slot.
    #[inline]
    pub const fn group(&self) -> RingGroupId {
        self.group
    }

    /// Buffer slot identifier (`bid`) within the owning [`BufferRing`].
    #[inline]
    pub const fn buf_id(&self) -> RingBufId {
        self.buf_id
    }

    /// Number of received bytes populated in this leased buffer.
    #[inline]
    pub const fn len(&self) -> u32 {
        self.len
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Wraps this lease in a linear [`ReturnBuf`] request to hand it back to its ring.
    #[blaze::consumes]
    #[inline]
    pub fn into_return(self) -> ReturnBuf<'io> {
        ReturnBuf { lease: self }
    }

    #[cfg(test)]
    #[inline]
    pub(crate) const fn new_for_ring(
        group: RingGroupId,
        buf_id: RingBufId,
        len: u32,
        brand: Brand<'io>,
    ) -> Self {
        Self { group, buf_id, len, _brand: brand }
    }
}

/// Linear [`Request`] that returns a [`LeasedBuf`] back to its kernel provided-buffer ring.
#[blaze::linear]
#[derive(Debug, PartialEq, Eq)]
pub struct ReturnBuf<'io> {
    pub lease: LeasedBuf<'io>,
}

impl<'io> ReturnBuf<'io> {
    #[inline]
    pub const fn new(lease: LeasedBuf<'io>) -> Self {
        Self { lease }
    }

    #[blaze::consumes]
    #[inline]
    pub fn into_lease(self) -> LeasedBuf<'io> {
        self.lease
    }
}

impl<'io> Request for ReturnBuf<'io> {
    type Answer<'s> = Result<(), LeaseError>;
}

/// Typed failure returned by [`BufferRing`] operations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LeaseError {
    /// No free provided buffers remain in the ring.
    RingEmpty,
    /// Returned buffer id (or ring group) is not valid for this ring.
    InvalidBufId { buf_id: RingBufId },
    /// Returned buffer slot is already free in the ring (double-return).
    AlreadyReturned { buf_id: RingBufId },
    /// Drain check found unreturned buffer leases still outstanding.
    LeakedLeases { outstanding: u16 },
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            LeaseError::RingEmpty => write!(f, "provided-buffer ring has no free Rx buffers"),
            LeaseError::InvalidBufId { buf_id } => {
                write!(f, "invalid buffer id {} for provided-buffer ring", buf_id.0)
            }
            LeaseError::AlreadyReturned { buf_id } => {
                write!(f, "buffer id {} was already returned to provided-buffer ring", buf_id.0)
            }
            LeaseError::LeakedLeases { outstanding } => {
                write!(
                    f,
                    "provided-buffer ring still has {outstanding} unreturned lease(s) outstanding"
                )
            }
        }
    }
}

/// Planted-bug seam for [`BufferRing`] controls (rules 43 & 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LeasePlant {
    /// Planted bug: accept returning a slot that is already free (misses double-return).
    SkipDoubleReturnCheck,
    /// Planted bug: skip checking `outstanding == 0` during [`BufferRing::verify_drained_with`]
    /// (misses leaked leases).
    SkipLeakCheck,
}

impl Plants for LeasePlant {}

/// Fixed-capacity bitset-tracked kernel provided-buffer ring (`IORING_REGISTER_PBUF_RING`).
///
/// Tracks up to `count` (`0..=u16::MAX`) Rx buffer slots in a compact bitset backed by an explicit
/// caller allocator `A`, branded with invariant [`Brand<'io>`].
#[derive(Debug)]
pub struct BufferRing<'io, A: Allocator = Alloc<'io>> {
    group: RingGroupId,
    count: u16,
    outstanding: u16,
    next_word: u16,
    brand: Brand<'io>,
    in_flight_bits: Vec<u64, A>,
}

impl<'io, A: Allocator> BufferRing<'io, A> {
    /// Allocates a new provided-buffer ring with `count` available Rx slots in `alloc`.
    pub fn new_in(group: RingGroupId, count: u16, brand: Brand<'io>, alloc: A) -> Self {
        let words = usize::from(count).div_ceil(64);
        let in_flight_bits = Vec::from_elem_in(0u64, words, alloc);
        Self { group, count, outstanding: 0, next_word: 0, brand, in_flight_bits }
    }

    /// Buffer group identifier (`bgid`) of this ring.
    #[inline]
    pub const fn group(&self) -> RingGroupId {
        self.group
    }

    /// Total number of Rx buffer slots managed by this ring.
    #[inline]
    pub const fn capacity(&self) -> u16 {
        self.count
    }

    /// Number of Rx buffer leases currently checked out and not yet returned.
    #[inline]
    pub const fn outstanding(&self) -> u16 {
        self.outstanding
    }

    /// Number of Rx buffer slots currently free in the ring.
    #[inline]
    pub const fn available(&self) -> u16 {
        self.count.saturating_sub(self.outstanding)
    }

    /// Reports whether `buf_id` is currently leased out.
    #[inline]
    pub fn is_in_flight(&self, buf_id: RingBufId) -> bool {
        if buf_id.0 >= self.count {
            return false;
        }
        let idx = usize::from(buf_id.0);
        let word = idx / 64;
        let bit = 1u64 << (idx % 64);
        (self.in_flight_bits[word] & bit) != 0
    }

    /// Acquires one free Rx buffer slot from the ring for a completion of `len` bytes.
    pub fn acquire_for_rx(&mut self, len: u32) -> Result<LeasedBuf<'io>, LeaseError> {
        if self.outstanding >= self.count {
            return Err(LeaseError::RingEmpty);
        }
        let words = self.in_flight_bits.len();
        if words == 0 {
            return Err(LeaseError::RingEmpty);
        }
        let start = usize::from(self.next_word) % words;
        let total = usize::from(self.count);
        for offset in 0..words {
            let w = (start + offset) % words;
            let base = w * 64;
            let valid_mask = if base + 64 <= total {
                u64::MAX
            } else {
                let rem = total - base;
                (1u64 << rem) - 1
            };
            let free_mask = valid_mask & !self.in_flight_bits[w];
            if free_mask != 0 {
                let bit = free_mask.trailing_zeros() as usize;
                let id = (base + bit) as u16;
                self.in_flight_bits[w] |= 1u64 << bit;
                self.outstanding += 1;
                self.next_word = w as u16;
                return Ok(LeasedBuf {
                    group: self.group,
                    buf_id: RingBufId(id),
                    len,
                    _brand: self.brand,
                });
            }
        }
        Err(LeaseError::RingEmpty)
    }

    /// Returns a leased Rx buffer to the ring, consuming the linear [`ReturnBuf`] request.
    #[blaze::consumes]
    #[inline]
    pub fn release(&mut self, req: ReturnBuf<'io>) -> Result<(), LeaseError> {
        self.release_with(req, NoPlant)
    }

    /// Returns a leased Rx buffer through the [`Planted`] seam for planted-bug controls.
    #[blaze::consumes]
    pub fn release_with<P: Planted<LeasePlant>>(
        &mut self,
        req: ReturnBuf<'io>,
        plant: P,
    ) -> Result<(), LeaseError> {
        let lease = req.into_lease();
        if lease.group != self.group || lease.buf_id.0 >= self.count {
            return Err(LeaseError::InvalidBufId { buf_id: lease.buf_id });
        }
        let idx = usize::from(lease.buf_id.0);
        let word = idx / 64;
        let bit = 1u64 << (idx % 64);
        let was_in_flight = (self.in_flight_bits[word] & bit) != 0;
        if !was_in_flight {
            if !plant.has(LeasePlant::SkipDoubleReturnCheck) {
                return Err(LeaseError::AlreadyReturned { buf_id: lease.buf_id });
            }
            return Ok(());
        }
        self.in_flight_bits[word] &= !bit;
        self.outstanding = self.outstanding.saturating_sub(1);
        Ok(())
    }

    /// Verifies that every leased Rx buffer has been returned to the ring (`outstanding == 0`).
    #[inline]
    pub fn verify_drained(&self) -> Result<(), LeaseError> {
        self.verify_drained_with(NoPlant)
    }

    /// Verifies ring drain through the [`Planted`] seam for planted-bug controls.
    #[inline]
    pub fn verify_drained_with<P: Planted<LeasePlant>>(&self, plant: P) -> Result<(), LeaseError> {
        if !plant.has(LeasePlant::SkipLeakCheck) && self.outstanding != 0 {
            return Err(LeaseError::LeakedLeases { outstanding: self.outstanding });
        }
        Ok(())
    }
}

impl<'io, A: Allocator> Handler<ReturnBuf<'io>> for BufferRing<'io, A> {
    type Error = Infallible;

    fn perform<Out>(
        &mut self,
        request: ReturnBuf<'io>,
        deliver: impl FnOnce(Perform<Result<(), LeaseError>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let res = self.release(request);
        Ok(deliver(Perform::Answered(res)))
    }
}
