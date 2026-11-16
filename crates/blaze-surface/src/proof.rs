//! [`HasText`]: the proof that a task holds one crate's text.

use blaze_id::CrateItemKey;

/// That the task holding it holds the text of the crate it names: what every [`crate::Source`]
/// method takes, so a question that reads text cannot be written where only records may be read
/// (docs/v2/plan/design/bottom.md, point 4). A task for a dependency holds none, and so asks
/// only [`crate::Items`].
///
/// It names its crate, and a `Source` of another crate refuses it
/// ([`crate::SurfaceError::WrongCrate`]); so it is the crate's 32-byte key, not zero-sized. It
/// is made only by [`HasText::assume`], which the driver that opened the crate's files calls.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct HasText {
    krate: CrateItemKey,
}

impl HasText {
    /// The proof that this task holds the text of `krate`.
    ///
    /// # Safety
    ///
    /// Only the driver that opened `krate`'s files for the task that will hold the proof calls
    /// this, once per task, after the files are open. A proof made anywhere else lets a task
    /// that holds only records read text: a dependency's question then falls into source, the
    /// class of bug the two traits exist to rule out, and its result is keyed by records it
    /// never read. Nothing memory-unsafe follows; `unsafe` marks the call sites a review and the
    /// lints look for, as blaze-id's `Portable` does.
    pub unsafe fn assume(krate: CrateItemKey) -> HasText {
        HasText { krate }
    }

    /// The crate whose text the task holds.
    pub fn krate(&self) -> CrateItemKey {
        self.krate
    }
}
