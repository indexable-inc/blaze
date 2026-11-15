//! The daemon's writer of request slots and its placer of start files
//! (/tmp/plan/10-no-daemon-run.md, task B; the slot's layout and its reader are
//! `blaze_certify::slot`, the client's check `blaze_certify::solo`).
//!
//! - [`Writing`]: after a hit or a written certificate, the slot of that request: the column's
//!   stamps the watcher saved, each name it saved unknown read and hashed between two equal
//!   steady looks, each read outside the views valued between two equal steady looks, then the
//!   client's own check rehearsed over the daemon's store ([`blaze_certify::solo::rehearse`]):
//!   a slot is written only where a client with nothing moved would hit on it, and its records
//!   are exactly the gets that check made. Written whole through a rename, never synced: a
//!   slot torn by a crash fails its check word, and its reader asks the daemon.
//! - [`Placing`]: a launcher's start file (`slot::start_file`), written beside the program
//!   once its image is durable: `.part` written, synced, renamed over, its directory synced.
//!   So a start file is whole or absent after any crash.
//! - [`keep`]: the slots kept, at most [`MOST_SLOTS`], the oldest removed first.
//!
//! A sans-IO core (rule 14): each look and write is a [`Step`] or a [`Place`] the loop's
//! driver carries out (certify.rs; serve/daemon/launch.rs for a start file).

#![blaze::sans_io]

use std::fmt;

use blaze_certify::certificate::{ABSOLUTE, LAUNCHER, Read};
use blaze_certify::slot::{self as layout, Column, Local, Outside, Placed, Premise, Slot};
use blaze_core::Stamp;
use blaze_ids::ContentId;
use blaze_record::Digest;

/// The most slots kept under `<state>/slots`: past it the oldest written goes.
pub(super) const MOST_SLOTS: usize = 64;

/// Why a request's slot is not written: each names its thing. The request is answered all the
/// same; its client asks the daemon.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Unslotted {
    /// The request has no cargo command line (`slot::key_of`).
    NoKey,
    /// It asks for the build's trace.
    Traced,
    /// The column vouches for the root `column` (or none), not the root `read` the
    /// certificate was valued at: a file saved under the build.
    Moved { read: ContentId, column: Option<ContentId> },
    /// The certificate reads a node of the view below its root: only the root is a client's.
    Node(String),
    /// It reads a whole tree outside the views, which no stamp vouches for.
    Walks(String),
    /// This name or path was not steady, or its two looks differ: no stamp vouches for it.
    Unsteady(String),
    /// This name's bytes, or this read's digest, are not what the certificate was valued at.
    Changed(String),
    /// A path dependency is mounted, and no column of it is held.
    Local(String),
    /// The engine's own executable could not be looked at: why.
    NoTool(String),
    /// The standard library's `DIGEST` file could not be found or looked at.
    NoLibrary,
    /// The client's check would not hit on the slot: why.
    Rehearsal(String),
    /// The slot could not be written: why.
    Write(String),
}

impl fmt::Display for Unslotted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unslotted::NoKey => f.write_str("the request has no cargo command line, which a slot is keyed by"),
            Unslotted::Traced => f.write_str("the request asks for the build's trace, which a build makes"),
            Unslotted::Moved { read, column } => match column {
                Some(column) => write!(f, "the checkout's column vouches for the root {column:x}, not {read:x} the certificate was valued at"),
                None => write!(f, "the checkout has no column that vouches for the root {read:x}"),
            },
            Unslotted::Node(path) => write!(f, "the certificate reads the view's {path}, below its root"),
            Unslotted::Walks(path) => write!(f, "the certificate reads the whole tree at {path}, which no stamp vouches for"),
            Unslotted::Unsteady(name) => write!(f, "{name} is not steady: no stamp vouches for it"),
            Unslotted::Changed(name) => write!(f, "{name} is not what the certificate was valued at"),
            Unslotted::Local(at) => write!(f, "the path dependency at {at} has no column a client can look at"),
            Unslotted::NoTool(why) => write!(f, "the engine's executable could not be looked at: {why}"),
            Unslotted::NoLibrary => f.write_str("the standard library's DIGEST file could not be looked at"),
            Unslotted::Rehearsal(why) => write!(f, "a client would not hit on the slot: {why}"),
            Unslotted::Write(why) => write!(f, "the slot could not be written: {why}"),
        }
    }
}

/// What a request's slot is of: the request as asked, and the certificate it was answered by.
pub(super) struct Of<'a> {
    /// `build` or `check`.
    pub word: &'a str,
    pub targets: &'a blaze_api::Targets,
    /// The request's root directory (`cargo.root`), what the key is of.
    pub root_dir: &'a str,
    /// Where the slots are, as the driver names files: `<state>/slots`.
    pub slots: &'a str,
    pub base: ContentId,
    pub traced: bool,
    pub drawn: ContentId,
    pub env: &'a [(String, String)],
    pub unasked: Vec<String>,
    /// The certificate's reads, each at the digest it was valued at (its answer's launcher
    /// programs are [`Writing::programs`] of its outputs).
    pub reads: &'a [(Read, Digest)],
    /// The request's frame: the CBOR of `(word, targets)`, environment and all.
    pub request: Vec<u8>,
}

/// A look or a write the driver carries out for a [`Writing`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Step {
    /// The `lstat` of each absolute path, in order: [`Seen::Stamps`].
    Stamps(Vec<String>),
    /// The whole file at this absolute path: [`Seen::Read`].
    Read(String),
    /// The digest now of a read outside the views (certify.rs `outside_now`): [`Seen::Valued`].
    Value(Read),
    /// `bytes` replace the file `to` whole, through a rename, waiting for no sync
    /// (`Effect::WriteUnsynced`): [`Seen::Written`].
    Write { to: String, bytes: Vec<u8> },
    Done(Result<Written, Unslotted>),
}

/// What the driver saw of the last [`Step`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Seen {
    /// The stamps, one a path, and when the look was asked for by the file system's clock.
    Stamps { stamps: Vec<Stamp>, asked_ns: u64 },
    Read(Result<Vec<u8>, String>),
    Valued(Digest),
    Written(Result<(), String>),
}

/// A slot the writer ended with: its key, the hash of its bytes but `written_ns` (`body`), and
/// whether it was written (not where the slot kept under its key is that one already).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Written {
    pub key: ContentId,
    pub body: ContentId,
    pub wrote: bool,
}

/// A bug planted in the writer, for a test's control. Never planted outside a test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::serve) enum Plant {
    None,
    /// The stamps are taken once, after the reads: a file edited between its read and the
    /// look keeps its new stamp over its old bytes.
    SlotBeforeLook,
    /// The slot takes the root the certificate was valued at, not compared with the root the
    /// column's stamps vouch for.
    ColumnOfAnotherRoot,
}

#[cfg(test)]
thread_local! {
    pub(in crate::serve) static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

#[cfg(test)]
fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

#[cfg(not(test))]
fn planted(plant: Plant) -> bool {
    let _ = plant;
    false
}

/// Where the writer is.
enum Phase {
    /// Nothing looked at.
    Opened,
    /// The first look is handed out.
    First,
    /// The `at`th name to read (of `reads`) is handed out.
    Reading { at: usize },
    /// The `at`th outside read is handed out.
    Valuing { at: usize },
    /// The second look is handed out.
    Again,
    /// The slot's bytes are handed out.
    Writing { body: ContentId },
    Done,
}

/// The writer of one request's slot.
pub(super) struct Writing {
    slot: Slot,
    env: Vec<(String, String)>,
    slots: String,
    /// The body of the slot kept under the key now, where one is ([`Listed::body`]).
    kept: Option<ContentId>,
    /// Each column name saved unknown: its place in `slot.column.names` and the content the
    /// certificate's root holds there.
    reads: Vec<(usize, ContentId)>,
    /// The first look's stamps, in the look's order.
    first: Vec<Stamp>,
    first_ns: u64,
    phase: Phase,
    /// The answer's launcher programs, kept with the slot once written ([`Listed::programs`]).
    programs: Vec<String>,
}

/// The path of a column's `name` under `dir` (`.` is `dir`), absolute.
fn under(dir: &str, name: &str) -> String {
    match name {
        "." => dir.to_owned(), // alloc: a path to look at, once a name a slot
        _ => format!("{}/{name}", dir.trim_end_matches('/')), // alloc: as above
    }
}

/// A head's column as the watcher saved it, copied for one slot ([`Saved::of`]): the column's
/// one home stays `blaze_vfs::Disk`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Saved {
    /// The watched directory, absolute.
    pub dir: String,
    /// The root the column vouches for ([`blaze_vfs::Column::root`]).
    pub root: Option<ContentId>,
    /// Each name relative to `dir` (`.` is `dir`), in name order, with its saved stamp.
    pub names: Vec<(String, Stamp)>,
}

impl Saved {
    /// The copy of `column`, its names made relative to its directory.
    pub(super) fn of(column: &blaze_vfs::Column<'_>) -> Saved {
        let dir = format!("/{}", column.dir.trim_start_matches('/')); // alloc: the slot's own, once a slot
        let mut names = Vec::with_capacity(column.len()); // alloc: as above, a name a name
        let prefix_len = column.dir.len();
        column.each(|name, stamp| {
            let rel = match name.get(prefix_len..).unwrap_or_default() {
                "" => ".".to_owned(), // alloc: as above
                rest => rest.trim_start_matches('/').to_owned(), // alloc: as above
            };
            names.push((rel, stamp));
        });
        Saved { dir, root: column.root, names }
    }
}

impl Writing {
    /// The writer of `of`'s slot, over the checkout's column `column` (whose root is the
    /// head's now), `content` the id the column's root holds for a name (each name saved
    /// unknown is hashed against it; `None`: no file there), the path dependencies' columns
    /// `locals`, the premises `tool` (the engine's executable) and `library` (its `DIGEST`),
    /// each file output's mark `placed`, and `kept`, the body of the slot under the key now.
    /// `now_ns`: the file system's clock now, after every look the column holds. Refused, by
    /// name, where no client could hit.
    #[expect(clippy::too_many_arguments, reason = "the parts of one slot, each of another type")]
    pub(super) fn new(
        of: Of<'_>,
        column: Saved,
        content: impl Fn(&str) -> Option<ContentId>,
        locals: Vec<Local>,
        tool: Premise,
        library: Premise,
        placed: Vec<Placed>,
        kept: Option<ContentId>,
        now_ns: u64,
    ) -> Result<Writing, Unslotted> {
        let key = layout::key_of(of.word, of.targets, of.root_dir).ok_or(Unslotted::NoKey)?;
        if of.traced {
            return Err(Unslotted::Traced);
        }
        let mut root = None;
        let mut outside = Vec::new(); // alloc: the slot's own, once a slot
        for (read, digest) in of.reads {
            match read {
                Read::Node(path) if path.is_empty() => root = Some(*digest),
                Read::Node(path) => return Err(Unslotted::Node(path.clone())), // clone: a reason's name
                Read::Tree(path) | Read::Package(path) => return Err(Unslotted::Walks(path.clone())), // clone: as above
                Read::Outside(_) | Read::Listing(_) | Read::Entry(_) => outside.push(Outside {
                    read: read.clone(), // clone: the slot's own read
                    digest: *digest,
                    stamp: Stamp::UNKNOWN,
                    looked_ns: 0,
                }),
                Read::Env(_) | Read::Drawn => {}
            }
        }
        let Saved { dir, root: saved_root, names } = column;
        let root = match (root, saved_root) {
            (Some(Digest::Is(read)), Some(column)) if read == column => column,
            // The planted bug: the certificate's root, whatever root the stamps vouch for.
            (Some(Digest::Is(read)), _) if planted(Plant::ColumnOfAnotherRoot) => read,
            (Some(Digest::Is(read)), column) => return Err(Unslotted::Moved { read, column }),
            (Some(Digest::Nothing) | None, column) => {
                return Err(Unslotted::Moved { read: ContentId([0; 32]), column });
            }
        };
        let mut reads = Vec::new(); // alloc: none unless a name was saved unknown
        for (at, (name, stamp)) in names.iter().enumerate() {
            if !stamp.is_unknown() {
                continue;
            }
            match content(if name == "." { "" } else { name }) {
                Some(id) => reads.push((at, id)),
                // A directory saved unknown: its listing is no file a look can hash.
                None => return Err(Unslotted::Unsteady(under(&dir, name))),
            }
        }
        let slot = Slot {
            key,
            written_ns: now_ns,
            base: of.base,
            traced: false,
            drawn: of.drawn,
            names: layout::names_of(of.env),
            unasked: of.unasked,
            premises: vec![tool, library], // alloc: the slot's own
            column: Column { dir, root, looked_ns: now_ns, names },
            locals,
            outside,
            placed,
            records: Vec::new(), // alloc: none until rehearsed
            request: of.request,
        };
        Ok(Writing {
            slot,
            env: of.env.to_vec(), // alloc: the request's environment, once a slot
            slots: of.slots.to_owned(), // alloc: as above
            kept,
            reads,
            first: Vec::new(), // alloc: none until looked
            first_ns: now_ns,
            programs: Vec::new(), // alloc: none until set
            phase: Phase::Opened,
        })
    }

    /// Every path the looks take, in order: each name saved unknown, then each outside path.
    fn looked(&self) -> Vec<String> {
        let column = &self.slot.column;
        let mut paths: Vec<String> = self.reads.iter().map(|(at, _)| under(&column.dir, &column.names[*at].0)).collect(); // alloc: once a slot
        paths.extend(self.slot.outside.iter().map(|it| it.read.parts().1.to_owned())); // alloc: as above
        paths
    }

    fn done(&mut self, outcome: Result<Written, Unslotted>) -> Step {
        self.phase = Phase::Done;
        Step::Done(outcome)
    }

    /// The name the `n`th look's path is of, for a reason.
    fn named(&self, n: usize) -> String {
        self.looked().swap_remove(n)
    }

    /// The next thing to do: `seen` is what the driver saw of the last [`Step`], `None` at the
    /// first. `store` is the daemon's, which the client's check is rehearsed over.
    pub(super) fn step<S: blaze_record::Store>(&mut self, store: &S, seen: Option<Seen>) -> Step
    where
        S::Error: fmt::Display,
    {
        match (std::mem::replace(&mut self.phase, Phase::Done), seen) {
            (Phase::Opened, None) if planted(Plant::SlotBeforeLook) => self.read(0),
            (Phase::Opened, None) => match self.looked() {
                paths if paths.is_empty() => self.rehearse(store),
                paths => {
                    self.phase = Phase::First;
                    Step::Stamps(paths)
                }
            },
            (Phase::First, Some(Seen::Stamps { stamps, asked_ns })) => {
                if stamps.len() != self.reads.len() + self.slot.outside.len() {
                    return self.done(Err(Unslotted::Unsteady(format!("{} stamps for the looks", stamps.len())))); // alloc: a reason
                }
                if let Some(n) = stamps.iter().position(|it| it.is_unknown() || blaze_vfs::racy(it, asked_ns)) {
                    let name = self.named(n);
                    return self.done(Err(Unslotted::Unsteady(name)));
                }
                (self.first, self.first_ns) = (stamps, asked_ns);
                self.read(0)
            }
            (Phase::Reading { at }, Some(Seen::Read(bytes))) => {
                let (n, wanted) = self.reads[at];
                let name = &self.slot.column.names[n].0;
                match bytes {
                    Ok(bytes) if ContentId::of(blaze_vfs::FILE_DOMAIN, &bytes) == wanted => self.read(at + 1),
                    Ok(_) => {
                        let path = under(&self.slot.column.dir, name);
                        self.done(Err(Unslotted::Changed(path)))
                    }
                    Err(why) => {
                        let path = under(&self.slot.column.dir, name);
                        self.done(Err(Unslotted::Changed(format!("{path} ({why})")))) // alloc: a reason
                    }
                }
            }
            (Phase::Valuing { at }, Some(Seen::Valued(digest))) => {
                if digest != self.slot.outside[at].digest {
                    let path = self.slot.outside[at].read.parts().1.to_owned(); // alloc: a reason's name
                    return self.done(Err(Unslotted::Changed(path)));
                }
                self.value(at + 1)
            }
            (Phase::Again, Some(Seen::Stamps { stamps, asked_ns })) => {
                let planted_once = planted(Plant::SlotBeforeLook);
                if planted_once {
                    (self.first, self.first_ns) = (stamps.clone(), asked_ns); // clone: the planted look's own
                }
                if stamps.len() != self.first.len() {
                    return self.done(Err(Unslotted::Unsteady(format!("{} stamps for the looks", stamps.len())))); // alloc: a reason
                }
                let moved = stamps.iter().zip(&self.first).position(|(now, then)| {
                    now != then || now.is_unknown() || (!planted_once && blaze_vfs::racy(now, self.first_ns))
                });
                if let Some(n) = moved {
                    let name = self.named(n);
                    return self.done(Err(Unslotted::Unsteady(name)));
                }
                // Each restamped name and outside path vouches as seen at the first look,
                // steady then; the column's other stamps were steady at the watcher's looks,
                // all before `now_ns`: one `looked_ns` for the column, the later of the two.
                let column = &mut self.slot.column;
                for ((n, _), stamp) in self.reads.iter().zip(&stamps) {
                    column.names[*n].1 = *stamp;
                }
                column.looked_ns = column.looked_ns.max(self.first_ns);
                let reads = self.reads.len();
                for (outside, stamp) in self.slot.outside.iter_mut().zip(&stamps[reads..]) {
                    (outside.stamp, outside.looked_ns) = (*stamp, self.first_ns);
                }
                self.rehearse(store)
            }
            (Phase::Writing { body }, Some(Seen::Written(written))) => match written {
                Ok(()) => self.done(Ok(Written { key: self.slot.key, body, wrote: true })),
                Err(why) => self.done(Err(Unslotted::Write(why))),
            },
            (Phase::Done, _) => self.done(Err(Unslotted::Write("the writer was stepped past its end".to_owned()))), // alloc: a reason
            (_, _) => self.done(Err(Unslotted::Write("the writer was told another thing than it looked for".to_owned()))), // alloc: a reason
        }
    }

    /// The `at`th name saved unknown read, or on to the outside reads.
    fn read(&mut self, at: usize) -> Step {
        match self.reads.get(at) {
            Some((n, _)) => {
                self.phase = Phase::Reading { at };
                Step::Read(under(&self.slot.column.dir, &self.slot.column.names[*n].0))
            }
            None => self.value(0),
        }
    }

    /// The `at`th outside read valued, or on to the second look.
    fn value(&mut self, at: usize) -> Step {
        match self.slot.outside.get(at) {
            Some(outside) => {
                self.phase = Phase::Valuing { at };
                Step::Value(outside.read.clone()) // clone: the driver's own read
            }
            None => {
                self.phase = Phase::Again;
                Step::Stamps(self.looked())
            }
        }
    }

    /// The client's check rehearsed over `store`: its gets are the slot's records.
    fn rehearse<S: blaze_record::Store>(&mut self, store: &S) -> Step
    where
        S::Error: fmt::Display,
    {
        match blaze_certify::solo::rehearse(&self.slot, store, &self.env) {
            Ok(gets) => self.slot.records = gets,
            Err(why) => return self.done(Err(Unslotted::Rehearsal(why.to_string()))),
        }
        let body = body_of(&self.slot);
        if self.kept == Some(body) {
            return self.done(Ok(Written { key: self.slot.key, body, wrote: false }));
        }
        self.phase = Phase::Writing { body };
        // alloc: the slot's path, once a write
        Step::Write { to: format!("{}/{:x}", self.slots.trim_end_matches('/'), self.slot.key), bytes: self.slot.encode() }
    }

    /// The written slot's launcher programs: each `LAUNCHER` output of its answer.
    pub(super) fn programs(outputs: &[blaze_certify::certificate::Output]) -> Vec<String> {
        // alloc: once a slot, a path a program
        outputs.iter().filter(|it| it.base == LAUNCHER).map(|it| it.path.clone()).collect()
    }

    /// The answer's launcher programs, kept with the slot ([`Writing::programs`]).
    pub(super) fn set_programs(&mut self, programs: Vec<String>) {
        self.programs = programs;
    }

    /// The programs [`Writing::set_programs`] gave.
    pub(super) fn programs_held(&self) -> &[String] {
        &self.programs
    }

    /// The written slot's request frame.
    pub(super) fn request(&self) -> &[u8] {
        &self.slot.request
    }

    /// When the slot was written by the file system's clock.
    pub(super) fn written_ns(&self) -> u64 {
        self.slot.written_ns
    }

    /// How many names the column has, reads outside, and records the slot holds: its span's
    /// fields.
    pub(super) fn counts(&self) -> (usize, usize, usize) {
        (self.slot.column.names.len(), self.slot.outside.len(), self.slot.records.len())
    }
}

/// The hash of `slot`'s bytes with `written_ns` left out: two slots that would say the same to
/// a client are one body, whenever written.
fn body_of(slot: &Slot) -> ContentId {
    let mut bytes = slot.encode();
    // `written_ns` is the 8 bytes after the magic, the check word and the key.
    bytes[72..80].fill(0);
    ContentId::of("blaze v2 request slot kept", &bytes[40..])
}

/// A file output's mark as a slot holds it: `ABSOLUTE` and its absolute path.
pub(super) fn placed(path: &str, stamp: Stamp, looked_ns: u64, content: ContentId) -> Option<Placed> {
    // alloc: the slot's own path
    layout::absolute(path).then(|| Placed { base: ABSOLUTE, path: path.to_owned(), stamp, looked_ns, content })
}

/// A slot kept under `<state>/slots`: what the bound and a launcher's request ([`request_of`])
/// read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Listed {
    pub key: ContentId,
    pub written_ns: u64,
    /// The hash of its bytes but `written_ns` ([`Written::body`]); `None` for one listed and
    /// not written by this daemon.
    pub body: Option<ContentId>,
    /// The launcher programs its answer has, and its request frame: what this daemon wrote;
    /// none for one listed.
    pub programs: Vec<String>,
    pub request: Vec<u8>,
}

/// `slot` kept in `listed` (in place of one under its key), and the key of each slot past
/// [`MOST_SLOTS`] to remove, oldest `written_ns` first.
pub(super) fn keep(listed: &mut Vec<Listed>, slot: Listed) -> Vec<ContentId> {
    listed.retain(|it| it.key != slot.key);
    listed.push(slot);
    let mut gone = Vec::new(); // alloc: none unless past the bound
    while listed.len() > MOST_SLOTS {
        let Some(oldest) = listed.iter().enumerate().min_by_key(|(_, it)| (it.written_ns, it.key)).map(|(at, _)| at) else { break };
        gone.push(listed.swap_remove(oldest).key);
    }
    gone
}

/// The key a slot's file name is of: its 64 hex digits.
pub(super) fn key_named(name: &str) -> Option<ContentId> {
    let name = name.rsplit('/').next()?;
    if name.len() != 64 {
        return None;
    }
    let mut key = [0; 32];
    for (at, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(name.get(2 * at..2 * at + 2)?, 16).ok()?;
    }
    Some(ContentId(key))
}

/// The request frame of the newest slot this daemon wrote whose answer has a `LAUNCHER`
/// output at `program` (H7): a launcher that started from its file asks the daemon for a
/// build of that request.
pub(crate) fn request_of<'a>(listed: &'a [Listed], program: &str) -> Option<&'a [u8]> {
    listed
        .iter()
        .filter(|it| it.programs.iter().any(|it| it == program))
        .max_by_key(|it| it.written_ns)
        .map(|it| it.request.as_slice())
}

/// One step of a start file's place.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Place {
    /// `bytes` written to the file `to` (made, or cut and written whole), not synced.
    Write { to: String, bytes: Vec<u8> },
    /// The file synced: its bytes durable.
    Sync(String),
    /// `from` renamed over `to`.
    Rename { from: String, to: String },
    /// The directory's names synced: the rename durable.
    SyncDir(String),
    Done,
}

/// A bug planted in the placer, for a test's control: the rename before the sync.
#[cfg(test)]
thread_local! {
    pub(super) static RENAMES_BEFORE_THE_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn renames_before_the_sync() -> bool {
    RENAMES_BEFORE_THE_SYNC.get()
}

#[cfg(not(test))]
fn renames_before_the_sync() -> bool {
    false
}

/// The place of a program's start file: whole or absent after any crash.
pub(crate) struct Placing {
    file: String,
    part: String,
    dir: String,
    image: Option<Vec<u8>>,
    at: u8,
}

impl Placing {
    /// The start file of the program at `program` (absolute, as the driver names files),
    /// holding `image`.
    pub(crate) fn new(program: &str, image: Vec<u8>) -> Placing {
        let file = layout::start_file(program);
        let part = format!("{file}.part"); // alloc: a path, once a place
        let dir = match file.rsplit_once('/') {
            Some(("", _)) | None => "/".to_owned(), // alloc: as above
            Some((dir, _)) => dir.to_owned(),       // alloc: as above
        };
        Placing { file, part, dir, image: Some(image), at: 0 }
    }

    /// The next step: each asked once the one before it completed.
    pub(crate) fn step(&mut self) -> Place {
        let planted = renames_before_the_sync();
        self.at += 1;
        // alloc: each step's own paths, once a place
        match (self.at, planted) {
            (1, _) => Place::Write { to: self.part.clone(), bytes: self.image.take().unwrap_or_default() },
            (2, false) => Place::Sync(self.part.clone()),
            (3, true) => Place::Sync(self.file.clone()),
            (3, false) | (2, true) => Place::Rename { from: self.part.clone(), to: self.file.clone() },
            (4, _) => Place::SyncDir(self.dir.clone()),
            _ => Place::Done,
        }
    }
}

#[cfg(test)]
mod tests;
