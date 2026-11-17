//! Records by key (drive.md, "The system side", "Store"; "D114 rulings", "The store under the
//! command"): ONE store a user, blaze-cas's plain store at `<home>/.blaze/v2/store`, read and
//! written by every checkout and worktree, records read IN PLACE ([`plain_place`]). A get is
//! a probe of the segments the head names, each mapped at its first probe; the request's first
//! pool task maps the rest ([`Store::map_all`]); a put is copied once into the command's arena;
//! the commit, the seal and a bounded compaction come after the answer is printed
//! ([`plain_place::after_answer`], called by main.rs's outer function).
//!
//! A key missed by the store is asked of a named origin when one is named; what comes back is
//! checked by the read path against the signed head's chain of digests, put, then answered.
//! What is not fetched is a typed miss: one line on stderr says what and why for each kind, and
//! the request computes the record locally. That is not a fallback (remote-read.md, "Not
//! fetched"): the local computation is the record's definition and the origin a cache of it.
//!
//! THE OLD HOME. The stand-in's folders (the home's `.blaze/v2/shared` and `.blaze/v2/own`, a
//! checkout's `.blaze/v2/packs`) are an old form: looked for once (a root records the look),
//! reported by name ([`old_home`]), never read, migrated or deleted; their records are made
//! again (rule 26).

use blaze_drive::profile::Profile;
use blaze_drive::record::Stored;
use blaze_drive::{Key, Kind, Name};
use blaze_ids::ContentId;

use core::fmt;
use core::time::Duration;

use blaze_cas::plain::{Lent, Settings};
use blaze_core::Driver;
use blaze_own::Alloc;
use blaze_remote::PublicKey;
use blaze_remote::read::{Answer, BatchError, Ed25519Keys, HeadCheck, Limits, Namespace, Object, Origin, OriginError, Reader, SignatureWhy, Unfetched, fetch};
use bstd::Stopped;
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::fs::{Dir, Fs, FsError, FsKind, ReadWrite};
use std::alloc::Global;
use bstd::mem::Bytes;
use bstd::out::Out;
use bstd::path::Path;
use bstd::Deadline;
use bstd::net::{Addr, Net};
use bstd::url::Url;
use blaze_h1::Range;
use bstd::requests::{Finished, Http, HttpKind, NO_REDIRECTS, Request, Wire};
use bstd::roots::Roots;

use crate::CliError;

/// The command's store on blaze-cas's plain store: `Lent`, `PlainPlace`, the proof.
pub(crate) mod plain_place;
pub use plain_place::{AfterAnswer, Checked, Counts, Facts, Held, PlaceError, PlainPlace};

/// The seam the request reads and writes records through. `'m` is the borrow of the command's
/// [`Lent`]: a [`Held<'m>`] outlives every later get and put (blaze-cas plain/lent.rs).
pub trait Store<'m> {
    /// The record at `key`, in place; `None` when there is none.
    fn get(&mut self, cx: &Cx, key: Key) -> Result<Option<Held<'m>>, CliError>;
    /// The record at `key` copied into `into`, for a reader whose bytes must live in its own
    /// frame (a pool job's); `false` when there is none.
    fn get_into(&mut self, cx: &Cx, key: Key, into: &mut Bytes) -> Result<bool, CliError> {
        match self.get(cx, key)? {
            Some(held) => {
                into.try_extend(held.bytes).map_err(|stopped| CliError::Store(StoreFailure::Copy(stopped)))?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
    /// `bytes` at `key`. A key already held with equal bytes is left as it is (rule 25); other
    /// bytes are refused (rule 21).
    fn put(&mut self, cx: &Cx, key: Key, bytes: &[u8]) -> Result<(), CliError>;
    /// Every segment the head names mapped, and every parked one checked: asked once, when the
    /// request's first pool task is about to start (work.rs `run_batches`); later asks do
    /// nothing.
    fn map_all(&mut self, cx: &Cx) -> Result<(), CliError>;
    /// Once the request has its answer: the store's facts into `profile` (the commit itself
    /// comes after the answer is printed, [`plain_place::after_answer`]).
    fn after_answer(&mut self, cx: &Cx, profile: &mut Profile) -> Result<(), CliError>;
    /// The `CrateScope` subjects of crates whose records hold nothing of a project (the
    /// toolchain's), told by the names step once its world is made: a trusted builder puts
    /// their records in the public namespace.
    fn public_crates(&mut self, subjects: &[ContentId]);
}

/// blaze's folder in the user's home, made by the first file made in it.
const HOME_FOLDER: &str = ".blaze";

/// The stand-in's folders under blaze's folder in the home: an old home when either is there.
pub(crate) const OLD_HOME: [&str; 2] = ["v2/shared", "v2/own"];
/// The stand-in's forms under a checkout: its packs folder and its `last` pointer file (the
/// last answer and leaf table are now a root of the one store, drive.md ruling 3).
pub(crate) const OLD_CHECKOUT: [&str; 2] = [".blaze/v2/packs", ".blaze/v2/at"];
/// The roots that say the old forms were looked for: the home's once, each checkout's once
/// (its name ends in the digest of the checkout's path, drive.md ruling 3).
const OLD_HOME_LOOKED: &str = "blaze.old-home-looked";
const OLD_CHECKOUT_LOOKED: &str = "blaze.old-packs-looked.";

/// Compaction slices run after the answer, each bounded by blaze-cas `SLICE_BYTES`.
pub(crate) const AFTER_ANSWER_SLICES: u32 = 1;

/// Why the command's store stopped it, by name (rule 19).
#[derive(Debug)]
pub enum StoreFailure {
    /// The environment names no home: the store is under it, and there is no other.
    NoHome,
    /// The home could not be opened, bstd saying why (and naming it).
    Home(bstd::fs::HomeError),
    /// The plain store refused.
    Place(PlaceError),
    /// A record could not be copied into a reader's frame.
    Copy(Stopped),
    /// Looking for an old form's folder failed.
    Look { folder: &'static str, error: FsError },
    /// A put of other bytes under a key the store holds (rule 21): the store's refusal, and
    /// the two records opened, so the refusal names the record and where they differ.
    TwoValues { key: Key, lent: blaze_cas::plain::LentError, clash: Clash },
}

/// A drive record's form, as [`record::open`](blaze_drive::record::open) reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Form {
    /// A whole record (at a task key with no reads, or at a value key).
    Whole,
    /// A shape: the read names of the records under one task key.
    Shape,
    /// Bytes that are no drive record (another writer's under a drive key).
    NotARecord,
}

impl Form {
    fn word(self) -> &'static str {
        match self {
            Form::Whole => "whole record",
            Form::Shape => "shape",
            Form::NotARecord => "bytes that are no drive record",
        }
    }
}

/// Two records under one drive key, opened (rule 19): the record's name, each side's form, the
/// first field they differ in, and the first names that differ when the field is a list of
/// reads (a shape's read names, a whole record's reads or parts), or the first differing byte
/// of the payload. `label` is the crate or file the name is of, filled by the command where it
/// knows the crate list ([`Clash::label_with`]).
#[derive(Debug)]
pub struct Clash {
    pub name: Option<Name>,
    pub held: Form,
    pub put: Form,
    /// The first field that differs: `form`, `name`, `source`, `out`, `split`, `reads`,
    /// `cells`, `parts`, `paths`, `findings` or `payload`; `none` when the records are equal
    /// (the store refused on its tag or namespace).
    pub field: &'static str,
    /// The first differing entry of a list field, held then put (`None` past a side's end).
    pub names: Option<(Option<Name>, Option<Name>)>,
    /// The first differing payload byte.
    pub payload_at: Option<usize>,
    /// When the read lists differ: the names only the held record read and only the put one
    /// did, so a read list that is not a function of the key's inputs shows which reads moved.
    pub apart: Option<Apart>,
    /// The file a whole record's own path entry names.
    pub path: Option<blaze_own::String<Global>>,
    pub label: Option<blaze_own::String<Global>>,
}

/// How many names of each side [`Apart`] keeps.
pub const APART_NAMES: usize = 16;

/// The read names only one side of a [`Clash`] holds: up to [`APART_NAMES`] a side, in name
/// order, and how many more.
#[derive(Debug)]
pub struct Apart {
    pub held: blaze_own::Vec<Name, Global>,
    pub put: blaze_own::Vec<Name, Global>,
    pub more_held: usize,
    pub more_put: usize,
}

impl Apart {
    /// `held` and `put`, each sorted by name (a record's reads are): one merge.
    fn of(held: impl Iterator<Item = Name>, put: impl Iterator<Item = Name>) -> Apart {
        let mut apart = Apart { held: blaze_own::Vec::new_in(Global), put: blaze_own::Vec::new_in(Global), more_held: 0, more_put: 0 };
        let keep = |list: &mut blaze_own::Vec<Name, Global>, more: &mut usize, name: Name| match list.len() < APART_NAMES {
            true => list.push(name),
            false => *more += 1,
        };
        let (mut held, mut put) = (held.peekable(), put.peekable());
        loop {
            match (held.peek().copied(), put.peek().copied()) {
                (None, None) => return apart,
                (Some(h), Some(p)) if h == p => {
                    held.next();
                    put.next();
                }
                (Some(h), Some(p)) if h < p => {
                    held.next();
                    keep(&mut apart.held, &mut apart.more_held, h);
                }
                (Some(h), None) => {
                    held.next();
                    keep(&mut apart.held, &mut apart.more_held, h);
                }
                (_, Some(p)) => {
                    put.next();
                    keep(&mut apart.put, &mut apart.more_put, p);
                }
            }
        }
    }
}

impl fmt::Display for Apart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let side = |f: &mut fmt::Formatter<'_>, what: &str, names: &[Name], more: usize| -> fmt::Result {
            write!(f, "; read only by the {what} record ({}):", names.len() + more)?;
            if names.is_empty() {
                f.write_str(" none")?;
            }
            for name in names.iter() {
                // A name prints as its kind and its subject's first bytes (key.rs `Display`).
                write!(f, " {name}")?;
            }
            if more > 0 {
                write!(f, " and {more} more")?;
            }
            Ok(())
        };
        side(f, "held", &self.held, self.more_held)?;
        side(f, "put", &self.put, self.more_put)
    }
}

/// The first place two lists differ: their entries there (`None` past a side's end).
fn first_apart<T: PartialEq>(mut a: impl Iterator<Item = T>, mut b: impl Iterator<Item = T>) -> Option<(Option<T>, Option<T>)> {
    loop {
        match (a.next(), b.next()) {
            (None, None) => return None,
            (x, y) if x == y => {}
            (x, y) => return Some((x, y)),
        }
    }
}

impl Clash {
    /// The records `held` (what the key holds; `None` when the store lends nothing there) and
    /// `put` (the refused bytes), opened and compared field by field.
    pub fn of(held: Option<&[u8]>, put: &[u8]) -> Clash {
        use blaze_drive::WireError;
        use blaze_drive::record::{Stored, open};
        fn form(stored: &Result<Stored<'_>, WireError>) -> Form {
            match stored {
                Ok(Stored::Whole(_)) => Form::Whole,
                Ok(Stored::Shape(_)) => Form::Shape,
                Err(_) => Form::NotARecord,
            }
        }
        fn name_of(stored: &Result<Stored<'_>, WireError>) -> Option<Name> {
            match stored {
                Ok(Stored::Whole(view)) => Some(view.name),
                Ok(Stored::Shape(view)) => Some(view.name),
                Err(_) => None,
            }
        }
        let held = held.map(open);
        let new = open(put);
        let mut clash = Clash {
            name: name_of(&new).or_else(|| held.as_ref().and_then(name_of)),
            held: held.as_ref().map_or(Form::NotARecord, form),
            put: form(&new),
            field: "form",
            names: None,
            payload_at: None,
            apart: None,
            path: None,
            label: None,
        };
        if let Ok(Stored::Whole(view)) = &new {
            if let Some(own) = view.paths().find(|path| path.name == view.name) {
                let mut path = blaze_own::String::new_in(Global);
                path.push_str(core::str::from_utf8(own.path).unwrap_or("a path not UTF-8"));
                clash.path = Some(path);
            }
        }
        match (held, new) {
            (Some(Ok(Stored::Whole(a))), Ok(Stored::Whole(b))) => {
                clash.field = if a.name != b.name {
                    "name"
                } else if a.source != b.source {
                    "source"
                } else if a.out != b.out {
                    "out"
                } else if a.split != b.split {
                    "split"
                } else if let Some((x, y)) = first_apart(a.reads(), b.reads()) {
                    clash.names = Some((x.map(|(name, _)| name), y.map(|(name, _)| name)));
                    clash.apart = Some(Apart::of(a.reads().map(|(name, _)| name), b.reads().map(|(name, _)| name)));
                    "reads"
                } else if let Some((x, y)) = first_apart(a.cells(), b.cells()) {
                    clash.names = Some((x.map(|read| read.name), y.map(|read| read.name)));
                    "cells"
                } else if let Some((x, y)) = first_apart(a.parts(), b.parts()) {
                    clash.names = Some((x.map(|part| part.name), y.map(|part| part.name)));
                    "parts"
                } else if let Some((x, y)) = first_apart(a.paths(), b.paths()) {
                    clash.names = Some((x.map(|path| path.name), y.map(|path| path.name)));
                    "paths"
                } else if first_apart(a.findings(), b.findings()).is_some() {
                    "findings"
                } else if let Some(at) = blaze_cas::plain::first_difference(a.payload, b.payload) {
                    clash.payload_at = Some(at);
                    "payload"
                } else {
                    "none"
                };
            }
            (Some(Ok(Stored::Shape(a))), Ok(Stored::Shape(b))) => {
                clash.field = if a.name != b.name {
                    "name"
                } else if let Some(apart) = first_apart(a.names(), b.names()) {
                    clash.names = Some(apart);
                    clash.apart = Some(Apart::of(a.names(), b.names()));
                    "reads"
                } else if let Some((x, y)) = first_apart(a.cells(), b.cells()) {
                    clash.names = Some((x.map(|(name, _)| name), y.map(|(name, _)| name)));
                    "cells"
                } else {
                    "none"
                };
            }
            _ => {}
        }
        clash
    }

    /// The crate the record's name is of, from the command's crate list: a `CrateScope`, its
    /// `Links`, or any task named by a crate's subject (work.rs `tally`'s labels).
    pub(crate) fn label_with(&mut self, names: Option<&crate::names::Names<'_>>) {
        let Some(name) = self.name else { return };
        let Some((krate, macro_side)) = names.and_then(|names| names.crate_at(name.subject).map(|at| names.crate_label(at))) else { return };
        let mut label = blaze_own::String::new_in(Global);
        label.push_str("crate ");
        label.push_str(krate);
        if macro_side {
            label.push_str(" (macro side)");
        }
        self.label = Some(label);
    }
}

impl fmt::Display for Clash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name {
            Some(name) => write!(f, "{name}")?,
            None => f.write_str("a record of no drive name")?,
        }
        if let Some(label) = &self.label {
            write!(f, " ({label})")?;
        }
        if let Some(path) = &self.path {
            write!(f, " ({path})")?;
        }
        write!(f, ": held {}, put {}", self.held.word(), self.put.word())?;
        match self.field {
            "none" => f.write_str(", equal fields")?,
            field => write!(f, ", first differing in {field}")?,
        }
        if let Some((held, put)) = self.names {
            let side = |f: &mut fmt::Formatter<'_>, name: Option<Name>| match name {
                Some(name) => write!(f, "{name}"),
                None => f.write_str("(none)"),
            };
            f.write_str(" (held ")?;
            side(f, held)?;
            f.write_str(", put ")?;
            side(f, put)?;
            f.write_str(")")?;
        }
        if let Some(at) = self.payload_at {
            write!(f, " at payload byte {at}")?;
        }
        if let Some(apart) = &self.apart {
            write!(f, "{apart}")?;
        }
        Ok(())
    }
}

impl fmt::Display for StoreFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreFailure::NoHome => f.write_str("HOME is not set: blaze's one store is under the home's .blaze/v2/store (rule: store.rs)"),
            StoreFailure::Home(error) => write!(f, "the home: {error}"),
            StoreFailure::Place(error) => write!(f, "{error}"),
            StoreFailure::Copy(stopped) => write!(f, "a record read from the store: {stopped}"),
            StoreFailure::Look { folder, error } => write!(f, "looking for the old store folder {folder}: {error}"),
            StoreFailure::TwoValues { key, lent, clash } => write!(f, "{clash}; the store under the home's .blaze/v2/store refused its put at {key}: {lent}"),
        }
    }
}

impl From<PlaceError> for CliError {
    fn from(error: PlaceError) -> CliError {
        CliError::Store(StoreFailure::Place(error))
    }
}

/// blaze's folder in the home, passed in from the command's start (rule 75) and reached
/// through the effect layer as a second tree ([`Fs::home`]), wherever it is.
pub struct Home {
    dir: Dir<ReadWrite>,
}

impl Home {
    /// From `HOME` as main.rs read it (absolute). No home is a typed refusal: the one store is
    /// under it.
    pub fn of(cx: &Cx, fs: &Fs, home: Option<&str>) -> Result<Home, CliError> {
        let Some(home) = home else { return Err(CliError::Store(StoreFailure::NoHome)) };
        match fs.home(cx, home, &Path::new(HOME_FOLDER)?) {
            Ok(dir) => Ok(Home { dir }),
            Err(error) => Err(CliError::Store(StoreFailure::Home(error))),
        }
    }

    pub fn dir(&self) -> &Dir<ReadWrite> {
        &self.dir
    }

    /// The command's store, opened lazily on the world's driver lent to [`plain_place::STORE_DIR`]
    /// (one driver a process, drive.md ruling 5): nothing is mapped until a get probes.
    pub fn open_store(&self, settings: &Settings, facts: Facts) -> Result<Lent<bstd::io::LentDriver, Global>, CliError> {
        let driver = self.dir.lend(&Path::new(plain_place::STORE_DIR)?);
        let dir = driver.dir().clone();
        Ok(plain_place::open(driver, plain_place::config(dir, settings, facts))?)
    }
}

/// The old forms' folders, each reported by name once (one line a folder), never read, migrated
/// or deleted. Looked for once in the home's life and once in each checkout's: the look is
/// recorded as a root, committed with this command's records after the answer.
pub fn old_home<D: Driver>(cx: &Cx, home: &Home, root: &Dir<ReadWrite>, checkout: &str, lent: &Lent<D, Global>, err: &Out) -> Result<u32, CliError> {
    let roots = lent.roots();
    let mut checkout_root = std::string::String::with_capacity(OLD_CHECKOUT_LOOKED.len() + 16);
    checkout_root.push_str(OLD_CHECKOUT_LOOKED);
    for byte in &ContentId::of("blaze-cli old packs look", checkout.as_bytes()).0[..8] {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        checkout_root.push(char::from(HEX[usize::from(byte >> 4)]));
        checkout_root.push(char::from(HEX[usize::from(byte & 15)]));
    }
    let looked = |name: &str| roots.iter().any(|(it, _)| it == name);
    let mut found = 0;
    let say = |cx: &Cx, place: &str, folder: &str| {
        err.line(cx, format_args!("blaze: an old store under {place}{folder} is not read by this blaze: its records are made again; delete it when no older blaze needs it"))
    };
    if !looked(OLD_HOME_LOOKED) {
        for folder in OLD_HOME {
            if home.dir.stat(cx, &Path::new(folder)?).map_err(|error| CliError::Store(StoreFailure::Look { folder, error }))?.is_some() {
                say(cx, "the home's .blaze/", folder)?;
                found += 1;
            }
        }
        lent.root(OLD_HOME_LOOKED, &[]).map_err(|error| CliError::Store(StoreFailure::Place(PlaceError::Lent(error))))?;
    }
    if !looked(&checkout_root) {
        for old in OLD_CHECKOUT {
            if root.stat(cx, &Path::new(old)?).map_err(|error| CliError::Store(StoreFailure::Look { folder: old, error }))?.is_some() {
                say(cx, "the checkout's ", old)?;
                found += 1;
            }
        }
        lent.root(&checkout_root, &[]).map_err(|error| CliError::Store(StoreFailure::Place(PlaceError::Lent(error))))?;
    }
    Ok(found)
}

/// Where records go (drive.md ruling 2): the one seam where the choice is made (rule 78). The
/// command's is [`ToolchainCrates`].
pub trait Sharing {
    /// The crates whose records hold nothing of a project, by their `CrateScope` subjects.
    fn add_public(&mut self, subjects: &[ContentId]);
    /// Whether `bytes`, a record about to be put, holds only public names: it goes to the
    /// public namespace when this machine is a trusted builder, else to `LOCAL`.
    fn all_names_public(&self, bytes: &[u8]) -> bool;
}

/// A drive record's names are all public when its name and every name it reads are on a
/// POSITIVE LIST BY SUBJECT (main's ruling, D115): `add_public` takes `CrateScope` subjects and
/// `Script` subjects; the cli adds a script's subject only when its crate is public (the
/// toolchain's, a checksummed registry archive's: target.md, "Namespaces") AND it is placed on
/// the wasm path, so a native script, and every record reading it, stays LOCAL (forgetting one
/// makes it local: the safe direction). Sharing never reads how a script ran. A `Fetch` record
/// stands for the home's sources folder (fetch.rs `SOURCES`): this machine's, never public
/// (D78). Anything not a drive record (a certificate, a root, a page) is not public.
pub struct ToolchainCrates {
    /// Sorted, without repeats.
    subjects: std::vec::Vec<ContentId, Global>,
}

impl ToolchainCrates {
    pub fn new() -> ToolchainCrates {
        ToolchainCrates { subjects: std::vec::Vec::new_in(Global) }
    }
}

impl Sharing for ToolchainCrates {
    fn add_public(&mut self, subjects: &[ContentId]) {
        self.subjects.extend_from_slice(subjects);
        self.subjects.sort_unstable();
        self.subjects.dedup();
    }

    fn all_names_public(&self, bytes: &[u8]) -> bool {
        let public = |name: Name| matches!(name.kind, Kind::CrateScope | Kind::Script) && self.subjects.binary_search(&name.subject).is_ok();
        let (name, all_public) = match blaze_drive::record::open(bytes) {
            // A cell read names its record as a whole read does, so both must be public.
            Ok(Stored::Whole(view)) => (
                view.name,
                public(view.name) && view.reads().all(|(name, _)| public(name)) && view.cells().all(|read| public(read.name)),
            ),
            Ok(Stored::Shape(view)) => (view.name, public(view.name) && view.names().all(public) && view.cells().all(|(name, _)| public(name))),
            // Not a drive record (a certificate, a root, a page): this machine's.
            Err(_) => return false,
        };
        match (name.kind, all_public) {
            // Fetched and adopted sources are this machine's (D78): never the shared namespace.
            (Kind::Fetch, _) => false,
            (_, all_public) => all_public,
        }
    }
}

/// The command's store: the plain store's handle, where records go, and a named origin.
pub struct CommandStore<'m, 'r, D: Driver, S: Sharing = ToolchainCrates> {
    place: PlainPlace<'m, D>,
    sharing: S,
    remote: Option<Remote<'r>>,
    mapped_all: bool,
    /// The open's time, for the profile and `--stats`.
    open_ns: u64,
}

impl<'m, 'r, D: Driver> CommandStore<'m, 'r, D, ToolchainCrates> {
    /// The handle over `lent`, opened in `open_ns`, then `origin` when one is named. Past
    /// [`plain_place::SEGMENTS_NAMED_MOST`] segments named, one line says compaction is behind.
    #[expect(clippy::too_many_arguments, reason = "the store's edge: each argument one handle or fact of it")]
    pub fn new(cx: &Cx, lent: &'m Lent<D, Global>, public_writer: bool, open_ns: u64, origin: Option<&'r NamedOrigin>, clock: Clock, err: &'r Out, alloc: Alloc<'r>) -> Result<CommandStore<'m, 'r, D>, CliError> {
        let remote = origin.map(|named| Remote {
            named,
            reader: Reader::new_in(Namespace::PUBLIC, &named.check, Limits::DEFAULT, alloc),
            clock,
            err,
            alloc,
            spent_ns: 0,
            said: 0,
        });
        let (_, named) = lent.mapped_of_named();
        if named > plain_place::SEGMENTS_NAMED_MOST {
            err.line(cx, format_args!("blaze: the store under the home's .blaze/{} names {named} segments, past {}: its compaction is behind", plain_place::STORE_DIR, plain_place::SEGMENTS_NAMED_MOST))?;
        }
        Ok(CommandStore { place: PlainPlace::new(lent, public_writer), sharing: ToolchainCrates::new(), remote, mapped_all: false, open_ns })
    }
}

/// The command's store: the plain store on the world's lent driver.
pub type CommandLent = Lent<bstd::io::LentDriver, Global>;

/// The one store as the command's function holds it, from before the request to after the
/// answer is printed.
pub struct Opened {
    pub home: Home,
    pub lent: CommandLent,
    /// The open's nanoseconds (the profile's `store open` line).
    pub open_ns: u64,
}

/// The one store a user, opened as a command opens it (drive.md, "The store under the
/// command"): the home from `HOME` (none: the typed refusal), the boot fact (none: one line,
/// and every segment of an earlier command is checked once), the nonce from the granted `Rng`;
/// the open maps nothing. Then the old forms are reported once ([`old_home`]).
pub fn open_for_command(cx: &Cx, io: &bstd::io::Io, home: Option<&str>, checkout_root: &Dir<ReadWrite>, checkout: &str, clock: &Clock, err: &Out) -> Result<Opened, CliError> {
    let home = Home::of(cx, &io.need_fs()?, home)?;
    let boot = match io.boot() {
        Ok(boot) => {
            let mut first = [0u8; 8];
            first.copy_from_slice(&boot.0[..8]);
            Some(u64::from_le_bytes(first))
        }
        Err(why) => {
            err.line(cx, format_args!("blaze: no boot fact ({why}): each segment an earlier command wrote is checked once"))?;
            None
        }
    };
    let nonce = nonce(&mut io.need_rng()?);
    let ns = |clock: &Clock| u64::try_from(clock.now().since_start().as_nanos()).unwrap_or(u64::MAX);
    let began = ns(clock);
    let lent = home.open_store(&Settings::default(), Facts { boot, nonce })?;
    let open_ns = ns(clock).saturating_sub(began);
    old_home(cx, &home, checkout_root, checkout, &lent, err)?;
    Ok(Opened { home, lent, open_ns })
}

/// After the answer is printed, once every handle over `lent` is gone: the ONE commit of this
/// command's records and roots ([`plain_place::after_answer`]), and with `stats` its line on
/// `err`: the commit's time, and the time from the answer's last byte to the store's last
/// effect (the process exits right after).
pub fn after_answer_printed(cx: &Cx, lent: CommandLent, stats: bool, clock: &Clock, err: &Out) -> Result<(), CliError> {
    let ns = |clock: &Clock| u64::try_from(clock.now().since_start().as_nanos()).unwrap_or(u64::MAX);
    let answered = ns(clock);
    let after = plain_place::after_answer(&lent, AFTER_ANSWER_SLICES)?;
    let commit_ns = ns(clock).saturating_sub(answered);
    drop(lent);
    if stats {
        err.line(
            cx,
            format_args!(
                "blaze: store after the answer: commit {} segments, sealed {}, slices {}, {:.3} ms; answer's last byte to exit {:.3} ms",
                after.segments_written,
                after.sealed,
                after.compaction_slices,
                commit_ns as f64 / 1e6,
                ns(clock).saturating_sub(answered) as f64 / 1e6,
            ),
        )?;
    }
    Ok(())
}

/// A planted bug of the nonce's test (`store/nonce_tests.rs`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NoncePlant {
    None,
    /// Every open takes one constant: two processes would name a segment alike.
    Constant,
}

/// The store's nonce ([`Facts::nonce`]): the next draw of the granted generator (bstd `Rng`,
/// seeded once from the system's secure source on a real driver, from the world's generator in
/// the simulation; main's ruling 21:3x: no second randomness grant). Taken where the store is
/// opened, from a generator handed in (rule 75).
pub fn nonce(rng: &mut bstd::rng::Rng) -> u64 {
    draw_nonce(rng, NoncePlant::None)
}

pub(crate) fn draw_nonce(rng: &mut bstd::rng::Rng, plant: NoncePlant) -> u64 {
    match plant {
        NoncePlant::None => rng.next_u64(),
        NoncePlant::Constant => 0x6e6f_6e63_6521,
    }
}

/// The root a checkout's last answer for one request shape hangs from (drive.md ruling 3):
/// `last.<16 hex of H(checkout path)>.<the pointer's 16 hex of H(tool, shape)>`. The one place a
/// digest of a machine's path stands: a root is `LOCAL`, never sealed or published.
pub fn last_root(checkout: &str, pointer: &blaze_drive::pointers::PointerName) -> std::string::String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut name = std::string::String::with_capacity(5 + 16 + 1 + 16);
    name.push_str("last.");
    for byte in &ContentId::of("blaze-cli last root checkout", checkout.as_bytes()).0[..8] {
        name.push(char::from(HEX[usize::from(byte >> 4)]));
        name.push(char::from(HEX[usize::from(byte & 15)]));
    }
    name.push('.');
    for &byte in pointer.bytes() {
        name.push(char::from(byte));
    }
    name
}

/// The root naming the last answer written for one units' source and request shape, by any
/// checkout on this machine: `seed.<16 hex of the units' source>.<the pointer's 16 hex>`. A
/// checkout with no last answer of its own (a second worktree's first command) follows it
/// (main.rs `request`): its reads re-hashed here key the certificate, so equal text is a hit,
/// other text a miss that walks forward from that answer. No machine's path: a checkout's
/// layout reads relative paths, so two worktrees of one tree name one seed. `LOCAL`, like
/// [`last_root`].
pub fn seed_root(units: &ContentId, pointer: &blaze_drive::pointers::PointerName) -> std::string::String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut name = std::string::String::with_capacity(5 + 16 + 1 + 16);
    name.push_str("seed.");
    for byte in &units.0[..8] {
        name.push(char::from(HEX[usize::from(byte >> 4)]));
        name.push(char::from(HEX[usize::from(byte & 15)]));
    }
    name.push('.');
    for &byte in pointer.bytes() {
        name.push(char::from(byte));
    }
    name
}

/// The checkout's last answer and leaf table for one request shape, in place: the one record the
/// root `root` ([`last_root`]) names (main.rs seals it with `seal::seal`). Read before the
/// request's handle is made and never asked of an origin: a root is `LOCAL`. `None`: no root
/// yet, or a root naming no single record (the request answers in full).
pub fn last<'m, D: Driver>(lent: &'m Lent<D, Global>, root: &str) -> Result<Option<Held<'m>>, CliError> {
    let place = PlainPlace::new(lent, false);
    let Some(named) = place.root(root)? else { return Ok(None) };
    let Ok(key) = <[u8; 32]>::try_from(named.bytes) else { return Ok(None) };
    Ok(place.get(Key(ContentId(key)))?)
}

impl<'m, D: Driver, S: Sharing> CommandStore<'m, '_, D, S> {
    /// `sealed` as the checkout's last answer for one request shape: put under its content key
    /// and named by each root of `roots` (the checkout's [`last_root`], and the [`seed_root`]
    /// of its units' source). All ride the ONE commit after the answer is printed
    /// ([`plain_place::after_answer`]): no generation names a pointer whose record is absent
    /// (blaze-cas plain/root_commit_tests.rs).
    pub fn set_last(&mut self, cx: &Cx, roots: &[&str], sealed: &[u8]) -> Result<(), CliError> {
        let key = Key(ContentId::of("blaze-cli last answer record", sealed));
        self.put(cx, key, sealed)?;
        for root in roots {
            self.place.pin(root, &[key])?;
        }
        Ok(())
    }
}

impl<'m, D: Driver, S: Sharing> Store<'m> for CommandStore<'m, '_, D, S> {
    fn public_crates(&mut self, subjects: &[ContentId]) {
        self.sharing.add_public(subjects);
    }

    fn get(&mut self, cx: &Cx, key: Key) -> Result<Option<Held<'m>>, CliError> {
        if let Some(held) = self.place.get(key)? {
            return Ok(Some(held));
        }
        let Some(remote) = &mut self.remote else { return Ok(None) };
        let began = remote.now_ns();
        let clock = remote.clock.clone();
        let mut now = || u64::try_from(clock.now().since_start().as_nanos()).unwrap_or(u64::MAX);
        // A batch of one key: the request asks one record at a time (blaze-drive's `Get`).
        // `BatchError` is only a batch past `keys_most` or one begun mid-batch: neither can be.
        let fetched = match &remote.named.at {
            OriginAt::Folder(folder) => fetch(&mut remote.reader, &mut FolderOrigin { cx, folder }, &mut now, core::slice::from_ref(&key.0), remote.alloc),
            OriginAt::Http(at) => fetch(&mut remote.reader, &mut HttpOrigin::new(cx, at, &clock), &mut now, core::slice::from_ref(&key.0), remote.alloc),
        };
        fetched.map_err(|error| CliError::Remote(RemoteError::Batch(error)))?;
        let found = match remote.reader.answer(0) {
            Answer::Found { .. } => {
                let bytes = remote.reader.found(0).unwrap_or(&[]);
                // Put before it is answered, and answered from the arena: lent for `'m`.
                self.place.put(key, false, bytes)?;
                self.place.get(key)?
            }
            Answer::Absent | Answer::Pending => None,
            Answer::Unfetched(why) => {
                remote.say(cx, key, &why)?;
                None
            }
        };
        if let Some(remote) = &mut self.remote {
            remote.spent_ns += remote.now_ns().saturating_sub(began);
        }
        Ok(found)
    }

    fn put(&mut self, _cx: &Cx, key: Key, bytes: &[u8]) -> Result<(), CliError> {
        let public = self.sharing.all_names_public(bytes);
        match self.place.put(key, public, bytes) {
            Ok(()) => Ok(()),
            Err(PlaceError::Lent(lent @ blaze_cas::plain::LentError::TwoValues { .. })) => {
                let held = self.place.get(key)?;
                let clash = Clash::of(held.map(|held| held.bytes), bytes);
                Err(CliError::Store(StoreFailure::TwoValues { key, lent, clash }))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn map_all(&mut self, _cx: &Cx) -> Result<(), CliError> {
        if !self.mapped_all {
            self.place.map_all()?;
            self.mapped_all = true;
        }
        Ok(())
    }

    fn after_answer(&mut self, cx: &Cx, profile: &mut Profile) -> Result<(), CliError> {
        // Facts of the run, not news: the profile's rows (and `--stats`), never plain stderr.
        let counts = self.place.counts();
        profile.store_open_ns = self.open_ns;
        profile.store_named = counts.named;
        profile.store_mapped = counts.mapped;
        profile.store_probe_gets = counts.probes.gets;
        profile.store_probes = counts.probes.probes;
        profile.store_probes_most = counts.probes.most;
        if let Some(remote) = &self.remote {
            let r = remote.reader.report();
            remote.err.line(
                cx,
                format_args!(
                    "blaze: origin: {} keys asked, {} found, {} absent, {} not fetched; {} range reads (head {}, manifest {}, summary {}, index {}, payload {}), {} late; {} bytes fetched for {} used; {:.3} ms",
                    r.keys,
                    r.found,
                    r.absent,
                    r.unfetched,
                    r.asks,
                    r.head_asks,
                    r.manifest_asks,
                    r.summary_asks,
                    r.page_asks,
                    r.payload_asks,
                    r.late,
                    r.bytes_fetched,
                    r.bytes_used,
                    remote.spent_ns as f64 / 1e6,
                ),
            )?;
        }
        Ok(())
    }
}

/// An origin named on the command line (remote-read.md): `--origin <folder> --origin-key <hex>`
/// after the checkout. Nothing ambient (rule 75): with no `--origin`, nothing is read but the
/// local store. The folder is beneath the start directory; the key is the one Ed25519 public
/// key whose signed head is believed.
pub struct NamedOrigin {
    at: OriginAt,
    check: OneKey,
}

/// Where a named origin's objects are: a folder, or an `http://` base address.
enum OriginAt {
    Folder(Dir<ReadWrite>),
    Http(HttpAt),
}

/// An `http://host:port/prefix` origin: the base it was given, and the network narrowed to
/// that one address (nothing else is reached; nothing ambient).
pub struct HttpAt {
    base: Url,
    net: Net,
}

/// The one trusted key of a named origin, owned, as the read path's head check.
struct OneKey {
    keys: [PublicKey; 1],
}

impl HeadCheck for OneKey {
    fn check(&self, namespace: Namespace, signer: &PublicKey, domain: &'static str, message: &[u8], signature: &[u8; 64]) -> Result<(), SignatureWhy> {
        Ed25519Keys { namespace: Namespace::PUBLIC, keys: &self.keys }.check(namespace, signer, domain, message, signature)
    }
}

impl NamedOrigin {
    /// The origin the arguments name (main.rs reads them): `--origin` and `--origin-key`
    /// together or neither.
    pub fn of(cx: &Cx, fs: &Fs, net: Option<&Net>, folder: Option<&str>, key: Option<&str>) -> Result<Option<NamedOrigin>, CliError> {
        match (folder, key) {
            (None, None) => Ok(None),
            (Some(folder), Some(key)) => {
                let key = PublicKey::from_hex(key).ok_or(CliError::Usage("--origin-key is not 64 hex digits (an Ed25519 public key)"))?;
                let at = if folder.starts_with("https://") {
                    return Err(CliError::Usage("--origin https://: bstd's Http has no TLS yet (D63, stage two); name an http:// origin or a folder"));
                } else if folder.starts_with("http://") {
                    let net = net.ok_or(CliError::Usage("--origin http://: this host offers no network"))?;
                    OriginAt::Http(HttpAt::of(folder, net)?)
                } else {
                    OriginAt::Folder(fs.open_dir(cx, &Path::new(folder)?)?)
                };
                Ok(Some(NamedOrigin { at, check: OneKey { keys: [key] } }))
            }
            _ => Err(CliError::Usage("--origin and --origin-key are named together")),
        }
    }
}

/// Why a read from a named origin stopped the command (not a miss: a miss computes locally).
#[derive(Debug)]
pub enum RemoteError {
    /// The read path refused the batch: past `keys_most`, or begun mid-batch.
    Batch(BatchError),
    /// A fetched record could not be copied into the request's buffer.
    Copy(Stopped),
}

impl core::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RemoteError::Batch(error) => write!(f, "the origin's read path: {error}"),
            RemoteError::Copy(stopped) => write!(f, "a record fetched from the origin: {stopped}"),
        }
    }
}

/// A folder as an origin: each ask one ranged read through bstd's files, bounded by the ask's
/// deadline. An object that is not there is `Missing`; any other failure `Unreadable`.
struct FolderOrigin<'o> {
    cx: &'o Cx,
    folder: &'o Dir<ReadWrite>,
}

impl Origin for FolderOrigin<'_> {
    fn read(&mut self, object: &Object, at: u64, into: &mut [u8], deadline_ns: u64) -> Result<usize, OriginError> {
        let failed = |error: FsError| if error.kind == FsKind::NotFound { OriginError::Missing } else { OriginError::Unreadable };
        // The name is built from a digest's hex only (`Object::name`): nothing fetched is a path.
        let name = object.name();
        let path = Path::new(name.as_str()).map_err(|_| OriginError::Unreadable)?;
        let cx = self.cx.until(Deadline::at(Duration::from_nanos(deadline_ns)));
        let file = self.folder.open_read(&cx, &path).map_err(failed)?;
        file.read_into(into, at).wait(&cx).map_err(failed)
    }
}

impl HttpAt {
    /// The origin at `base` (`http://host:port[/prefix]`), the network narrowed to its address.
    fn of(base: &str, net: &Net) -> Result<HttpAt, CliError> {
        let base = Url::parse(base.trim_end_matches('/')).map_err(|_| CliError::Usage("--origin http://: not a URL"))?;
        let origin = base.origin().ok_or(CliError::Usage("--origin http://: the URL names no host"))?;
        // clone: the origin's address, a refcount.
        let net = net.connect_only(&Addr::Tcp(origin.at().clone()));
        Ok(HttpAt { base, net })
    }
}

/// How the shared cache's origin is asked over HTTP: a reply's body is held to the ask's range
/// by bstd's `Http` itself (and to a record's most here); a redirect is refused; a host that
/// says nothing for ten seconds is `Silent`, and the ask's deadline bounds the whole.
const ORIGIN_WIRE: Wire = Wire {
    user_agent: "blaze",
    body_most: 64 << 20,
    // A 64 MiB body in 16 KiB receives, and its head.
    reads_most: (64 << 20) / (16 << 10) + 64,
    wait_most: Duration::from_secs(10),
    redirects: NO_REDIRECTS,
};

/// An `http://` origin: each ask one ranged GET of `<base>/<object name>` through bstd's
/// `Http` over its `Net` (D63; remote-read.md, "HTTP in the tree"). bstd holds the reply to the
/// ask's length before a byte past it is taken, takes only a `206` of exactly the range asked
/// (blaze-h1's `Response::ranged`), refuses a redirect (`NO_REDIRECTS`) and bounds every wait;
/// its simulation names each refusal (blaze-std tests/requests_servers.rs). A 404 is `Missing`,
/// a 416 (asked past the object's end) no bytes, anything else `Unreadable`. Nothing here
/// trusts the bytes: the read machine checks the signed head and the digest chain.
struct HttpOrigin<'o> {
    cx: &'o Cx,
    at: &'o HttpAt,
    /// bstd's client, reaching the origin's address alone.
    http: Http,
    /// The URL's text, reused from ask to ask.
    url: blaze_own::Vec<u8, Global>,
}

impl<'o> HttpOrigin<'o> {
    fn new(cx: &'o Cx, at: &'o HttpAt, clock: &Clock) -> HttpOrigin<'o> {
        // No TLS yet (`https://` is refused where the origin is named), so no roots are trusted.
        let http = Http::over(&at.net, clock, &Roots::none(), ORIGIN_WIRE);
        let http = match at.base.origin() {
            Some(origin) => http.only(origin),
            None => http,
        };
        HttpOrigin { cx, at, http, url: blaze_own::Vec::new_in(Global) }
    }
}

impl Origin for HttpOrigin<'_> {
    fn read(&mut self, object: &Object, at: u64, into: &mut [u8], deadline_ns: u64) -> Result<usize, OriginError> {
        self.read_with(object, at, into, deadline_ns, |_| {})
    }
}

impl HttpOrigin<'_> {
    /// [`Origin::read`], with `between` run after the request is sent and before its answer is
    /// read: nothing in the command; the test's server, in the simulation (bstd's begin/finish
    /// split, which lets one lane play both ends).
    fn read_with(&mut self, object: &Object, at: u64, into: &mut [u8], deadline_ns: u64, between: impl FnOnce(&Cx)) -> Result<usize, OriginError> {
        if into.is_empty() {
            return Ok(0);
        }
        // The name is built from a digest's hex only (`Object::name`): nothing fetched is a path.
        let name = object.name();
        self.url.clear();
        let joined = [self.at.base.as_str().as_bytes(), b"/", name.as_str().as_bytes()];
        for part in joined {
            self.url.try_extend_from_slice(part).map_err(|_| OriginError::Unreadable)?;
        }
        let text = core::str::from_utf8(&self.url).map_err(|_| OriginError::Unreadable)?;
        let url = Url::parse(text).map_err(|_| OriginError::Unreadable)?;
        let range = Range { first: at, last: Some(at + into.len() as u64 - 1) };
        let request = Request { range: Some(range), ..Request::get(&url) };
        let cx = self.cx.until(Deadline::at(Duration::from_nanos(deadline_ns)));
        let mut body = cx.bytes();
        let answered = match self.http.begin(&cx, &request) {
            Ok(exchange) => {
                between(&cx);
                match exchange.finish(&cx, &mut body) {
                    Ok(Finished::Answered(response)) => Ok(response),
                    // `NO_REDIRECTS` makes a redirect an error; a hop is never begun.
                    Ok(Finished::Redirected(next)) => {
                        let _ = next.finish(&cx, &mut body);
                        return Err(OriginError::Unreadable);
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        };
        match answered {
            Ok(_) => {
                // bstd held the body to the range, `into.len()` bytes at most.
                let got = body.len().min(into.len());
                into[..got].copy_from_slice(&body.as_slice()[..got]);
                Ok(got)
            }
            Err(error) => match error.kind {
                HttpKind::Protocol(blaze_h1::Error { why: blaze_h1::Why::RangeStatus { status: 404 }, .. }) => {
                    Err(OriginError::Missing)
                }
                HttpKind::Protocol(blaze_h1::Error { why: blaze_h1::Why::RangeStatus { status: 416 }, .. }) => Ok(0),
                _ => Err(OriginError::Unreadable),
            },
        }
    }
}

struct Remote<'r> {
    named: &'r NamedOrigin,
    reader: Reader<'r>,
    clock: Clock,
    err: &'r Out,
    alloc: Alloc<'r>,
    /// Time spent reading the origin and checking what came back.
    spent_ns: u64,
    /// One bit for each kind of "not fetched" already said.
    said: u8,
}

impl Remote<'_> {
    fn now_ns(&self) -> u64 {
        u64::try_from(self.clock.now().since_start().as_nanos()).unwrap_or(u64::MAX)
    }

    /// One "not fetched" line for each kind, the first time it happens in this command.
    fn say(&mut self, cx: &Cx, key: Key, why: &Unfetched) -> Result<(), CliError> {
        let kind = match why {
            Unfetched::Origin { .. } => 0,
            Unfetched::Short { .. } => 1,
            Unfetched::Late { .. } => 2,
            Unfetched::Refused(_) => 3,
            Unfetched::Limit { .. } => 4,
        };
        if self.said & (1 << kind) != 0 {
            return Ok(());
        }
        self.said |= 1 << kind;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut short = [0u8; 12];
        for (at, byte) in key.0.0[..6].iter().enumerate() {
            short[at * 2] = HEX[usize::from(byte >> 4)];
            short[at * 2 + 1] = HEX[usize::from(byte & 15)];
        }
        let short = core::str::from_utf8(&short).map_err(|_| CliError::Text { what: "a key's hex" })?;
        self.err.line(cx, format_args!("blaze: origin: record {short}… not fetched, computed here: {why}"))?;
        Ok(())
    }
}

/// The http origin's proof: the folder's records byte-equal over HTTP, a liar refused.
#[cfg(test)]
#[path = "store_http_tests.rs"]
mod http_tests;

/// Two worktrees on one home (remote-read.md, "Shared across worktrees (2026-11-16)").
#[cfg(test)]
#[path = "store_worktree_tests.rs"]
mod worktree_tests;

/// The old home's report and the no-home refusal, on bstd's simulated world.
#[cfg(test)]
#[path = "store/old_home_tests.rs"]
mod old_home_tests;

/// The nonce: equal under one simulation seed, different under two, and different for two opens
/// of one process (control: a constant).
#[cfg(test)]
#[path = "store/nonce_tests.rs"]
mod nonce_tests;

/// A natively run build script's record and its reader stay `LOCAL` (D115; the scripts lane's).
#[cfg(test)]
#[path = "store/sharing_tests.rs"]
mod sharing_tests;
