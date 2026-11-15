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
mod std_census;
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
        ImageStore, KEPT_DOMAIN, Lowered, PAGES_DOMAIN, ReadThrough, Refused, Served, StoreError,
        Stored, StoredOf, Which, ast_site, filing_keys, module_path, front_keys, image_parts, kept_key, pages_key, serve,
        serve_found, serve_keeping, store, whole_pages, withdrawn,
    },
};
pub use crate::nameres::frozen::Mode;
/// `blaze_record`, the form what is filed by a crate's reads is kept in: for a caller of
/// [`find_by_reads`], which counts the searches in a [`record::Tally`] it owns and reports
/// ([`record::Counts`]).
pub use blaze_record as record;
pub use crate::nameres::frozen::pages::{KeyedFile, PageSource, Paged, is_paged};

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
    /// The image's diagnostic items part ([`Part::DiagnosticItems`]), as
    /// `diagnostic_item::crate_diagnostic_items` reads it.
    diagnostic_items: crate::nameres::frozen::Frozen,
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
    /// and diagnostic items parts once each for the facts their slots are read by.
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
        let diagnostic_items = crate::nameres::frozen::Frozen {
            bytes: PartBytes::of(&bytes, &parts, Part::DiagnosticItems),
            mode,
        };
        Ok(Image {
            base,
            key,
            bytes,
            parts,
            mode,
            lang_items,
            diagnostic_items,
            def_map: None,
            pages: Some(pages),
        })
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
/// lang items and its diagnostic items as located gifts (`nameres::frozen`'s `LangItemsSlot`,
/// `DiagnosticItemsSlot`: `base_db::Located`, their first gift marking nothing), its def map
/// pages (`Mode::Serve`, `Mode::Verify`) and its def map whole (`Mode::Check`, `Mode::Verify`)
/// in name resolution's slots of the crate (`PagesSlot`, `DefMapSlot`), each read by its own
/// fact. Called by the driver in the edit that commits the snapshot it serves the crate in,
/// never during a build.
///
/// The image and the pages stay slots, which mark the crate: what is read with them is not
/// what is computed without them. A served signature carries the image's block scopes, not a
/// lowering's (`signatures::Serve::lowering`), and `is_served` itself is read as a value (the
/// served-else-lowered predicate); a paged crate's def map holds `ScopeSlot::Paged` scopes read
/// a name at a time, not the resolved ones. So a crate whose arena holds lowered values and is
/// then given its image is made again: the floor of serving a crate already lowered, which
/// only not serving it (keeping it lowered while its arena lives) would avoid.
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
    use crate::nameres::frozen::pages::{KeyedFilesSlot, keyed_files};
    // A crate given by its address before is given by its files now: the files it was given
    // with are taken, so its pages' files are looked up and checked again (`Dec::files`).
    if keyed_files(edit.db(), krate).is_some() {
        let key = krate.key(edit.db());
        edit.take_given::<KeyedFilesSlot>(key);
    }
    give_image_parts(edit, krate, krate_id, image);
}

/// The proof a crate's record was read at its address in the request a snapshot is built for:
/// what [`give_keyed`] alone takes, so a crate's pages are trusted as given (no file of it
/// looked up or compared) only where the driver checked the record's address
/// (`blaze_keyed::record::check_read`: the address is the hash of the record's key and the
/// answers its shape got, so a record at its address is fresh by the key: rule verify-once).
///
/// Made by `blaze_front::surface::Opened::give` alone, from a `blaze_front::surface::ReadAt`,
/// which only a `blaze_keyed::front::Table` the driver handed a task makes; raw bytes from
/// anywhere else are given by [`give_image`], whose pages check every file. Valid for the
/// snapshot of the request whose universe held that table, and kept by nothing: it is spent
/// in the give.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadAtAddress(base_db::ContentId);

impl ReadAtAddress {
    /// The proof that the record at `address` was read there (see the type). Called by
    /// `blaze_front::surface::Opened::give`, and by this crate's tests, alone.
    pub fn vouched(address: base_db::ContentId) -> ReadAtAddress {
        ReadAtAddress(address)
    }

    /// The record's address.
    pub fn address(self) -> base_db::ContentId {
        self.0
    }
}

/// Why a crate could not be given by its address ([`give_keyed`]), naming the crate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyedError {
    /// The image is not served from pages: a crate given by its address reads its def map
    /// from its pages, which name its files ([`Image::in_place`] under [`Mode::Serve`]).
    NotPaged { krate: String },
    /// The pages' header or files segment does not read, or names a file twice, or no file id
    /// is left for one of its files.
    Files { krate: String, why: String },
}

impl std::fmt::Display for KeyedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyedError::NotPaged { krate } => write!(
                f,
                "{krate} is not given by its address: its image is not served from def map pages"
            ),
            KeyedError::Files { krate, why } => {
                write!(f, "{krate} is not given by its address: its pages' files: {why}")
            }
        }
    }
}

/// Gives `krate`, whose stable id is `krate_id`, what it is read from as [`give_image`] does,
/// trusting its pages as read at `read`'s address: the keyed way of giving a crate. No file of
/// the crate is looked up or compared, none needs to be in a source root or to have text, and
/// nothing of the crate is parsed unless something read off its syntax is asked for.
///
/// The files its pages name are made here (`pages::make_keyed_files`): its root as the
/// crate's root file, every other a new file id with no text in the root's source root, each
/// with the content the pages were made from; they are given in the crate's slot
/// `pages::KeyedFilesSlot`, which [`crate::nameres::frozen`]'s readers name files by in place
/// of looking them up. A file of another crate the pages name is that crate's own, given with
/// it. What still needs a file's text is refused, typed, where it is asked for and is not there
/// (`pages::demand_text`, a `frozen::Unreadable` naming the crate and the file): a
/// demanded body, the items of a trait or an impl. The driver
/// that has a file's text sets it on the file id it was given ([`keyed_files`]).
///
/// Reads the pages' header and files segment (a few hundred bytes a crate) and nothing else.
///
/// # Panics
///
/// Where the crate's root file is in no source root: every crate's root is.
pub fn give_keyed(
    edit: &mut base_db::Edit<'_>,
    krate: Crate,
    krate_id: base_db::ContentId,
    image: Image,
    read: ReadAtAddress,
) -> Result<(), KeyedError> {
    use crate::nameres::frozen::pages::{self, KeyedFiles, KeyedFilesSlot};
    // alloc: the crate's name, once a crate given, for its error
    let name = krate.named(edit.db()).to_string();
    let Some(paged) = image.pages.as_ref().filter(|_| image.mode == Mode::Serve) else {
        return Err(KeyedError::NotPaged { krate: name });
    };
    let keyed = pages::make_keyed_files(edit, krate, paged, read.address())
        .map_err(|why| KeyedError::Files { krate: name, why })?;
    let fact = pages::keyed_files_fact(krate_id, &keyed);
    give_image_parts(edit, krate, krate_id, image);
    given::give::<KeyedFilesSlot, KeyedFiles>(edit, krate, fact, keyed);
    // hir-expand's reads of a file's text (a declarative macro's expander, a file's index)
    // ask `pages::demand_text` first, in a database that keeps no entries too. One fact for
    // every crate: given again, it marks nothing.
    pages::give_demand(edit);
    Ok(())
}

/// The files `krate` was given by its address with ([`give_keyed`]), each by its name in the
/// crate's pages, with the file id this database gave it and the content it was made from:
/// what a driver sets a file's text on where it has it (a body of the crate it will lower).
/// `None` for a crate not given so.
pub fn keyed_files(db: &Db, krate: Crate) -> Option<&[KeyedFile]> {
    crate::nameres::frozen::pages::keyed_files(db, krate).map(|it| it.files())
}

/// [`give_image`] without taking the files a crate was given by its address with.
fn give_image_parts(
    edit: &mut base_db::Edit<'_>,
    krate: Crate,
    krate_id: base_db::ContentId,
    image: Image,
) {
    use crate::nameres::frozen::{
        self, DefMapSlot, DiagnosticItemsSlot, Frozen, LangItemsSlot, pages,
    };
    let key = krate.key(edit.db());
    let lang_items = &image.lang_items;
    let fact = frozen::frozen_fact(krate_id, "lang items", lang_items);
    // Located: its first gift marks no arena (`LangItemsSlot`'s promise).
    // clone: a `PartBytes` handle on the image's buffer, once per crate given an image
    given::give_located::<LangItemsSlot, Frozen>(edit, krate, fact, lang_items.clone());
    let diagnostic_items = &image.diagnostic_items;
    let fact = frozen::frozen_fact(krate_id, "diagnostic items", diagnostic_items);
    // Located, as the lang items are (`DiagnosticItemsSlot`'s promise).
    // clone: as above
    given::give_located::<DiagnosticItemsSlot, Frozen>(edit, krate, fact, diagnostic_items.clone());
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
/// lang items, its diagnostic items, its def map pages and its def map whole. What the driver
/// does for each crate of
/// [`Served::withdrawn`] (keys, as a gift is taken by key): from the commit the crate is
/// resolved from its files. Whether it held any.
///
/// # Panics
///
/// Where the world has no crate `key` (`base_db::Edit::take_given`).
pub fn withdraw_image(edit: &mut base_db::Edit<'_>, key: base_db::CrateKey) -> bool {
    use crate::nameres::frozen::{DefMapSlot, DiagnosticItemsSlot, LangItemsSlot, pages};
    // Each taken, none skipped by a short circuit.
    let taken = [
        edit.take_given::<ImageSlot>(key),
        edit.take_located::<LangItemsSlot>(key),
        edit.take_located::<DiagnosticItemsSlot>(key),
        edit.take_given::<DefMapSlot>(key),
        edit.take_given::<pages::PagesSlot>(key),
        // What [`give_keyed`] gives beside: none for a crate given by [`give_image`].
        edit.take_given::<pages::KeyedFilesSlot>(key),
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
