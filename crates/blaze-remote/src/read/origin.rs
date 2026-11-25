//! What the read path asks of an origin, and the one loop that carries a [`Reader`]'s asks to
//! one. An origin is the smallest thing a static object store gives: a byte range of a named
//! object. It lists nothing and writes nothing. Multi-ask waves are dispatched together through
//! [`Origin::read_batch`] so pooled HTTP/HTTPS origins execute ranged batches in parallel while
//! serial origins inherit the default loop over [`Origin::read`].

use blaze_ids::ContentId;
use blaze_own::{Alloc, Vec};

use super::form::Object;
use super::machine::{Ask, BatchError, OriginError, Poll, Reader};

/// One slot in a parallel batch read ([`Origin::read_batch`]): the ask's id, object, range,
/// destination buffer (borrowed directly from the reader's slot — zero copy), and deadline.
pub struct BatchRead<'a> {
    pub id: u32,
    pub object: Object,
    pub at: u64,
    pub into: &'a mut [u8],
    pub deadline_ns: u64,
}

/// Outcome of one slot in [`Origin::read_batch`], tagged with the clock reading when it completed
/// so each ask's deadline is checked independently against its own arrival time.
#[derive(Clone, Copy, Debug)]
pub struct BatchOutcome {
    pub result: Result<usize, OriginError>,
    pub at_ns: u64,
}

pub trait Origin {
    /// Bytes of `object` from `at` into `into` (as many as it holds there, at most
    /// `into.len()`), done by `deadline_ns` on the clock the loop is given or failed.
    fn read(&mut self, object: &Object, at: u64, into: &mut [u8], deadline_ns: u64) -> Result<usize, OriginError>;

    /// Reads a wave of `reads` in parallel where supported (up to the origin's bounded
    /// concurrency), writing each slot's result and arrival timestamp into `outcomes` at the
    /// matching index.
    ///
    /// Default implementation executes each read serially via [`Origin::read`].
    fn read_batch(
        &mut self,
        reads: &mut [BatchRead<'_>],
        clock: &mut dyn FnMut() -> u64,
        outcomes: &mut [BatchOutcome],
    ) {
        for (req, out) in reads.iter_mut().zip(outcomes.iter_mut()) {
            let result = self.read(&req.object, req.at, req.into, req.deadline_ns);
            *out = BatchOutcome { result, at_ns: clock() };
        }
    }
}

/// One batch of `keys` read through `reader` from `origin`: each wave's asks are dispatched
/// together via [`Origin::read_batch`] (in parallel on pooled HTTP/HTTPS origins, serially on
/// single-range origins), and each reply is checked against its deadline on `clock` when it
/// completes, a late one not used. Answers are then in `reader`.
pub fn fetch<O: Origin + ?Sized>(reader: &mut Reader<'_>, origin: &mut O, clock: &mut dyn FnMut() -> u64, keys: &[ContentId], alloc: Alloc<'_>) -> Result<(), BatchError> {
    reader.begin(keys, clock())?;
    let mut asks: Vec<Ask, Alloc<'_>> = Vec::new_in(alloc);
    let mut ids: Vec<u32, Alloc<'_>> = Vec::new_in(alloc);
    let mut outcomes: Vec<BatchOutcome, Alloc<'_>> = Vec::new_in(alloc);
    // Each turn makes at least one ask or ends the batch, and a batch makes at most
    // `asks_most` asks: the loop is bounded (rule 18).
    loop {
        asks.clear();
        match reader.poll(clock(), &mut asks) {
            Poll::Done => return Ok(()),
            Poll::Waiting { .. } => {}
        }
        if asks.len() <= 1 {
            for ask in asks.iter() {
                let Some(into) = reader.buffer(ask.id) else { continue };
                let result = origin.read(&ask.object, ask.at, into, ask.deadline_ns);
                let now = clock();
                reader.replied(ask.id, result, now);
            }
            continue;
        }
        ids.clear();
        for ask in asks.iter() {
            ids.push(ask.id);
        }
        outcomes.clear();
        let fallback_now = clock();
        outcomes.resize(
            ids.len(),
            BatchOutcome { result: Err(OriginError::Unreadable), at_ns: fallback_now },
        );
        let dispatched = reader.with_pending_buffers(ids.as_slice(), alloc, |batch| {
            let count = batch.len();
            if let Some(out_slice) = outcomes.as_mut_slice().get_mut(..count) {
                origin.read_batch(batch, clock, out_slice);
            }
            count
        });
        for i in 0..dispatched {
            if let (Some(&id), Some(&outcome)) = (ids.as_slice().get(i), outcomes.as_slice().get(i)) {
                reader.replied(id, outcome.result, outcome.at_ns);
            }
        }
    }
}
