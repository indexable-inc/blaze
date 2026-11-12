//! An image being made, as a value that has to end (docs/v2/plan/design/restore.md, "Persisted
//! state that is hard to get wrong", the write path): [`Txn`] holds the sections written so
//! far, and ends in [`Txn::commit`], which hands the finished image to the file's
//! [`Placer`], or in [`Txn::abort`], which writes nothing.
//!
//! - **Linear.** `Txn` is `#[blaze::linear]` (crates/hir-ty-linear): an image begun and left on
//!   an early return, with neither called, does not compile. A panic that unwinds past one
//!   drops its sections, which is an abort: nothing of it reached the placer.
//! - **The one way to the root.** [`Placer::place`] is this crate's own, and `commit` is its
//!   one caller outside the placer's simulation: no code writes the image's file without a
//!   `Txn` that it began and whose sections it filled. The placer's rename is the commit point
//!   (`place.rs`); what `commit` returns only says the placing began.

use blaze_core::Effect;

use crate::{Builder, Jobs, Stable, TooLarge, place::Placer};

/// An image begun: its sections so far. Ended by [`Txn::commit`] or [`Txn::abort`].
#[blaze::linear]
#[must_use = "an image begun is committed to its placer or aborted"]
pub struct Txn {
    builder: Builder,
}

/// A commit asked of the placer: the image's length. Its end is the placer's
/// [`crate::place::Outcome`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Asked {
    pub len: usize,
}

impl Txn {
    /// An image with no section yet.
    pub fn begin() -> Txn {
        Txn { builder: Builder::new() }
    }

    /// The image's sections, for each owner to write its own.
    pub fn sections(&mut self) -> &mut Builder {
        &mut self.builder
    }

    /// Finishes the image (its spine and CRCs, on `jobs`) and starts placing it at `placer`'s
    /// file. An image too large to lay out is the error, and nothing was asked of the placer.
    #[blaze::consumes]
    pub fn commit<S: Stable>(
        self,
        placer: &mut Placer<S>,
        jobs: Jobs<'_>,
        effects: &mut Vec<Effect>,
    ) -> Result<Asked, TooLarge> {
        let Txn { builder } = self;
        let bytes = builder.finish_on(jobs)?;
        let len = bytes.len();
        placer.place(bytes, jobs, effects);
        Ok(Asked { len })
    }

    /// Ends the image with nothing written: the file stays the image it was.
    #[blaze::consumes]
    pub fn abort(self) {
        let Txn { builder } = self;
        drop(builder);
    }
}

#[cfg(test)]
mod tests {
    use blaze_core::{FileName, Tag};
    use blaze_format::schema;

    use super::*;
    use crate::{Image, SectionId, serial};

    /// A commit asks the placer for exactly the image its sections make, and an abort asks
    /// for nothing.
    #[test]
    fn a_commit_places_the_image_its_sections_make_and_an_abort_places_nothing() {
        let file = FileName::new("state/db/image").unwrap();
        let mut placer: Placer<Vec<u8>> = Placer::new(file, Tag(1), None);
        let mut effects = Vec::new();

        let mut aborted = Txn::begin();
        aborted.sections().add(SectionId::PLANE_HISTORY, schema("txn test 1"), &[1, 2, 3]);
        aborted.abort();
        assert!(effects.is_empty() && !placer.busy(), "an abort asks the placer nothing");

        let mut txn = Txn::begin();
        txn.sections().add(SectionId::PLANE_HISTORY, schema("txn test 1"), &[1, 2, 3]);
        let asked = txn.commit(&mut placer, &serial, &mut effects).expect("a small image");
        assert!(placer.busy(), "the placing began");
        let [Effect::Write { bytes, .. }] = effects.as_slice() else {
            panic!("a first image is one whole write: {effects:?}")
        };
        assert_eq!(bytes.len(), asked.len);
        // clone: the test's copy, to open
        let image = Image::open(bytes.clone()).expect("the image the sections make");
        assert_eq!(image.sections().len(), 1);
    }
}
