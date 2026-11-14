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

pub(crate) mod facts;
mod filed;
mod image;
mod key;
mod late;
mod reads;
mod serve;
pub(crate) mod signatures;
pub mod given;
pub mod found;
#[cfg(test)]
mod tests;

use base_db::{Crate, Db};

pub use crate::deps::signatures::{BODY_KEY_DOMAIN, IdKind, PortableId, PortableIds, body_key};
pub use crate::deps::{
    filed::{checked_reads, closure_expansion_reads, file_by_reads, find_by_reads, outside_reads},
    image::{
        Held, IMAGE_SCHEMA, ImageBuf, ImageError, ImageRef, Part, PartBytes, Portable,
        write_image,
    },
    key::{
        Ineligible, KEY_DOMAIN, KeyInputs, KeyWalk, PRODUCER, PlanBase, absolute, eligible,
        key_inputs, keys, sealed, why_not_eligible,
    },
    late::{
        ImageParts, LATE_ENV, LATE_INCLUDE, LATE_INCLUDE_BYTES, LATE_INCLUDE_STR,
        LATE_OPTION_ENV, LateSeen, LateValue,
    },
    reads::{READS_DOMAIN, Reads, SLOTS, image_key, slot},
    serve::{
        ImageStore, KEPT_DOMAIN, PAGES_DOMAIN, ReadThrough, Refused, Served, StoreError, Stored,
        StoredOf, Which, filing_keys, front_keys, image_parts, kept_key, pages_key, serve, store,
        withdrawn,
    },
};
pub use crate::nameres::frozen::Mode;
/// `blaze_record`, the form what is filed by a crate's reads is kept in: for a caller of
/// [`find_by_reads`], which counts the searches in a [`record::Tally`] it owns and reports
/// ([`record::Counts`]).
pub use blaze_record as record;
pub use crate::nameres::frozen::pages::{PageSource, Paged, is_paged};

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
    /// The image's lang items part, as name resolution reads it ([`crate::nameres::frozen`]).
    lang_items: crate::nameres::frozen::Frozen,
    /// Under `Mode::Check` and `Mode::Verify` the crate's def map record whole, which what is
    /// resolved or read is compared with; never when serving.
    def_map: Option<crate::nameres::frozen::Frozen>,
    /// The def map pages the crate is read from: under `Mode::Serve` and `Mode::Verify`.
    pages: Option<Paged>,
}

impl Image {
    /// The image held in `bytes[range]`, a section of a record (a `Surface` record:
    /// crates/blaze-front/src/surface.rs) held where the store keeps it, opened in place:
    /// what a crate read from its surface is given ([`give_image`]). `base` and `key` are the
    /// record's address (the key the image was written under, [`write_image`], is `key`);
    /// `mode` is [`Mode::Serve`], and `pages` the crate's def map pages
    /// ([`crate::nameres::frozen::pages::encode`]'s bytes, another section of the record).
    ///
    /// `range` starts at a multiple of [`blaze_format::ALIGN`] of the record, whose bytes
    /// start aligned ([`Held`]). Each part's range is kept as an offset of the whole record
    /// (its offset in the image, plus `range.start`), so every reader slices `bytes` as it
    /// does a store's image ([`Image::view`], `PartBytes::In`).
    ///
    /// Floor: opening a crate is one check of the image's header (the format header and
    /// schema, the root's key compared, each of [`Part::ALL`]'s bounds and alignment checked),
    /// no copy and no decode; a part is read where it is when a query asks for it. Here:
    /// exactly that. The check reads the image's head (its root, 40 bytes, and a 12-byte
    /// record a part) and nothing of a part's bytes or of the pages; the holder and the pages'
    /// source are counted, not copied. Giving it ([`give_image`]) then hashes the lang items
    /// part once for the fact its slot is read by.
    ///
    /// Errors, each naming what: `range` outside the record ([`ImageError::OutOfRecord`]) or
    /// not aligned ([`ImageError::Unaligned`]); bytes written under another key
    /// ([`ImageError::Key`]) or not an image ([`ImageError::Format`], [`ImageError::Parts`]);
    /// a mode other than [`Mode::Serve`], or no pages ([`ImageError::Mode`]): `Check` and
    /// `Verify` compare with the def map's record whole, which a surface does not hold, and a
    /// served crate's def map is read from its pages alone.
    pub fn in_place(
        base: base_db::ContentId,
        key: base_db::ContentId,
        bytes: ImageBuf,
        range: std::ops::Range<usize>,
        mode: Mode,
        pages: Option<Paged>,
    ) -> Result<Image, ImageError> {
        let record = bytes.bytes();
        if range.start > range.end || range.end > record.len() {
            return Err(ImageError::OutOfRecord { range, len: record.len() });
        }
        if !range.start.is_multiple_of(blaze_format::ALIGN) {
            return Err(ImageError::Unaligned { start: range.start });
        }
        let pages = match (mode, pages) {
            (Mode::Serve, Some(pages)) => pages,
            (Mode::Serve, None) => {
                return Err(ImageError::Mode {
                    mode,
                    why: "a crate served from an image reads its def map from its pages, and none were given",
                });
            }
            (Mode::Check | Mode::Verify, _) => {
                return Err(ImageError::Mode {
                    mode,
                    why: "it compares with the crate's def map record whole, which a surface does not hold",
                });
            }
        };
        // clone: a `Range<usize>`, which is not `Copy`
        let image = ImageRef::open(&record[range.clone()], key)?;
        // Each part's offset in the image, made an offset of the record.
        let parts = image
            .ranges(&record[range.clone()])
            .map(|part| part.start + range.start..part.end + range.start);
        let lang_items = crate::nameres::frozen::Frozen {
            bytes: PartBytes::of(&bytes, &parts, Part::LangItems),
            mode,
        };
        Ok(Image { base, key, bytes, parts, mode, lang_items, def_map: None, pages: Some(pages) })
    }

    /// The image, read in place: a slice of the buffer [`serve`] or [`Image::in_place`]
    /// checked a part, so a reader of one signature pays no check of the image.
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

/// The kind of a crate's slot holding its [`Image`] ([`given`]): given by the driver
/// ([`give_image`]) for a dependency it maps the image of, and for the crate being built when
/// its surface key hit (its own image, written by the build before: spec F).
///
/// The slot is read by a fact that says only that the crate is served and how
/// ([`image_slot_fact`]): the image is read item by item and each reader notes the fact of the
/// item it read ([`blaze_memo::read`] in [`signatures`]'s and the frozen pages' accessors), so
/// a read list holds the facts of the items read, never the whole image's digest.
pub struct ImageSlot;

/// The fact the image slot of the crate of stable id `krate_id` is read by: that the crate is
/// served from an image, in `mode`. Not the image's key: what was read of the image is listed
/// item by item.
pub fn image_slot_fact(krate_id: base_db::ContentId, mode: Mode) -> blaze_memo::Fact {
    let mode_byte = [mode as u8];
    blaze_memo::Fact {
        id: base_db::ContentId::of("hir-def image slot", &krate_id.0),
        digest: base_db::ContentId::of("hir-def image slot mode", &mode_byte),
    }
}

/// Gives `krate`, whose stable id is `krate_id`, what it is read from, in `edit`: `image`, as
/// [`serve`] found it, in the crate's slot [`ImageSlot`] (read by [`image_slot_fact`]); its
/// lang items, its def map pages (`Mode::Serve`, `Mode::Verify`) and its def map whole
/// (`Mode::Check`, `Mode::Verify`) go in name resolution's slots of the crate
/// (`nameres::frozen`'s `LangItemsSlot`, `PagesSlot`, `DefMapSlot`), each read by its own
/// fact. Called by the driver in the edit that commits the snapshot it serves the crate in,
/// never during a build.
///
/// A gift lasts until it is replaced or taken (`base_db::Edit::take_given`), so a slot this
/// image has no part for (a def map whole under `Mode::Serve`) is taken: none is left from an
/// image given before in another mode.
pub fn give_image(
    edit: &mut base_db::Edit<'_>,
    krate: Crate,
    krate_id: base_db::ContentId,
    image: Image,
) {
    use crate::nameres::frozen::{self, DefMapSlot, Frozen, LangItemsSlot, pages};
    let key = krate.key(edit.db());
    let lang_items = &image.lang_items;
    let fact = frozen::frozen_fact(krate_id, "lang items", lang_items);
    // clone: a `PartBytes` handle on the image's buffer, once per crate given an image
    given::give::<LangItemsSlot, Frozen>(edit, krate, fact, lang_items.clone());
    if let Some(def_map) = &image.def_map {
        let fact = frozen::frozen_fact(krate_id, "def map", def_map);
        // clone: as above
        given::give::<DefMapSlot, Frozen>(edit, krate, fact, def_map.clone());
    } else {
        edit.take_given::<DefMapSlot>(key);
    }
    if let Some(paged) = &image.pages {
        let fact = pages::pages_fact(krate_id, paged);
        // clone: the pages' key and an `Arc` of their source
        given::give::<pages::PagesSlot, Paged>(edit, krate, fact, paged.clone());
    } else {
        edit.take_given::<pages::PagesSlot>(key);
    }
    let slot = image_slot_fact(krate_id, image.mode);
    given::give::<ImageSlot, Image>(edit, krate, slot, image);
}

/// Takes from the crate of `key`, in `edit`, everything [`give_image`] gives it: the image, its
/// lang items, its def map pages and its def map whole. What the driver does for each crate of
/// [`Served::withdrawn`] (keys, as a gift is taken by key): from the commit the crate is
/// resolved from its files. Whether it held any.
///
/// # Panics
///
/// Where the world has no crate `key` (`base_db::Edit::take_given`).
pub fn withdraw_image(edit: &mut base_db::Edit<'_>, key: base_db::CrateKey) -> bool {
    use crate::nameres::frozen::{DefMapSlot, LangItemsSlot, pages};
    // Each taken, none skipped by a short circuit.
    let taken = [
        edit.take_given::<ImageSlot>(key),
        edit.take_given::<LangItemsSlot>(key),
        edit.take_given::<DefMapSlot>(key),
        edit.take_given::<pages::PagesSlot>(key),
    ];
    taken.contains(&true)
}

/// The image `krate` is read from, if it is: what every reader of one crate's image goes
/// through. Reads the crate's own slot ([`ImageSlot`]), whose fact says only that the crate is
/// served and how ([`image_slot_fact`]): each item read of the image notes its own
/// ([`note_item_read`]).
///
/// This is also the served-else-lowered predicate ([`crate::signatures::Serve`]): a crate is
/// served exactly when it has an image here, which the driver gives to a dependency it maps and
/// to the crate being built when its surface key hit.
#[inline]
pub fn image_of(db: &Db, krate: Crate) -> Option<&Image> {
    krate.given::<ImageSlot, Image>(db)
}

/// Whether `krate` is served from an image ([`image_of`]): read in place, never lowered.
#[inline]
pub fn is_served(db: &Db, krate: Crate) -> bool {
    image_of(db, krate).is_some()
}

/// The image `krate` is read from, if it is.
pub fn image(db: &Db, krate: Crate) -> Option<ImageRef<'_>> {
    image_of(db, krate).map(Image::view)
}

/// What is done with the image of `krate`, if it has one: [`Mode::Serve`] reads it in place of
/// computing, [`Mode::Check`] computes as well and records a difference, [`Mode::Verify`]
/// writes what was read and records a difference from the bytes.
pub fn mode(db: &Db, krate: Crate) -> Option<Mode> {
    image_of(db, krate).map(Image::mode)
}

/// Notes the read of one item of a crate's image: the item's stable id (`item`, the id the
/// image's index finds it by) and the digest of its record's bytes. What every accessor that
/// reads an item of an image in place calls, so a read list names the item and not the image
/// ([`ImageSlot`]'s fact says only the mode). `domain` is a hash domain, a constant.
pub fn note_item_read(db: &Db, domain: &'static str, item: base_db::ContentId, record: &[u8]) {
    let fact = blaze_memo::Fact { id: item, digest: base_db::ContentId::of(domain, record) };
    blaze_memo::read(blaze_memo::Database::memo(db), fact);
}

/// The stable key of `id`, a `const` item of a crate's modules ([`signatures`]): the same in
/// every process and checkout. `None` for a constant in a block.
pub fn const_key(db: &Db, id: crate::ConstId) -> Option<base_db::ContentId> {
    signatures::const_key(db, id)
}
