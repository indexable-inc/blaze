//! Dependency crates read from images (docs/v2/plan/design/frontend.md item 5; #1293, #1294).
//!
//! A dependency (a crate whose origin is not local, all of whose dependencies are dependencies
//! too) is read from its *image*: one [`blaze_format`] buffer holding what the front end needs
//! of the crate without its source: its frozen `DefMap` and lang items (the portable encoding of
//! [`crate::nameres::frozen`]) and its items' signatures ([`signatures`]). The image is keyed by
//! every input of the crate ([`key`]) and kept by the store; [`serve`] gives a database the
//! images the store has, and [`store`] writes those it lacks once the crates are resolved.
//!
//! On a hit nothing of the crate is parsed until something asks for a body (codegen of a
//! generic or `#[inline]` function, a function not in the code cache, a const evaluation:
//! [`crate::nameres::demand`]), and that parses the file as an outline, whose lazy bodies are
//! parsed one at a time as they are walked into (`syntax::SourceFile::parse_outline`).
//! `blaze check` of the workspace checks its own crates, which are never dependencies, fully.
//!
//! An image is read in place: its parts are slices of the one buffer the store holds, each
//! found by its tag with no copy, and a signature is decoded when its item is asked for, alone.
//!
//! What a crate reads that is known only once it is resolved (the process variables its
//! `env!`s read, the files and variables its proc-macro expansions read) is its [`reads`]: a
//! crate with any is filed under its key and the values of those reads now, the way a run of
//! user code is (`blaze_usercode::index`), so an image is used only while what it read is
//! unchanged. The key has no absolute path in it ([`key`]), so a checkout at another path, or
//! another machine through the global cache ([`ReadThrough`]), finds the same image.

mod filed;
mod image;
mod key;
mod reads;
mod serve;
pub(crate) mod signatures;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use base_db::{Crate, Db};
use blaze_hash::HashMap;

pub use crate::deps::signatures::{BODY_KEY_DOMAIN, IdKind, PortableId, PortableIds, body_key};
pub use crate::deps::{
    filed::{checked_reads, file_by_reads, find_by_reads},
    image::{Held, IMAGE_SCHEMA, ImageBuf, ImageError, ImageRef, Part, PartBytes, write_image},
    key::{KEY_DOMAIN, KeyInputs, PRODUCER, absolute, eligible, key_inputs, keys},
    reads::{READS_DOMAIN, Reads, SLOTS, image_key, slot},
    serve::{ImageStore, ReadThrough, Refused, Served, Stored, crate_name, serve, store},
};
pub use crate::nameres::frozen::Mode;

/// An image a database was given: the buffer, which [`serve`] opened and checked against the
/// key it is filed under, where its parts are, and what is done with it.
#[derive(Clone)]
pub struct Image {
    /// The crate's key ([`keys`]), which a later [`serve`] compares to keep the image.
    base: base_db::ContentId,
    key: base_db::ContentId,
    bytes: ImageBuf,
    parts: [std::ops::Range<usize>; Part::ALL.len()],
    mode: Mode,
}

impl Image {
    /// The image, read in place: three slices of the buffer [`serve`] checked, so a reader of
    /// one signature pays no check of the image.
    #[inline]
    pub fn view(&self) -> ImageRef<'_> {
        ImageRef::at(self.bytes.bytes(), &self.parts)
    }

    /// What holds the bytes the image is read from: what a value read from it in place borrows.
    pub fn bytes(&self) -> &ImageBuf {
        &self.bytes
    }

    /// The key the image is filed under: the crate's key ([`keys`]) for a crate with no
    /// [`Reads`], else that and the values its reads had.
    pub fn key(&self) -> base_db::ContentId {
        self.key
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }
}

/// Images compare by their key: the store gives the bytes of a key only after checking their
/// content hash, so equal keys are equal bytes. The crate's key is one of what they were filed
/// by, so equal filed keys are equal crate keys.
impl PartialEq for Image {
    fn eq(&self, other: &Image) -> bool {
        self.key == other.key && self.mode == other.mode
    }
}

impl Eq for Image {}

impl std::hash::Hash for Image {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
        self.mode.hash(state);
    }
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("key", &self.key)
            .field("len", &self.bytes.bytes().len())
            .field("mode", &self.mode)
            .finish()
    }
}

/// The images of the crates a database reads from them, set by [`serve`].
#[blaze_query::input(singleton, debug)]
pub struct DepImages {
    #[returns(ref)]
    pub images: Arc<HashMap<Crate, Image>>,
}

/// The image `krate` is read from, if it is: what every reader of one crate's image goes
/// through, so a [`serve`] that leaves the crate's image as it was re-runs none of its queries.
#[blaze_query::tracked(returns(ref))]
pub fn image_of(db: &Db, krate: Crate) -> Option<Image> {
    // clone: two buffer handles and a few words, once per crate per serve that changes the set
    DepImages::try_get(db)?.images(db).get(&krate).cloned()
}

/// The image `krate` is read from, if it is.
pub(crate) fn image(db: &Db, krate: Crate) -> Option<ImageRef<'_>> {
    image_of(db, krate).as_ref().map(Image::view)
}

/// What is done with the image of `krate`, if it has one: [`Mode::Serve`] reads it in place of
/// computing, [`Mode::Check`] computes as well and records a difference, [`Mode::Verify`]
/// writes what was read and records a difference from the bytes.
pub(crate) fn mode(db: &Db, krate: Crate) -> Option<Mode> {
    image_of(db, krate).as_ref().map(Image::mode)
}

/// The stable key of `id`, a `const` item of a crate's modules ([`signatures`]): the same in
/// every process and checkout. `None` for a constant in a block.
pub fn const_key(db: &Db, id: crate::ConstId) -> Option<base_db::ContentId> {
    signatures::const_key(db, id)
}
