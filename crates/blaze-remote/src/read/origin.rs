//! What the read path asks of an origin, and the one loop that carries a [`Reader`]'s asks to
//! one. An origin is the smallest thing a static object store gives: a byte range of a named
//! object. It lists nothing and writes nothing. A folder (blaze-cli's `FolderOrigin`, through
//! bstd's files) and the simulation's origin (this module's tests) are the two built; an HTTP
//! origin through blaze-h1 is the next (remote-read.md, "Not built").

use blaze_ids::ContentId;
use blaze_own::{Alloc, Vec};

use super::form::Object;
use super::machine::{Ask, BatchError, OriginError, Poll, Reader};

pub trait Origin {
    /// Bytes of `object` from `at` into `into` (as many as it holds there, at most
    /// `into.len()`), done by `deadline_ns` on the clock the loop is given or failed.
    fn read(&mut self, object: &Object, at: u64, into: &mut [u8], deadline_ns: u64) -> Result<usize, OriginError>;
}

/// One batch of `keys` read through `reader` from `origin`, each ask made in turn: an origin
/// whose read blocks (a folder) is asked one range at a time, and each reply is checked against
/// its deadline on `clock` when it returns, a late one not used. Answers are then in `reader`.
pub fn fetch<O: Origin + ?Sized>(reader: &mut Reader<'_>, origin: &mut O, clock: &mut dyn FnMut() -> u64, keys: &[ContentId], alloc: Alloc<'_>) -> Result<(), BatchError> {
    reader.begin(keys, clock())?;
    let mut asks: Vec<Ask, Alloc<'_>> = Vec::new_in(alloc);
    // Each turn makes at least one ask or ends the batch, and a batch makes at most
    // `asks_most` asks: the loop is bounded (rule 18).
    loop {
        asks.clear();
        match reader.poll(clock(), &mut asks) {
            Poll::Done => return Ok(()),
            Poll::Waiting { .. } => {}
        }
        for ask in asks.iter() {
            let Some(into) = reader.buffer(ask.id) else { continue };
            let result = origin.read(&ask.object, ask.at, into, ask.deadline_ns);
            let now = clock();
            reader.replied(ask.id, result, now);
        }
    }
}
