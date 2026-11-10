//! The daemon's one image file (docs/v2/plan/design/restore.md, "The state is one mapped
//! image"): the database's sections, the VFS's, the plane's history and the symbols they name,
//! made in one builder and put at the file by [`Placer`], then mapped back for the database to
//! settle onto.
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
//! - Once the write is renamed into place the VFS is told ([`Vfs::image_durable`]) and
//!   flushed, which rewrites its journal onto the image; the file is then mapped and opened,
//!   for [`Database::settle`].
//! - The plane's history is a [`History`], in the section [`blaze_history::section`] lays out.
//!   One of another blaze starts an empty history ([`Unlearned`]), which changes how work is
//!   ordered and admitted, never what is built; one that is corrupt or does not decode is an
//!   error ([`HistoryError`]).
//! - An image that is not made is an [`ImageFailed`]: the one before it stays on disk, and
//!   whoever asked (the cold build's settle, a snapshot, a stop) fails with it.
//! - Each image made is logged once mapped: its hash, bytes and sections ([`Shape`]), and how
//!   long it took to make, write and map.

#![forbid(unsafe_code)]

use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use blaze_core::{Completion, Done, Effect, FileName, IoError, Mapping, Tag};
use blaze_history::{DecodeError, History, section::HistoryError};
use blaze_image::{
    Builder, Corrupt, Image, OpenError, Refused, SectionId, Stable, TooLarge,
    place::{Outcome, Placer, Wrote},
};
use blaze_query::{Database, DbError};
use blaze_vfs::{ImageError, ImageMark, Vfs};

/// Why the plane's history starts empty from an image: the image was written by another blaze.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Unlearned {
    /// The image has no history section, or one of another schema.
    Refused(Refused),
    /// The section holds another version of [`History::encode`]'s bytes.
    Version,
}

impl fmt::Display for Unlearned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unlearned::Refused(refused) => write!(f, "{refused}"),
            Unlearned::Version => {
                write!(f, "the image's {} is of another blaze", SectionId::PLANE_HISTORY)
            }
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

/// The library syntax trees `image` stores, read in place; `None` when it was written without
/// them. A section of another schema or layout, or one that does not read, is the error saying
/// why, for the start to report: its library files are then parsed.
pub fn trees(image: &Arc<Image<Mapping>>) -> Result<Option<base_db::trees::Trees>, String> {
    let section = match image.section(SectionId::SYNTAX_TREES, base_db::trees::SCHEMA) {
        Ok(section) => section,
        Err(Refused::Missing { .. }) => return Ok(None),
        Err(refused) => return Err(refused.to_string()),
    };
    let bytes = image
        .whole_bytes(&section)
        .map_err(|corrupt| format!("the image's {}: {corrupt}", SectionId::SYNTAX_TREES))?;
    base_db::trees::Trees::open(bytes).map(Some).map_err(|error| error.to_string())
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
    /// A group of the image the VFS was restored from failed its check.
    Vfs(Corrupt),
    Db(DbError),
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
            Unmade::Vfs(corrupt) => {
                write!(f, "the daemon image /{file}: {}", ImageError::Corrupt(*corrupt))
            }
            Unmade::Db(error) => {
                write!(f, "the database's sections of the daemon image /{file}: {error}")
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
    pub db: &'a Database,
    /// A kept front end's database: its library files' parses are the image's library syntax
    /// trees. With none kept the image has no such section, and the next start parses them.
    pub front: Option<&'a base_db::Db>,
    pub vfs: &'a mut Vfs,
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
    /// The last image made, mapped, until the database settles onto it.
    mapped: Option<Arc<Image<Mapping>>>,
    /// Why the last image asked was not made; cleared when one is asked again.
    failed: Option<ImageFailed>,
    /// How many images it has made and handed to the driver to write, in its life.
    made: u64,
}

impl Images {
    /// The image at `file`, written and mapped with the tags `tags` and `tags + 1`. `on_disk`
    /// is the file as the start mapped it, none for no file: the first image shares its blocks.
    pub fn new(file: FileName, tags: u64, on_disk: Option<Mapping>) -> Images {
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
        }
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
        let mut builder = Builder::new();
        let mark = match parts.vfs.write_image(&mut builder) {
            Ok(mark) => mark,
            Err(ImageError::Busy) => {
                let before = parts.effects.len();
                parts.vfs.flush(parts.effects);
                if idle && parts.effects.len() == before {
                    return self.fail(Unmade::Busy);
                }
                self.state = State::Waiting;
                return;
            }
            Err(ImageError::Corrupt(corrupt)) => return self.fail(Unmade::Vfs(corrupt)),
        };
        let mut live = intern::Live::new();
        if let Err(error) = parts.db.write_image(&mut builder, &mut live) {
            return self.fail(Unmade::Db(error));
        }
        blaze_history::section::write(parts.predict, &mut builder);
        if let Some(front) = parts.front {
            if let Some(failed) = front.trees().and_then(|it| it.failed()) {
                tracing::warn!(
                    failed,
                    "a stored library tree failed to read: the image carries none of the old ones on"
                );
            }
            // alloc: the section, once an image.
            let mut trees = Vec::new();
            base_db::trees::write_section(front, &mut trees);
            builder.add(SectionId::SYNTAX_TREES, base_db::trees::SCHEMA, &trees);
        }
        intern::write_symbols(&live, &mut builder);
        match builder.finish_on(&on_pool) {
            Ok(bytes) => {
                _span.record("bytes", bytes.len());
                let len = bytes.len();
                self.placer.place(bytes, &on_pool, parts.effects);
                self.made += 1;
                self.state = State::Writing {
                    mark,
                    made: began.elapsed(),
                    since: Instant::now(),
                    bytes: len,
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
            eprintln!(
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

    /// The last image made, for the database to settle onto.
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
}

#[cfg(test)]
mod shape_tests {
    use blaze_format::SchemaHash;

    use super::*;

    #[test]
    fn an_images_shape_names_its_hash_bytes_and_sections_largest_first() {
        let mut builder = Builder::new();
        builder.add(SectionId::PLANE_HISTORY, SchemaHash(blaze_ids::ContentId([1; 32])), &[7; 10]);
        builder.add(
            SectionId::DB_MEMOS,
            SchemaHash(blaze_ids::ContentId([2; 32])),
            &vec![9; 3 << 20],
        );
        builder.add(SectionId::interned(1), SchemaHash(blaze_ids::ContentId([3; 32])), &[1; 100]);
        builder.add(SectionId::interned(2), SchemaHash(blaze_ids::ContentId([3; 32])), &[1; 100]);
        let image = Image::open(builder.finish().unwrap()).unwrap();
        let text = Shape(&image).to_string();
        assert!(text.starts_with(&format!("{} (", image.hash())), "{text}");
        assert!(
            text.contains(
                "4 sections; the database's memos 3.0 MiB; an interned table 0.0 MiB in 2"
            ),
            "{text}"
        );
        assert!(text.ends_with("the plane's history 0.0 MiB)"), "{text}");
    }
}
