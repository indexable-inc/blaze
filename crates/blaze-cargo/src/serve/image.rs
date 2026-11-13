//! The daemon's one image file (docs/v2/plan/design/restore.md, "The state is one mapped
//! image"): the VFS's sections and the plane's history, made in one builder and put at the file
//! by [`Placer`], then mapped back. No engine state is in it (docs/v2/plan/design/end-state.md:
//! the in-memory engine is a cache of what this process verified and can be dropped): a start
//! maps the VFS, replays the journal's tail and answers from the certificate. What one machine
//! knows beside the store (the tool's hash, the outputs placed, each checkout's carry session)
//! is a journal mark (`serve::local`), durable with the head it belongs to, image or not. An
//! image of the layout before this one still holds the database's, the symbols', the library
//! trees' and the carry sessions' sections ([`SectionId::RETIRED`] among them): they are
//! never read.
//!
//! - An image is asked for after a cold build, by a snapshot and at a stop. One asked while
//!   another is being made is made once that one is mapped.
//! - Only what differs from the image on disk is written ([`blaze_image::place`]): the file is
//!   cloned, the 64 KiB units that changed written into the clone, and the clone synced and
//!   renamed over it. A one-line edit's image wrote all 190 MiB of itself in 1.5 to 3.5 s
//!   (2026-11-10), most of it the VFS's tree and the library syntax trees, unchanged.
//! - No request waits behind an image: the one asked between requests ([`Images::ask_between`])
//!   is let go when a request comes ([`Images::let_go`]) and asked again once the daemon is
//!   idle, unless its commit is already under way. One a snapshot or a stop waits for
//!   ([`Images::ask`]) is never let go.
//! - The VFS writes its sections first. While its journal moves to another image
//!   ([`ImageError::Busy`]) nothing is written and the VFS is flushed; the image is tried again
//!   each turn. Busy when the VFS has nothing left to do is an error, and the old image stays.
//! - The VFS's image names each file by its content ID and holds none of its bytes: its texts
//!   are the store's ([`StoreTexts`], kind `Text`), put by the VFS before it writes. While a
//!   put is not durable ([`ImageError::Storing`]) nothing is written and the store is flushed;
//!   the image is tried again each turn, and Storing with nothing left in flight is an error.
//! - The texts the image on disk names are a root of the store's collector by construction
//!   ([`FileSet`], the pin [`TEXT_PIN`]): before the VFS writes, the set of the texts the next
//!   image names is listed and pinned beside the set of the image on disk, and nothing is
//!   written until both are durable ([`Prepared::Flush`]: the store is flushed and the image
//!   tried again each turn, and with nothing left in flight it is [`Unmade::Unpinned`]). The
//!   set before is let go of only once the image after it is renamed into place. A start pins
//!   again what the image it used names ([`text_pin_at_start`]).
//! - Once the write is renamed into place the VFS is told ([`Vfs::image_durable`]) and
//!   flushed, which rewrites its journal onto the image (its marks with it); the file is then
//!   mapped and opened, and its shape logged.
//! - The plane's history is a [`History`], in the section [`blaze_history::section`] lays out.
//!   One of another blaze starts an empty history ([`Unlearned`]), which changes how work is
//!   ordered and admitted, never what is built; one that is corrupt or does not decode is an
//!   error ([`HistoryError`]).
//! - An image that is not made is an [`ImageFailed`]: the one before it stays on disk, and
//!   whoever asked (the image between requests, a snapshot, a stop) fails with it.
//! - Each image made is logged once mapped: its hash, bytes and sections ([`Shape`]), and how
//!   long it took to make, write and map.

#![forbid(unsafe_code)]

use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use blaze_cas::{
    Handle, Key, Kind, Mismatch, Store, Tickets,
    sets::{FileSet, FileSetStart, Prepared},
};
use blaze_core::{Completion, Done, Effect, FileName, IoError, Mapping, Tag};
use blaze_history::{DecodeError, History, section::HistoryError};
use blaze_ids::ContentId;
use blaze_image::{
    Image, OpenError, Refused, SectionId, Stable, TooLarge, Txn,
    place::{Outcome, Placer, Wrote},
};
use blaze_vfs::{
    Bytes, FaultWhy, ImageError, ImageMark, StoreFault, TEXT_SCHEMA, Texts, Vfs,
};

/// The VFS's file texts in the daemon's store (/tmp/plan/vfs-image-next.md): a file's bytes
/// under `Key::of(TEXT_SCHEMA, id, &[])`, kind [`Kind::Text`], read in place through the
/// store's pool as a [`blaze_cas::Pinned`] (never an mmap of a store file), put through a
/// writer of its own and durable at the store's next flush.
pub struct StoreTexts {
    handle: Handle,
    /// The puts not yet seen durable.
    pending: blaze_core::lock::Lock<Tickets>,
}

impl StoreTexts {
    /// The texts of `store`, which must be open.
    pub fn new(store: &mut Store) -> Arc<StoreTexts> {
        let handle = Handle::new(store, std::path::PathBuf::from("/"));
        let pending = blaze_core::lock::Lock::new("serve.image.texts", Tickets::default());
        // alloc: once a daemon, shared by the VFS and the image maker.
        Arc::new(StoreTexts { handle, pending })
    }

    fn key(id: &ContentId) -> Key {
        Key::of(TEXT_SCHEMA, *id, &[])
    }
}

impl Texts for StoreTexts {
    fn get(&self, id: &ContentId) -> Result<Option<Bytes>, StoreFault> {
        match self.handle.pinned(&StoreTexts::key(id)) {
            Ok(pinned) => Ok(pinned.map(blaze_cas::Pinned::into_bytes)),
            Err(corrupt) => {
                tracing::error!(target: "blaze", file = %format!("{id:x}"), %corrupt, "the store's text of a VFS file failed its check");
                Err(StoreFault { file: *id, why: FaultWhy::Corrupt })
            }
        }
    }

    fn has(&self, id: &ContentId) -> bool {
        self.handle.has(&StoreTexts::key(id))
    }

    fn put(&self, id: &ContentId, bytes: &[u8]) -> Result<(), StoreFault> {
        match self.handle.put(StoreTexts::key(id), Kind::Text, bytes) {
            Ok(ticket) => {
                self.pending.lock().add(ticket);
                Ok(())
            }
            Err(mismatch) => {
                tracing::error!(target: "blaze", file = %format!("{id:x}"), ?mismatch, "the store holds other bytes under a VFS file's text key");
                Err(StoreFault { file: *id, why: FaultWhy::Mismatch })
            }
        }
    }

    fn durable(&self) -> bool {
        let mut pending = self.pending.lock();
        pending.retain_pending(|ticket| self.handle.durable(ticket));
        pending.is_empty()
    }
}

/// The name of the store's pin of the texts the daemon image on disk names ([`FileSet`]).
pub const TEXT_PIN: &str = "vfs image";
const TEXT_PIN_WHY: &str = "the file texts the daemon image on disk names";

/// The id the listings of an image's texts are of.
fn text_set() -> ContentId {
    ContentId::of("blaze v2 daemon image texts 1", b"")
}

/// The store's pin of the image's texts at a start: `used` is the ids of the files the image
/// the start used names, as its own columns hold them ([`Vfs::image_file_ids`]), none when it
/// used none. Says what it found: the set listed as the run that wrote the image left it; none
/// listed (a store of the layout before, whose collections did not keep an image's texts),
/// with how many of them the store lacks; or no image, the pin let go.
pub fn text_pin_at_start<'a>(
    store: &mut Store,
    used: Option<impl Iterator<Item = &'a ContentId>>,
    when: u64,
) -> Result<(FileSet, FileSetStart), String> {
    // alloc: once a start, a key a file the image names
    let keys = used.map(|ids| ids.map(StoreTexts::key).collect::<Vec<Key>>());
    let (pin, start) = FileSet::start(store, TEXT_PIN, TEXT_PIN_WHY, text_set(), keys, when)
        .map_err(|mismatch| {
            format!("the store's pin {TEXT_PIN:?} of the daemon image's texts at a start: {}", Refusal(&mismatch))
        })?;
    match start {
        FileSetStart::NoFile => {
            tracing::info!(target: "blaze", pin = TEXT_PIN, "no daemon image used: the pin of its texts let go");
        }
        FileSetStart::Listed { keys, .. } => {
            tracing::info!(target: "blaze", pin = TEXT_PIN, keys, "the daemon image's texts pinned as the run that wrote it left them");
        }
        FileSetStart::Unlisted { keys, missing, .. } => crate::serve::said::logged!(WARN,
            "blaze serve: the store held no pin {TEXT_PIN:?} of the daemon image's {keys} file texts (a store of the layout before): pinned now; it lacks {missing} of them, and a read of one fails naming its file"
        ),
    }
    Ok((pin, start))
}

/// A put the store refused, for a person.
struct Refusal<'a>(&'a Mismatch);

impl fmt::Display for Refusal<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Mismatch { key, kept, given } = self.0;
        write!(f, "the store holds other bytes under the listing record {:x} (kept {kept:x}, given {given:x})", key.0)
    }
}

/// Why the plane's history starts empty from an image: the image was written by another blaze.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Unlearned {
    /// The image has no history section, or one of another schema.
    Refused(Refused),
    /// The section holds another version of [`History::encode`]'s bytes.
    Version,
    /// The start was asked to leave the history unused
    /// ([`super::daemon::RESTORE_WITHOUT`]).
    Asked,
}

impl fmt::Display for Unlearned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unlearned::Refused(refused) => write!(f, "{refused}"),
            Unlearned::Version => {
                write!(f, "the image's {} is of another blaze", SectionId::PLANE_HISTORY)
            }
            Unlearned::Asked => write!(
                f,
                "the start was asked to leave the image's {} unused ({} names `history`)",
                SectionId::PLANE_HISTORY,
                super::daemon::RESTORE_WITHOUT
            ),
        }
    }
}

/// The plane's history in `image`, as `config` sizes it, and why it starts empty when the image
/// was written without one or with one of another blaze. The daemon does not start on an
/// error: a section that is corrupt or does not decode.
pub fn history<S: Stable>(
    image: &Image<S>,
    config: blaze_history::Config,
) -> Result<(History, Option<Unlearned>), HistoryError> {
    match blaze_history::section::restore(image, config) {
        Ok(history) => Ok((history, None)),
        Err(HistoryError::Refused(refused)) => {
            Ok((History::new(config), Some(Unlearned::Refused(refused))))
        }
        Err(HistoryError::Decode(DecodeError::Version)) => {
            Ok((History::new(config), Some(Unlearned::Version)))
        }
        Err(error) => Err(error),
    }
}

/// An image as the logs name it: its hash, its bytes and its sections by what they hold,
/// largest first.
pub struct Shape<'a, S: Stable>(pub &'a Image<S>);

impl<S: Stable> fmt::Display for Shape<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let image = self.0;
        // alloc: a log line's rows, once an image written or restored
        let mut held: Vec<(&'static str, u64, u32)> = Vec::new();
        for section in image.sections() {
            let name = section.id.name();
            match held.iter_mut().find(|(it, ..)| *it == name) {
                Some((_, len, count)) => {
                    *len += section.len;
                    *count += 1;
                }
                None => held.push((name, section.len, 1)),
            }
        }
        held.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        write!(f, "{} ({}, {} sections", image.hash(), Mib(image.len()), image.sections().len())?;
        for (name, len, count) in held {
            write!(f, "; {name} {}", Mib(len))?;
            if count > 1 {
                write!(f, " in {count}")?;
            }
        }
        write!(f, ")")
    }
}

/// Bytes in MiB, to a tenth.
struct Mib(u64);

impl fmt::Display for Mib {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1} MiB", self.0 as f64 / (1u64 << 20) as f64)
    }
}

/// Milliseconds, to a tenth.
pub(crate) fn ms(it: Duration) -> String {
    // alloc: a log line's figure
    format!("{:.1} ms", it.as_secs_f64() * 1e3)
}

/// Why an image asked was not made.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Unmade {
    /// The VFS's journal is moving to another image and nothing is left in flight to wait on.
    Busy,
    /// Texts the image names are not durable in the store and nothing is left in flight to
    /// wait on.
    Storing,
    /// The store refused the text of a file the image would name.
    Unstored(StoreFault),
    /// The store's pin of the texts the image would name ([`TEXT_PIN`]) is not durable and
    /// nothing is left in flight to wait on.
    Unpinned,
    /// The store refused a record of the listing of the texts the image would name.
    Unlisted(Mismatch),
    TooLarge(TooLarge),
    Write(IoError),
    Map(IoError),
    /// The image just written does not open.
    Open(OpenError),
}

/// An image of `file` not made, and why.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ImageFailed {
    pub file: FileName,
    pub why: Unmade,
}

impl fmt::Display for ImageFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file = &self.file;
        match &self.why {
            Unmade::Busy => write!(f, "the daemon image /{file}: {}", ImageError::Busy),
            Unmade::Storing => write!(f, "the daemon image /{file}: {}", ImageError::Storing),
            Unmade::Unstored(fault) => {
                write!(f, "the daemon image /{file}: {}", ImageError::Unstored(*fault))
            }
            Unmade::Unpinned => write!(
                f,
                "the daemon image /{file}: the store's pin {TEXT_PIN:?} of the texts it names is not durable, and nothing is in flight to make it so"
            ),
            Unmade::Unlisted(mismatch) => {
                write!(f, "the daemon image /{file}: listing the texts it names: {}", Refusal(mismatch))
            }
            Unmade::TooLarge(error) => write!(f, "the daemon image /{file}: {error}"),
            Unmade::Write(error) => write!(f, "writing the daemon image /{file}: {error}"),
            Unmade::Map(error) => {
                write!(f, "mapping the daemon image /{file} just written: {error}")
            }
            Unmade::Open(error) => write!(f, "the daemon image /{file} just written: {error}"),
        }
    }
}

/// For the API's error text, at the edge.
impl From<&ImageFailed> for String {
    fn from(failed: &ImageFailed) -> String {
        failed.to_string()
    }
}

/// What an image is made of: the parts of the daemon that write its sections.
pub struct Parts<'a> {
    pub vfs: &'a mut Vfs,
    /// The store the VFS's texts are put in: flushed while they are not durable.
    pub store: &'a mut Store,
    pub predict: &'a History,
    pub effects: &'a mut Vec<Effect>,
}

enum State {
    Idle,
    /// The VFS's journal is moving to another image: made once it can be.
    Waiting,
    /// `made` is how long the image took to make; the placing of its `bytes` began at `since`.
    Writing {
        mark: ImageMark,
        made: Duration,
        since: Instant,
        bytes: usize,
    },
    /// A request came while the image asked between requests was being placed: its placing
    /// ends with the effect in flight and the removal of its clone.
    LettingGo,
    /// `written` is how many of the image's bytes were written, the rest shared.
    Mapping {
        made: Duration,
        wrote: Duration,
        written: u64,
        since: Instant,
    },
}

pub struct Images {
    file: FileName,
    /// Puts each image at the file; its effects carry `write`.
    placer: Placer<Mapping>,
    write: Tag,
    map: Tag,
    state: State,
    /// Another image was asked while one was being made.
    again: bool,
    /// A snapshot or a stop waits for the image asked: it is not let go for a request.
    waited: bool,
    /// The file system's refusal to share an image's blocks was said, once a daemon.
    said_unshared: bool,
    /// The last image made, mapped, until the loop takes it between requests.
    mapped: Option<Arc<Image<Mapping>>>,
    /// Why the last image asked was not made; cleared when one is asked again.
    failed: Option<ImageFailed>,
    /// How many images it has made and handed to the driver to write, in its life.
    made: u64,
    /// The store the VFS's texts are in: what each image's files are put in, and what the VFS
    /// settles on with an image.
    texts: Arc<StoreTexts>,
    /// The store's pin of the texts the image on disk names, and the next one's while it is
    /// made.
    text_pin: FileSet,
}

impl Images {
    /// The image at `file`, written and mapped with the tags `tags` and `tags + 1`. `on_disk`
    /// is the file as the start mapped it, none for no file: the first image shares its blocks.
    /// `texts` is the store the VFS was restored with, and `text_pin` its pin of the texts the
    /// image the start used names ([`text_pin_at_start`]).
    pub fn new(
        file: FileName,
        tags: u64,
        on_disk: Option<Mapping>,
        texts: Arc<StoreTexts>,
        text_pin: FileSet,
    ) -> Images {
        Images {
            // clone: a name, once a daemon.
            placer: Placer::new(file.clone(), Tag(tags), on_disk),
            file,
            write: Tag(tags),
            map: Tag(tags + 1),
            state: State::Idle,
            again: false,
            waited: false,
            said_unshared: false,
            mapped: None,
            failed: None,
            made: 0,
            texts,
            text_pin,
        }
    }

    /// The pin of the image's texts lets go of the set of the image before once the image
    /// after it is in place: asked each time the store is at hand.
    fn settle_pin(&mut self, store: &mut Store) {
        if let Err(mismatch) = self.text_pin.settle(store, blaze_gc::record::now_ns()) {
            // clone: a refcount, once a refused pin.
            self.failed = Some(ImageFailed { file: self.file.clone(), why: Unmade::Unlisted(mismatch) });
        }
    }

    /// The store of the VFS's texts, as a settle on an image reads it.
    pub fn texts(&self) -> Arc<dyn Texts> {
        // clone: a count, once a settle.
        self.texts.clone()
    }

    /// How many images it has made and handed to the driver to write: what a test of the stop
    /// counts (a stop during an unchanged settle makes none of its own).
    pub fn made(&self) -> u64 {
        self.made
    }

    pub fn owns(&self, tag: Tag) -> bool {
        tag == self.write || tag == self.map
    }

    /// Asks for an image of the daemon as it is now, for someone who waits for it (a snapshot,
    /// a stop): it is made whatever comes.
    pub fn ask(&mut self, parts: Parts<'_>) {
        self.waited = true;
        self.ask_between(parts);
    }

    /// Asks for an image of the daemon as it is now, between requests (the settle's): a
    /// request that comes while it is made lets go of it ([`Images::let_go`]).
    pub fn ask_between(&mut self, mut parts: Parts<'_>) {
        self.failed = None;
        self.settle_pin(parts.store);
        match self.state {
            State::Idle => self.make(&mut parts, false),
            State::Waiting
            | State::Writing { .. }
            | State::LettingGo
            | State::Mapping { .. } => self.again = true,
        }
    }

    /// A request came: lets go of the image being made, if no one waits for it and its commit
    /// is not under way. `true` when it was let go: the file stays the image it was, nothing
    /// is mapped, and whoever asked asks again once the daemon is idle.
    pub fn let_go(&mut self) -> bool {
        if self.waited || self.again {
            return false;
        }
        match self.state {
            State::Waiting => {
                self.state = State::Idle;
                true
            }
            State::Writing { .. } if self.placer.let_go() => {
                self.state = State::LettingGo;
                true
            }
            State::Idle | State::Writing { .. } | State::LettingGo | State::Mapping { .. } => false,
        }
    }

    /// Makes what waits: an image whose VFS sections could not be written yet, or one asked
    /// while another was made. `idle` is whether the driver has no effect in flight or queued.
    pub fn step(&mut self, mut parts: Parts<'_>, idle: bool) {
        self.settle_pin(parts.store);
        match self.state {
            State::Waiting => self.make(&mut parts, idle),
            State::Idle if self.again => {
                self.again = false;
                self.make(&mut parts, false);
            }
            State::Idle | State::Writing { .. } | State::LettingGo | State::Mapping { .. } => {}
        }
    }

    fn make(&mut self, parts: &mut Parts<'_>, idle: bool) {
        let began = Instant::now();
        let _span =
            tracing::info_span!(target: "blaze", "image make", bytes = tracing::field::Empty)
                .entered();
        // The texts the image names are pinned, durably, before a byte of it is written.
        // alloc: once an image made, a key a file of the tree (the listing keeps a key a leaf)
        let keys: Vec<Key> = parts.vfs.file_ids().map(StoreTexts::key).collect();
        match self.text_pin.prepare(parts.store, keys, blaze_gc::record::now_ns()) {
            Ok(Prepared::Ready) => {}
            Ok(Prepared::Flush) => {
                let before = parts.effects.len();
                parts.store.flush(parts.effects);
                if idle && parts.effects.len() == before {
                    return self.fail(Unmade::Unpinned);
                }
                self.state = State::Waiting;
                return;
            }
            Err(mismatch) => return self.fail(Unmade::Unlisted(mismatch)),
        }
        // The image as a transaction (`blaze_image::Txn`, linear): every way out of here
        // commits it to the placer or aborts it, or this does not compile.
        let mut txn = Txn::begin();
        let mark = match parts.vfs.write_image(txn.sections(), &*self.texts) {
            Ok(mark) => mark,
            Err(ImageError::Busy) => {
                txn.abort();
                let before = parts.effects.len();
                parts.vfs.flush(parts.effects);
                if idle && parts.effects.len() == before {
                    return self.fail(Unmade::Busy);
                }
                self.state = State::Waiting;
                return;
            }
            Err(ImageError::Storing) => {
                txn.abort();
                let before = parts.effects.len();
                parts.store.flush(parts.effects);
                if idle && parts.effects.len() == before {
                    return self.fail(Unmade::Storing);
                }
                self.state = State::Waiting;
                return;
            }
            Err(ImageError::Unstored(fault)) => {
                txn.abort();
                return self.fail(Unmade::Unstored(fault));
            }
        };
        blaze_history::section::write(parts.predict, txn.sections());
        match txn.commit(&mut self.placer, &on_pool, parts.effects) {
            Ok(asked) => {
                _span.record("bytes", asked.len);
                self.made += 1;
                self.state = State::Writing {
                    mark,
                    made: began.elapsed(),
                    since: Instant::now(),
                    bytes: asked.len,
                };
            }
            Err(error) => self.fail(Unmade::TooLarge(error)),
        }
    }

    /// Takes a completion of its own.
    pub fn complete(&mut self, vfs: &mut Vfs, completion: Completion, effects: &mut Vec<Effect>) {
        if completion.tag == self.write {
            // One of the placing's effects: nothing changes until it says how it ended.
            let Some(outcome) = self.placer.complete(completion.done, effects) else { return };
            return self.placed(vfs, outcome, effects);
        }
        match (std::mem::replace(&mut self.state, State::Idle), completion.done) {
            (State::Mapping { made, wrote, written, since }, Done::Mapped(mapping)) => {
                // The file holds these bytes: the next image shares what it can of them.
                // clone: a count of the mapping, once an image.
                self.placer.holds(mapping.clone());
                match Image::open(mapping) {
                    Ok(image) => {
                        let (shape, mapped) = (Shape(&image), since.elapsed());
                        tracing::info!(target: "blaze", bytes = image.len(), written, sections = image.sections().len(), made_ms = made.as_millis() as u64, wrote_ms = wrote.as_millis() as u64, "image written");
                        // logged: the info event above, with its fields
                        eprintln!(
                            "blaze serve: wrote the daemon image /{} {shape}: made in {}, {} of it written in {} (the rest is the last image's blocks), mapped in {}",
                            self.file,
                            ms(made),
                            Mib(written),
                            ms(wrote),
                            ms(mapped)
                        );
                        self.mapped = Some(Arc::new(image));
                        self.rest();
                    }
                    Err(error) => self.fail(Unmade::Open(error)),
                }
            }
            (State::Mapping { .. }, Done::Failed(error)) => self.fail(Unmade::Map(error)),
            (_, done) => unreachable!("the daemon image's effect completed with {done:?}"),
        }
    }

    /// The placing ended with `outcome`.
    fn placed(&mut self, vfs: &mut Vfs, outcome: Outcome, effects: &mut Vec<Effect>) {
        match (std::mem::replace(&mut self.state, State::Idle), outcome) {
            (State::Writing { mark, made, since, .. }, Outcome::Placed(wrote)) => {
                // The image renamed into place is the one prepared last: its texts' set is the
                // durable image's, and the set before is let go of with the store's next turn.
                self.text_pin.placed();
                assert!(
                    vfs.image_durable(mark.name),
                    "the image written is the one the VFS staged last"
                );
                vfs.flush(effects);
                self.say_unshared(&wrote);
                // clone: an effect owns its file's name; once an image.
                effects.push(Effect::Map { file: self.file.clone(), tag: self.map });
                self.state = State::Mapping {
                    made,
                    wrote: since.elapsed(),
                    written: wrote.written,
                    since: Instant::now(),
                };
            }
            (State::Writing { .. }, Outcome::Failed(error)) => self.fail(Unmade::Write(error)),
            // Let go for a request: no failure, and the one who asked asks again.
            (State::LettingGo, Outcome::LetGo) => self.rest(),
            (_, outcome) => unreachable!("the daemon image's placing ended with {outcome:?}"),
        }
    }

    /// Says, once a daemon, that the file system shares no blocks, so each image is written
    /// whole.
    fn say_unshared(&mut self, wrote: &Wrote) {
        if let Some(error) = &wrote.unshared
            && !self.said_unshared
        {
            self.said_unshared = true;
            crate::serve::said::logged!(WARN,
                "blaze serve: the daemon image /{} is written whole ({}) each time, not only what changed: its file could not be cloned: {error}",
                self.file,
                Mib(wrote.len)
            );
        }
    }

    /// Idle: with no other image asked, no one waits any more.
    fn rest(&mut self) {
        if !self.again {
            self.waited = false;
        }
    }

    fn fail(&mut self, why: Unmade) {
        self.state = State::Idle;
        self.rest();
        // clone: a refcount, once a failed image.
        self.failed = Some(ImageFailed { file: self.file.clone(), why });
    }

    /// What it is doing, for a person waiting on it (`blaze stop`): none when idle with no
    /// image asked.
    pub fn doing(&self) -> Option<String> {
        let file = &self.file;
        Some(match self.state {
            State::Idle if self.again => format!(
                "making another daemon image /{file}: the daemon changed while the last was made"
            ),
            State::Idle => return None,
            State::Waiting => format!(
                "waiting for the VFS journal to move before making the daemon image /{file}"
            ),
            State::Writing { bytes, .. } => match self.again {
                false => format!("writing the daemon image /{file} ({} MiB)", bytes >> 20),
                true => {
                    format!("writing the daemon image /{file} ({} MiB), then another", bytes >> 20)
                }
            },
            State::LettingGo => format!(
                "letting go of the daemon image /{file} a request came during, then making another"
            ),
            State::Mapping { .. } => format!("mapping the daemon image /{file} it wrote"),
        })
    }

    /// Whether every image asked was made, or failed.
    pub fn done(&self) -> bool {
        matches!(self.state, State::Idle) && !self.again
    }

    /// Why the last image asked was not made.
    pub fn failed(&self) -> Option<&ImageFailed> {
        self.failed.as_ref()
    }

    /// The last image made, mapped: the loop takes it between requests.
    pub fn take_mapped(&mut self) -> Option<Arc<Image<Mapping>>> {
        self.mapped.take()
    }
}

/// [`blaze_image::Jobs`] on the worker pool ([`blaze_graph::par`]): `job(0)` to `job(n - 1)`,
/// back once all ran; a job's panic is resumed here.
// dyn: the shape of `blaze_image::Jobs`; one call per section job of an image, each a whole section's work
pub fn on_pool(n: usize, job: &(dyn Fn(usize) + Sync)) {
    blaze_graph::par::for_each(0..n, job);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use blaze_format::SchemaHash;
    use blaze_history::section::SCHEMA as HISTORY;
    use blaze_image::Builder;

    use super::*;

    #[test]
    fn jobs_on_the_pool_each_run_once() {
        let ran: Vec<AtomicUsize> = (0..100).map(|_| AtomicUsize::new(0)).collect();
        on_pool(ran.len(), &|at| _ = ran[at].fetch_add(1, Ordering::Relaxed));
        assert!(ran.iter().all(|it| it.load(Ordering::Relaxed) == 1));
    }

    #[test]
    fn a_jobs_panic_is_the_callers() {
        let caught = std::panic::catch_unwind(|| on_pool(8, &|at| assert_ne!(at, 5, "job five")));
        assert!(caught.is_err());
    }

    fn with_history(schema: SchemaHash, bytes: &[u8]) -> Image<Vec<u8>> {
        let mut builder = Builder::new();
        builder.add(SectionId::PLANE_HISTORY, schema, bytes);
        Image::open(builder.finish().unwrap()).unwrap()
    }

    fn empty() -> Vec<u8> {
        History::new(blaze_history::Config::default()).encode()
    }

    #[test]
    fn the_plane_history_survives_an_image() {
        let mut learned = History::new(blaze_history::Config::default());
        let key = blaze_graph::plane::Key([7; 32]);
        for n in 0..50u64 {
            learned.observe(blaze_graph::admission::kinds::COMPILE, Some(&key), n * 1000, n << 10);
        }
        let image = with_history(HISTORY, &learned.encode());
        let (restored, unlearned) = history(&image, blaze_history::Config::default()).unwrap();
        assert_eq!(restored.encode(), learned.encode());
        assert_eq!(unlearned, None);
    }

    #[test]
    fn a_history_of_another_blaze_or_none_starts_empty_saying_why() {
        let config = blaze_history::Config::default();
        let none = Image::open(Builder::new().finish().unwrap()).unwrap();
        let (restored, unlearned) = history(&none, config).unwrap();
        assert_eq!(restored.encode(), empty());
        assert!(
            matches!(unlearned, Some(Unlearned::Refused(Refused::Missing { .. }))),
            "{unlearned:?}"
        );
        // The schema of the section blaze-graph wrote before blaze-history owned it.
        let estimates = with_history(
            blaze_format::schema(
                "blaze plane history 1: Estimates::encode bytes, version blzest01",
            ),
            b"blzest01",
        );
        let (restored, unlearned) = history(&estimates, config).unwrap();
        assert_eq!(restored.encode(), empty());
        assert!(
            matches!(unlearned, Some(Unlearned::Refused(Refused::Schema { .. }))),
            "{unlearned:?}"
        );
        let older = with_history(HISTORY, b"blzhis00 and what followed");
        let (restored, unlearned) = history(&older, config).unwrap();
        assert_eq!(restored.encode(), empty());
        assert_eq!(unlearned, Some(Unlearned::Version));
    }

    #[test]
    fn an_image_not_made_names_the_file_and_why() {
        let failed = ImageFailed {
            file: FileName::new("state/db/image").unwrap(),
            why: Unmade::Write(IoError("no space left on device".to_owned())),
        };
        let text = String::from(&failed);
        assert!(text.contains("writing the daemon image /state/db/image"), "{text}");
        assert!(text.contains("no space left"), "{text}");
    }

    #[test]
    fn a_history_that_does_not_decode_stops_the_daemon_naming_it() {
        let mut cut = empty();
        cut.truncate(cut.len() - 1);
        let Err(error) = history(&with_history(HISTORY, &cut), blaze_history::Config::default())
        else {
            panic!("a cut history decoded")
        };
        assert!(matches!(error, HistoryError::Decode(_)), "{error:?}");
        assert!(error.to_string().contains("the plane's history"), "{error}");
    }

    /// The invariant: an image of the layout before this one (with the database's, the
    /// symbols', the library trees' and the carry sessions' sections) gives the same history
    /// as one with the history alone, and its other sections are never read: here they hold
    /// bytes no reader of theirs would take. Rule 26. Control: an image whose history section
    /// is cut is refused, so the test reads the section it says it reads.
    #[test]
    fn an_image_of_the_previous_layout_gives_its_history_and_its_engine_sections_go_unread() {
        let mut learned = History::new(blaze_history::Config::default());
        learned.observe(blaze_graph::admission::kinds::COMPILE, None, 1000, 1 << 10);
        let bytes = learned.encode();
        let mut builder = Builder::new();
        builder.add(SectionId::PLANE_HISTORY, HISTORY, &bytes);
        let junk = |n: u8| SchemaHash(blaze_ids::ContentId([n; 32]));
        for (at, retired) in SectionId::RETIRED.into_iter().enumerate() {
            builder.add(retired, junk(3 + at as u8), b"not trees, not sessions");
        }
        let before = Image::open(builder.finish().unwrap()).unwrap();
        let (restored, unlearned) = history(&before, blaze_history::Config::default()).unwrap();
        assert_eq!((restored.encode(), unlearned), (bytes.clone(), None));
        let shape = Shape(&before).to_string();
        assert!(shape.contains("a section no blaze writes now"), "{shape}");

        let cut = with_history(HISTORY, &bytes[..bytes.len() - 1]);
        assert!(history(&cut, blaze_history::Config::default()).is_err(), "a cut history is read");
    }
}

#[cfg(test)]
mod shape_tests {
    use blaze_format::SchemaHash;
    use blaze_image::Builder;

    use super::*;

    #[test]
    fn an_images_shape_names_its_hash_bytes_and_sections_largest_first() {
        let mut builder = Builder::new();
        builder.add(SectionId::PLANE_HISTORY, SchemaHash(blaze_ids::ContentId([1; 32])), &[7; 10]);
        builder.add(SectionId::VFS_TREE, SchemaHash(blaze_ids::ContentId([2; 32])), &vec![9; 3 << 20]);
        builder.add(SectionId::VFS_DISKS, SchemaHash(blaze_ids::ContentId([3; 32])), &[1; 100]);
        // A section of the layout before, which a shape names as such.
        builder.add(SectionId::RETIRED[5], SchemaHash(blaze_ids::ContentId([3; 32])), &[1; 100]);
        let image = Image::open(builder.finish().unwrap()).unwrap();
        let text = Shape(&image).to_string();
        assert!(text.starts_with(&format!("{} (", image.hash())), "{text}");
        assert!(
            text.contains(
                "4 sections; the VFS tree 3.0 MiB in 2; the symbol table, a section no blaze writes now 0.0 MiB"
            ),
            "{text}"
        );
        assert!(text.ends_with("the plane's history 0.0 MiB)"), "{text}");
    }
}
