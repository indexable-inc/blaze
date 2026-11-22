//! Where a request's head or a response's body is written: blaze-own's vector, in the caller's
//! allocator, or `alloc`'s (blaze-http's client keeps its buffers in one).

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_own::{AllocFailure, Vec};

/// A byte buffer the writer and the parser append to.
pub trait Out {
    /// Room for `more` bytes, or why there is none.
    fn try_reserve_more(&mut self, more: usize) -> Result<(), AllocFailure>;
    /// `bytes` appended, or why they could not be.
    fn try_put(&mut self, bytes: &[u8]) -> Result<(), AllocFailure>;
}

impl<B: Allocator> Out for Vec<u8, B> {
    fn try_reserve_more(&mut self, more: usize) -> Result<(), AllocFailure> {
        self.try_reserve(more)
    }

    fn try_put(&mut self, bytes: &[u8]) -> Result<(), AllocFailure> {
        self.try_extend_from_slice(bytes)
    }
}

/// Deprecated (C1, no global heap): kept while blaze-http's client (client.rs, the body piece
/// handed to its asker) appends to `alloc`'s `Vec`; it goes when that buffer is a blaze-own
/// `Vec<u8, A>` in a named allocator.
#[expect(
    blaze::implicit_allocator,
    reason = "`alloc`'s `Vec` is on the global heap: kept until blaze-http's client moves to blaze-own's `Vec` (W1 list)"
)]
impl Out for alloc::vec::Vec<u8> {
    fn try_reserve_more(&mut self, more: usize) -> Result<(), AllocFailure> {
        self.try_reserve(more).map_err(|_| AllocFailure::CapacityOverflow { what: "u8", len: self.len(), additional: more })
    }

    fn try_put(&mut self, bytes: &[u8]) -> Result<(), AllocFailure> {
        self.try_reserve_more(bytes.len())?;
        self.extend_from_slice(bytes);
        Ok(())
    }
}
