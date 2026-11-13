//! The request machine's driver (docs/v2/plan/design/certificate.md): the thin IO side of
//! [`request::Request`], on the daemon's loop.
//!
//! A build or a check is checked against its certificate before it is a job
//! ([`Jobs::certify_begin`]): the machine's asks are answered here (a node from the request's
//! pinned view, a variable from the request's environment, a file or a listing outside the
//! views through [`blaze_usercode::inputs::Disk`], an output's stamp and bytes through the
//! loop's driver: [`Effect::Stamp`], [`Effect::ReadAll`]). A hit says what the build said again
//! and finishes with no job; a miss goes on into the job ([`Jobs::begin_job`]). Every build
//! of a checkout is heard as it speaks ([`Building`], [`Jobs::certify_heard`]); one that
//! answered, having compiled or not, hands the machine what it read and what a build with
//! nothing to do says ([`said::Saying`]) at its end ([`Jobs::certify_written`]) and the
//! record is written.
//!
//! What this driver does not do yet, each named in certificate.md, "State": the shape is not
//! relocated (the base holds the checkout's directory, an output's path is absolute, base 0);
//! the one node is the view's root.

use std::cell::RefCell;

use blaze_record::Digest;

use blaze_core::Stamp;

use super::certificate::{self, ABSOLUTE, Evidence, Read, env_digest};
use super::request::{self, Ask, Next, Told, View};
use super::said;
use super::*;
use blaze_vfs::outside::{Look, LookKind, Looks};

use crate::serve::local;
use super::certificate::CodeHolder;
use crate::serve::work::{HeldCode, Looked, Seen};

/// The most (checkout, shape) pairs evidence is held of; past it the oldest goes. Evidence is
/// never a condition: without it a check hashes each output once.
const MOST_EVIDENCE: usize = 64;

/// The most states one call steps a machine through before it stops, named: a check has
/// five (find, value, stamp, hash, stamp again), a write two.
const MOST_ROUNDS: u32 = 16;


/// The daemon's store as `blaze_record` reads and writes it: every list and answer a
/// `Kind::Query` value under its own key. The tickets of what it put are its owner's to
/// settle.
struct Records<'a> {
    // Only the loop's thread holds it, for one step: a cell, no lock.
    store: RefCell<&'a mut Store>,
    tickets: RefCell<Vec<blaze_cas::Ticket>>,
    tally: &'a blaze_record::Tally,
}

impl<'a> Records<'a> {
    fn new(store: &'a mut Store, tally: &'a blaze_record::Tally) -> Records<'a> {
        // alloc: none until a record is put
        Records { store: RefCell::new(store), tickets: RefCell::new(Vec::new()), tally }
    }
}

impl blaze_record::Store for Records<'_> {
    type Held = Vec<u8>;
    type Error = String;

    fn get(&self, _: blaze_record::Kind, key: &ContentId) -> Result<Option<Vec<u8>>, String> {
        match self.store.borrow().get(&blaze_cas::Key(*key)) {
            // alloc: a record's bytes, once a list or answer read
            Ok(entry) => Ok(entry.map(|it| it.bytes.to_vec())),
            // alloc: an error, once
            Err(corrupt) => Err(format!("the store's value under {key:x} is corrupt: {corrupt:?}")),
        }
    }

    fn put(&self, _: blaze_record::Kind, key: ContentId, bytes: &[u8]) -> Result<(), String> {
        match self.store.borrow_mut().put(blaze_cas::Key(key), blaze_cas::Kind::Query, bytes) {
            Ok(ticket) => {
                self.tickets.borrow_mut().push(ticket);
                Ok(())
            }
            // alloc: an error, once
            Err(mismatch) => Err(format!("the store holds other bytes under {key:x}: {mismatch:?}")),
        }
    }

    fn bytes(held: &Vec<u8>) -> &[u8] {
        held
    }

    fn tally(&self) -> &blaze_record::Tally {
        self.tally
    }
}

/// A request whose check is under way: no job yet; or (`late`) the job itself, which
/// waited for this daemon's look at the standard library and has opened no view.
struct Checking {
    id: JobId,
    /// The request's view and work, to become its job at a miss; none of a late check,
    /// whose job holds them.
    view: Option<Opened>,
    work: Option<Work>,
    idle: bool,
    late: bool,
    machine: request::Request,
    root: u32,
    shape: ContentId,
    base: ContentId,
    /// The request's pinned view's root.
    now: ContentId,
    env: Vec<(String, String)>,
    drawn: ContentId,
    /// The asks of the machine's present state, as it gave them: an effect's tag names one
    /// by its place here.
    asks: Vec<Ask>,
    /// How many of them are looks in flight at the driver.
    open: u32,
    /// The file system's clock as they were asked for: no look of them is earlier, so a stamp
    /// steady by it is steady.
    asked_ns: u64,
    /// Each read the machine was told, with its digest: what a slot of the hit holds
    /// ([`slot::Writing`]).
    told: Vec<(Read, Digest)>,
    /// The whole check, to its hit or its miss.
    span: tracing::Span,
}

/// A build of a checkout under way, heard as it speaks: at its end, what its request's
/// certificate is written of ([`Ended`]). One a job, from its view's last open to its
/// `Finish`.
pub(super) struct Building {
    pub(super) job: JobId,
    root: u32,
    shape: ContentId,
    /// The view it reads (a root id of content, its own lock-file writes in it), its config
    /// and its request.
    inputs: Inputs,
    saying: said::Saying,
    /// Every look at the disk outside every view its tasks made: the list its job hands each
    /// task ([`crate::serve::work::JobInput::looks`]), its own and no other build's.
    looks: blaze_vfs::outside::Looks,
    /// The variables its steps asked for by name ([`Building::steps_read`]); or why they are
    /// not known.
    named: Result<Vec<String>, String>,
    /// The files and the listings its steps read by path outside the views; or why they are
    /// not known.
    read: Result<(Vec<std::path::PathBuf>, Vec<std::path::PathBuf>), String>,
    /// Every read of the machine's files its traced steps' records rest on, each with what
    /// was there when the record held (`Writer::read_seen`); or the step that has no trace.
    seen: Result<Vec<Seen>, String>,
    /// Its request's word and targets, as asked: what its slot is keyed by.
    asked: Option<(&'static str, blaze_api::Targets)>,
}

impl Building {
    /// What its steps read, as its writer says once every task of it ended.
    pub(super) fn steps_read(
        &mut self,
        named: Result<Vec<String>, String>,
        read: Result<(Vec<std::path::PathBuf>, Vec<std::path::PathBuf>), String>,
        seen: Result<Vec<Seen>, String>,
    ) {
        (self.named, self.read, self.seen) = (named, read, seen);
    }

    /// It ended, `passed` or not.
    fn end(self, passed: bool) -> Ended {
        let Building { root, shape, inputs, saying, looks, named, read, seen, asked, .. } = self;
        if !passed {
            // alloc: a reason, once a build
            return Ended { root, shape, built: Err(request::Unwritten::Failed.to_string()) };
        }
        // Each look it made at the disk that no watch vouched for is a read of its own, at
        // what that look saw: a file's content, an entry, a listing.
        let looks = looks.all();
        let looked = match looked_twice(&looks) {
            None => Ok(looks),
            Some(why) => Err(why),
        };
        let built = match saying.end() {
            Ok(said) => Ok(Walked { inputs, said, named, read, seen, looked, asked }),
            // alloc: a reason, once a build
            Err(what) => Err(format!("no request certificate is written: the build said {what}, which no build says with nothing to do")),
        };
        Ended { root, shape, built }
    }
}

/// A build that ended, as its [`Building`] heard it: kept until the loop has the store in
/// hand ([`Jobs::certify_written`]).
pub(super) struct Ended {
    root: u32,
    shape: ContentId,
    /// What it hands the machine, or why it leaves no certificate.
    built: Result<Walked, String>,
}

/// What a build that answered read and what a build of its request says with nothing to do.
struct Walked {
    inputs: Inputs,
    /// [`said::Saying::end`].
    said: Vec<Response>,
    named: Result<Vec<String>, String>,
    read: Result<(Vec<std::path::PathBuf>, Vec<std::path::PathBuf>), String>,
    seen: Result<Vec<Seen>, String>,
    /// What it looked at on the disk itself, each look with what it saw; or the paths it
    /// saw at two values.
    looked: Result<Vec<Look>, String>,
    asked: Option<(&'static str, blaze_api::Targets)>,
}

/// Why a build whose looks are `looks` ([`Looks::all`]: sorted, so the looks of one path and
/// kind are side by side) leaves no certificate, when one path was seen at two values: each
/// such path, what it was asked, and the two callers that saw it differ. A build that read
/// something that moved under it can't be said again by what it read.
pub(super) fn looked_twice(looks: &[Look]) -> Option<String> {
    let mut twice = Looks::twice(looks).peekable();
    twice.peek()?;
    // alloc: a reason, once a build that saw a path move under it
    let named: Vec<String> = twice
        .map(|(one, two)| format!("{} {} (seen by {} and by {})", one.kind, one.path.display(), one.by, two.by))
        .collect();
    // alloc: as above
    Some(format!(
        "no request certificate is written: the build saw {} at two values; it moved under the build",
        named.join(", ")
    ))
}

/// What the jobs hold for the request machines.
pub(super) struct Certify {
    /// The blaze that serves: the hash of its own executable ([`tool_of`]), once it was asked
    /// for.
    tool: Option<[u8; 32]>,
    /// The searches of the certificates' read-lists, counted.
    tally: blaze_record::Tally,
    checking: Vec<Checking>,
    /// One a checkout and request shape, oldest first, at most [`MOST_EVIDENCE`].
    evidence: Vec<(u32, ContentId, Evidence)>,
    /// One a job that builds a checkout now.
    pub(super) building: Vec<Building>,
    pub(super) ended: Vec<Ended>,
    /// The jobs begun unchecked because this daemon had not looked at the standard library
    /// yet: each is checked as that look is in ([`Jobs::certify_late`]).
    late: Vec<JobId>,
    /// Whether this daemon has marked its library ([`mark_library`]): once a daemon's life,
    /// as its library is.
    library_marked: bool,
    /// The request slots being written, each by its number ([`Own::Slot`]), with the look it
    /// waits for at the driver.
    slots: Vec<(u32, slot::Writing, Waits)>,
    /// The next slot's number.
    next_slot: u32,
    /// The slots kept under `<state>/slots`: `None` until the first write lists the
    /// directory; at most [`slot::MOST_SLOTS`].
    pub(in crate::serve) listed: Option<Vec<slot::Listed>>,
}

/// What a slot's writer waits for at the driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Waits {
    /// A batch of stamps asked for at this time by the file system's clock.
    Stamps { asked_ns: u64 },
    Read,
    Write,
}

/// The tag number of the listing of `<state>/slots` ([`Own::Slot`]).
const LISTING: u32 = u32::MAX;
/// The tag number of a slot's removal past the bound.
const REMOVAL: u32 = u32::MAX - 1;

impl Certify {
    pub(super) fn new() -> Certify {
        Certify {
            tool: None,
            tally: blaze_record::Tally::default(),
            checking: Vec::new(), // alloc: none until a request is checked
            evidence: Vec::new(), // alloc: none until a request is checked
            building: Vec::new(), // alloc: none until a checkout is built
            ended: Vec::new(),    // alloc: none until a build ends
            late: Vec::new(),     // alloc: none unless a request comes before the library is looked at
            library_marked: false,
            slots: Vec::new(), // alloc: none until a slot is written
            next_slot: 0,
            listed: None,
        }
    }

    /// Whether a slot is being written: what the daemon's exit waits for (H8).
    pub(in crate::serve) fn slots_writing(&self) -> bool {
        !self.slots.is_empty()
    }
}

/// The word and targets of `work`, where it is a request a slot is of: a build or a check.
fn slot_asked(work: &Work) -> Option<(&'static str, blaze_api::Targets)> {
    match work {
        // clone: the targets its slot is keyed by, once a build
        Work::Build(targets) => Some(("build", targets.clone())),
        Work::Check(targets) => Some(("check", targets.clone())), // clone: as above
        _ => None,
    }
}

/// Where the evidence of checkout `root` and shape `shape` is in `held`; where none is held,
/// made from the journal's marks of placed outputs ([`Evidence::seeded`]), so the first check
/// after a start hashes no output that is as it was placed or last hashed. A mark this blaze
/// does not read (another layout, cut short) vouches for nothing: it is said, named, and
/// removed, and its output is hashed once, as with no mark; evidence is never a condition.
/// Past [`MOST_EVIDENCE`] the oldest goes, and with it the mark of each output no evidence
/// held keeps ([`unmark_unheld`]).
fn evidence_at(held: &mut Vec<(u32, ContentId, Evidence)>, root: u32, shape: ContentId, vfs: &mut Vfs) -> usize {
    if let Some(at) = held.iter().position(|it| it.0 == root && it.1 == shape) {
        return at;
    }
    if held.len() >= MOST_EVIDENCE {
        let (_, _, gone) = held.remove(0);
        unmark_unheld(&gone, held, vfs);
    }
    let (seeded, undecoded) = {
        let _seeding = tracing::info_span!(target: "blaze", "request certificate: the outputs' marks").entered();
        let mut placed = Vec::new(); // alloc: once a (checkout, shape) first checked, an entry a mark
        let mut undecoded = Vec::new(); // alloc: none unless a mark is of another layout
        for (name, bytes) in vfs.marks(local::PLACED) {
            let Some(path) = name.strip_prefix(local::PLACED) else { continue };
            match local::Placed::decode(name, bytes) {
                // alloc: an output's path, once a mark
                Ok(local::Placed { stamp, looked_ns, content }) => placed.push((path.to_owned(), stamp, looked_ns, content)),
                Err(why) => undecoded.push(why),
            }
        }
        let seeded = Evidence::seeded(
            ABSOLUTE,
            placed.iter().map(|(path, stamp, looked_ns, content)| (path.as_str(), *stamp, *looked_ns, *content)),
        );
        (seeded, undecoded)
    };
    for why in undecoded {
        let (local::Undecoded::Layout { name, .. } | local::Undecoded::Length { name, .. }) = &why;
        tracing::warn!(target: "blaze", "request certificate: {why}: it vouches for nothing and is removed; its output is hashed once");
        vfs.unmark(name);
    }
    held.push((root, shape, seeded));
    held.len() - 1
}

/// The mark's name of the output at `path` (base [`ABSOLUTE`]).
fn placed_mark(path: &str) -> String {
    format!("{}{path}", local::PLACED) // alloc: a mark's name, once an output changed
}

/// Removes the mark of each output `gone` (an evidence no longer held) kept that no evidence
/// of `held` keeps.
fn unmark_unheld(gone: &Evidence, held: &[(u32, ContentId, Evidence)], vfs: &mut Vfs) {
    for (base, path) in gone.entries() {
        if base == ABSOLUTE && !held.iter().any(|it| it.2.holds(base, path)) {
            drop(vfs.unmark(&placed_mark(path)));
        }
    }
}

#[cfg(test)]
thread_local! {
    /// The planted bug of the marks' bound's control: a mark is never removed for an output
    /// no evidence keeps any more, as before the bound.
    static KEEPS_MARKS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Each entry of `held[at]`'s evidence changed by the machine's last step written to the
/// journal's marks (`local::PLACED` and the output's path: its stamp, when it was looked at,
/// its content), or its mark removed where nothing is kept of it now by any evidence held
/// (forgotten, kept racy, or trimmed off the answer's outputs: [`Evidence::only`]). So the
/// marks are bounded by what the evidence held keeps: a path no output is placed at any more
/// loses its mark at the first answer without it. The tickets are not waited for: a mark lost
/// to a crash leaves its output hashed once by the next daemon, never trusted wrongly (a mark
/// is only what the evidence already vouched for, under the stamp compared whole).
fn mark_changed(held: &mut [(u32, ContentId, Evidence)], at: usize, vfs: &mut Vfs) {
    let (before, rest) = held.split_at_mut(at);
    let Some(((_, _, evidence), after)) = rest.split_first_mut() else { return };
    let others = |base: u8, path: &str| before.iter().chain(after.iter()).any(|it| it.2.holds(base, path));
    evidence.drain_changed(|base, path, kept| {
        if base != ABSOLUTE {
            return;
        }
        let name = placed_mark(path);
        match kept {
            Some((stamp, looked_ns, content)) => {
                let value = local::Placed { stamp: *stamp, looked_ns, content: *content }.encode();
                if let Err(too) = vfs.mark(&name, &value) {
                    tracing::error!(target: "blaze", "request certificate: {too}");
                }
            }
            #[cfg(test)]
            None if KEEPS_MARKS.get() => {}
            None if others(base, path) => {}
            None => drop(vfs.unmark(&name)),
        }
    });
}

/// The blaze that serves, by content: the BLAKE3 hash of the executable running now
/// ([`blaze_profile::identity::binary_hash`]: what `builds.blaze` names each build's blaze by).
/// Never its version: a blaze built again under the same version is another program, and what
/// the old one answered is not its answer (rule 21). The daemon's start knows it before any
/// request, by the executable's stamp ([`local::tool_hash`], the one home of it, which seeds
/// the process's hash), so this reads nothing; `held` keeps it for the loop. A blaze that
/// cannot read itself certifies nothing and says so.
fn tool_of(held: &mut Option<[u8; 32]>) -> Result<[u8; 32], String> {
    if let Some(tool) = held {
        return Ok(*tool);
    }
    let tool = {
        let _hashing = tracing::info_span!(target: "blaze", "request certificate: the hash of this blaze's executable").entered();
        blaze_profile::identity::binary_hash().map_err(|error| {
            // alloc: an error, once
            format!("the request certificate is of the blaze that serves it, by the hash of its executable, which could not be read: {error}")
        })?
    };
    Ok(*held.insert(tool))
}

/// The base of the certificates of a request of plan shape `plan`, config `config` (less its
/// environment) and request `request`, served by the blaze `tool` ([`tool_of`]).
fn base_of(plan: ContentId, config: ContentId, request: ContentId, tool: &[u8; 32]) -> ContentId {
    certificate::base(&certificate::Shape { plan, config, request }, tool)
}

/// The variables set in `env` that a build reads with no step asking for them by name.
fn unasked_of(env: &[(String, String)]) -> Vec<String> {
    // clone: a name, once a set variable read unasked, once a request
    env.iter().filter(|(name, _)| read_unasked(name)).map(|(name, _)| name.clone()).collect()
}

/// The digest of a look that found `found` at `path` outside the views. A path that could
/// not be read has a digest of its own that no content has: a check against it misses and
/// the build says what is wrong with the path; a write never records it ([`unreadable`]).
fn outside_digest(found: Result<Option<[u8; 32]>, ()>, path: &str) -> Digest {
    match found {
        Ok(Some(hash)) => Digest::Is(ContentId(hash)),
        Ok(None) => Digest::Nothing,
        Err(()) => Digest::Is(unreadable(path)),
    }
}

fn unreadable(path: &str) -> ContentId {
    Hasher::new("blaze v2 serve request certificate unreadable").part(path.as_bytes()).finish()
}

/// The digest now of `read`, a file or a listing outside the views: one look at the disk,
/// in a span.
pub(super) fn outside_now(read: &Read) -> Digest {
    use blaze_usercode::trace::Inputs as _;
    let (path, how) = match read {
        Read::Outside(path) => (path, Looked::File),
        Read::Entry(path) => (path, Looked::Entry),
        Read::Listing(path) => (path, Looked::Listing),
        Read::Tree(path) => (path, Looked::Tree { package: false }),
        Read::Package(path) => (path, Looked::Tree { package: true }),
        _ => return Digest::Nothing,
    };
    let _look = tracing::info_span!(
        target: "blaze",
        "request certificate: a look at a path outside the views",
        function = path.as_str(),
        how = ?how,
    )
    .entered();
    // A look of no build's: its answer is the digest, compared with the certificate's or
    // valued into it.
    let disk = blaze_usercode::inputs::Disk::new(Vec::new(), &Looks::none()); // alloc: none, no variable is asked of it
    let mut now = disk.now();
    let at = std::path::Path::new(path);
    // The question the step's record asked ([`Looked`]), asked again of the same table.
    let found = match how {
        Looked::File => now.file(at),
        Looked::Entry => now.entry(at).map(|entry| blaze_usercode::trace::entry_digest(&entry)),
        Looked::Listing => now.dir(at),
        Looked::Tree { package } => now.tree(at, package),
    };
    outside_digest(found.map_err(|_| ()), path)
}

/// The read a traced step's look `how` at `path` is in a certificate.
fn read_of(how: Looked, path: String) -> Read {
    match how {
        Looked::File => Read::Outside(path),
        Looked::Entry => Read::Entry(path),
        Looked::Listing => Read::Listing(path),
        Looked::Tree { package: false } => Read::Tree(path),
        Looked::Tree { package: true } => Read::Package(path),
    }
}

/// Each of a build's own looks at the disk (`looked`, [`Looks::all`]) as a read of its own
/// certificate, valued at what that look saw: a file at its content's hash, an entry at its
/// [`blaze_usercode::trace::entry_digest`], a listing at its names'. A check asks each again
/// of the disk ([`outside_now`]) and misses where one moved. A path that is no UTF-8 is
/// refused by name.
pub(super) fn looked_reads(looked: &[Look]) -> Result<Vec<(Read, Digest)>, String> {
    let mut valued = Vec::with_capacity(looked.len()); // alloc: once a build, a read a look
    for Look { path, kind, answer, .. } in looked {
        if request::planted(request::Plant::DropsLooked(*kind)) {
            continue;
        }
        let Some(path) = path.to_str() else {
            // alloc: a reason, once a build
            return Err(format!("no request certificate is written: {} is no UTF-8 path", path.display()));
        };
        // alloc: a name
        valued.push((read_of(looked_as(*kind), path.to_owned()), outside_digest(Ok(*answer), path)));
    }
    Ok(valued)
}

/// The question a build's own look of `kind` asked, as a traced step's would ([`read_of`]).
fn looked_as(kind: LookKind) -> Looked {
    match kind {
        LookKind::File => Looked::File,
        LookKind::Entry => Looked::Entry,
        LookKind::Listing => Looked::Listing,
    }
}

/// The identity of an output's `bytes` in the form the record names it by (`wanted`): the
/// file hashed whole (`output_key`), or, for a file blaze's linker made, the root of the
/// linker's digest (`linked_key`). Both are functions of the bytes alone.
fn content_of(bytes: &[u8], wanted: Option<ContentId>) -> ContentId {
    use super::super::unit::{linked_key, output_key};
    let whole = output_key(bytes).0;
    if wanted == Some(whole) || wanted.is_none() {
        return whole;
    }
    let linked = linked_key(&blaze_link::digest::Digest::of(bytes).root()).0;
    if wanted == Some(linked) { linked } else { whole }
}

/// What is at an output's path as the jobs placed it ([`outputs_of`]).
pub(super) enum AtPath {
    /// The build placed no file there (debug info beside an image it did not write).
    Nothing,
    /// A placement in flight: what the file holds is not known yet.
    Placing,
    /// The file placed, by the key of its bytes, with the store's look at the path after its
    /// rename and when that look was asked for (`blaze_cas::PlacedStamp`): `None` where the
    /// store vouched for no look, and the next check hashes the output once.
    Placed { content: ContentId, stamp: Option<(Stamp, u64)> },
}

/// The outputs a build of a plan whose outputs are at `paths` left, as its certificate names
/// them: the file placed at each path (`file`), and for each [`CodeHolder`] that holds a
/// program at a path (`code`), what it holds, under the holder's base
/// ([`certificate::code_base`]). That code is what a run of the program executes, and no file
/// says which build it is of (a launcher's file is the same bytes at every build; a dev
/// executable runs its memory over whatever file is there), so a certificate without it
/// would answer for a build whose code is not the one held (rule 21). A holder that holds
/// code of no build blaze can name leaves no certificate, by name: never an entry left out.
/// A sans-IO core: the driver and the tests hand in what is placed and held.
pub(super) fn outputs_of<'p>(
    paths: impl IntoIterator<Item = &'p str>,
    file: impl Fn(&str) -> AtPath,
    code: impl Fn(CodeHolder, &str) -> Option<HeldCode>,
) -> Result<Vec<(certificate::Output, Option<(Stamp, u64)>)>, String> {
    let mut outputs = Vec::new(); // alloc: once a build, an entry an output
    for path in paths {
        match file(path) {
            AtPath::Placed { content, stamp } => outputs.push((
                // alloc: an output's own path, once a build
                certificate::Output { base: ABSOLUTE, path: path.to_owned(), content },
                stamp,
            )),
            AtPath::Placing => {
                return Err(format!("no request certificate is written: the output {path} is still being placed"));
            }
            AtPath::Nothing => {}
        }
        if request::planted(request::Plant::LeavesCodeOut) {
            continue;
        }
        for holder in CodeHolder::ALL {
            match code(holder, path) {
                Some(HeldCode::Is(content)) => outputs.push((
                    // alloc: as above
                    certificate::Output { base: certificate::code_base(holder), path: path.to_owned(), content },
                    None,
                )),
                Some(HeldCode::Unnamed) => {
                    return Err(format!(
                        "no request certificate is written: the program at {path} runs the code {holder} holds, which is of no build blaze can name (its last build failed, or its unit has no record key)"
                    ));
                }
                None => {}
            }
        }
    }
    Ok(outputs)
}

/// What the driver tells the machine of `ask` where it is a look at a [`CodeHolder`]: the
/// identity of the build whose code `steps` holds there, read in place; `None` for a look at
/// a file. A holder is never stamped: a stamp asked of one finds nothing (looked at
/// `looked_ns`, the driver's wall clock), and the request misses and builds.
fn code_told<S: Steps>(steps: &S, ask: &Ask, looked_ns: u64) -> Option<Told> {
    let (Ask::Stamp { base, path } | Ask::Content { base, path }) = ask else { return None };
    let holder = certificate::holder_of(*base)?;
    Some(match ask {
        Ask::Content { .. } => Told::Content(match steps.held_code(holder, path) {
            Some(HeldCode::Is(content)) => Some(content),
            Some(HeldCode::Unnamed) | None => None,
        }),
        _ => Told::Stamp { stamp: None, looked_ns },
    })
}

/// Why a request is not checked.
enum Unchecked {
    /// It has no certificate to be checked against, and is built: why.
    Refused(String),
    /// It cannot be checked, and fails: what.
    Failed(String),
}

/// A path dependency outside the view that no watch vouches for, named: it is read again by
/// the build before its config takes its mount, and until then its content id is not known.
fn stale_local<S: Steps>(w: &World<'_, S>) -> Option<String> {
    let stale = w.mounts.stale_locals(vouched_local, super::super::fetch::local::vouched_file, super::super::fetch::local::absent);
    let at = stale.first()?;
    // alloc: a reason, once a request
    Some(format!(
        "a path outside the view is read again before it can be vouched for: {at} ({}); {} such in all",
        blaze_vfs::outside::why_looked(std::path::Path::new(at)),
        stale.len(),
    ))
}

/// The place blaze's own copy of the standard library is mounted at, where the journal's mark
/// of it vouches (`local::LibraryMark::vouches`): its `DIGEST` file's stamp now is the one
/// marked, the mount it names is held, and it is the library this blaze reads. Then a start
/// knows its library with no job and no file walked. `None`, with no look at the disk, where
/// there is no mark; `None` where the library is not blaze's own copy
/// (`toolchain::library::owned`: a developer's tree is walked as a job, file by file) or the
/// mark does not vouch. A mark this blaze does not read is said and removed.
fn library_by_mark<S: Steps>(w: &mut World<'_, S>) -> Option<String> {
    let mark = match local::LibraryMark::decode(local::LIBRARY, w.vfs.marked(local::LIBRARY)?) {
        Ok(mark) => mark,
        Err(why) => {
            tracing::warn!(target: "blaze", "request certificate: {why}: it vouches for nothing and is removed; the library is walked");
            w.vfs.unmark(local::LIBRARY);
            return None;
        }
    };
    let _by_mark = tracing::info_span!(target: "blaze", "request certificate: the library by its mark").entered();
    let library = crate::toolchain::library::found().ok()?;
    let digest_file = crate::toolchain::library::owned(library)?;
    let now = local::stamp_now(&digest_file).ok()?;
    let at = blaze_units::stdlib::library_at(&library.commit);
    mark.vouches(&now, &w.mounts.libraries(w.vfs), &at).then_some(at)
}

/// The mark of this daemon's library, mounted at `at`, written where it is blaze's own copy
/// and the mark does not say this mount already: its `DIGEST` file's stamp, when looked at,
/// and the mount's fingerprint and place ([`library_by_mark`] reads it at the next start).
/// Not waited for: a mark lost to a crash leaves the next start walking the library once.
fn mark_library<S: Steps>(w: &mut World<'_, S>, at: &str) {
    let mounted = w.mounts.libraries(w.vfs);
    let Some((fingerprint, _)) = mounted.iter().find(|(_, place)| place == at) else { return };
    let marked = w.vfs.marked(local::LIBRARY).map(|it| local::LibraryMark::decode(local::LIBRARY, it));
    if let Some(Ok(mark)) = &marked
        && mark.fingerprint == *fingerprint
        && mark.at == at
        && crate::toolchain::library::found().ok().and_then(crate::toolchain::library::owned).is_some_and(|digest| {
            local::stamp_now(&digest).is_ok_and(|now| now == mark.digest_file)
        })
    {
        return;
    }
    let Ok(library) = crate::toolchain::library::found() else { return };
    let Some(digest_file) = crate::toolchain::library::owned(library) else { return };
    if blaze_units::stdlib::library_at(&library.commit) != at {
        return;
    }
    let _marking = tracing::info_span!(target: "blaze", "request certificate: the library's mark").entered();
    // Stamped on the real disk (`stamp_now`), not through the driver: by the system's clock.
    let looked_ns = blaze_core::system_wall_ns();
    let Ok(digest_file) = local::stamp_now(&digest_file) else { return };
    // clone: the mark's own strings, once a daemon
    let mark = local::LibraryMark { digest_file, looked_ns, fingerprint: fingerprint.clone(), at: at.to_owned() };
    if let Err(too) = w.vfs.mark(local::LIBRARY, &mark.encode()) {
        tracing::error!(target: "blaze", "request certificate: {too}");
    }
}

/// Whether a request whose environment is `env` asks for its build's trace.
fn traced_by(env: &[(String, String)]) -> bool {
    blaze_units::query::env_var(env, crate::cli::client_rows::PROFILE).is_some_and(|it| !it.is_empty())
}

/// The check of job `id`'s request, whose view and work made `viewed`, `request`, `now` (its
/// pinned view's root) and `drawn`; served by the blaze `tool` names. A check the job
/// itself holds ([`Jobs::certify_late`]) until its caller says otherwise.
#[expect(clippy::too_many_arguments, reason = "the parts of one request, each of another type")]
fn shaped(
    tool: &mut Option<[u8; 32]>,
    id: JobId,
    viewed: Viewed,
    request: ContentId,
    now: ContentId,
    drawn: ContentId,
    idle: bool,
    span: tracing::Span,
) -> Result<Checking, Unchecked> {
    let Viewed { root, config, bare, shape, .. } = viewed;
    let Some(root) = root else {
        // alloc: a reason, once a request on an agent's view
        return Err(Unchecked::Refused("its view is no checkout's: an agent's view has no certificate".to_owned()));
    };
    let tool = tool_of(tool).map_err(Unchecked::Failed)?;
    let env = config.invocation.env;
    let traced = traced_by(&env);
    let base = base_of(shape, bare, request, &tool);
    let machine = request::Request::new(base, traced, unasked_of(&env));
    Ok(Checking {
        id,
        view: None,
        work: None,
        idle,
        late: true,
        machine,
        root,
        shape,
        base,
        now,
        env,
        drawn,
        asks: Vec::new(), // alloc: none until the machine asks
        open: 0,
        asked_ns: 0,
        told: Vec::new(), // alloc: none until a read is told
        span,
    })
}

impl<S: Steps> Jobs<S> {
    /// Job `id`, asked on `view` for `work`, checked against its certificate before it is a
    /// job. `Some`: it is to be built now ([`Jobs::begin_job`]), with what its walk is named
    /// by. `None`: it was answered, or its check waits for looks at its outputs and goes on
    /// at their completions ([`Jobs::certify_told`]).
    pub(super) fn certify_begin(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        view: Option<Opened>,
        work: Work,
        idle: bool,
    ) -> Option<(Option<Opened>, Work, Option<(String, ContentId)>)> {
        self.refused = None;
        // A job that waited for the library and ended otherwise left its id here.
        self.certify.late.retain(|it| *it != id);
        let (Some(opened), Some(request)) = (view.as_ref(), request_id(&work)) else {
            return Some((view, work, None));
        };
        match &self.library {
            Some(at) if !self.certify.library_marked => {
                self.certify.library_marked = true;
                mark_library(w, at);
            }
            Some(_) => {}
            None => {
                if let Some(at) = library_by_mark(w) {
                    self.library = Some(at);
                }
            }
        }
        // clone: the config owns the library's place, once a request
        let Some(library) = self.library.clone() else {
            // The config, and so the base, names where the standard library is mounted and
            // holds it by content. This daemon has not looked at the library yet: it does,
            // once its life, off the loop, as the first stage of this request's job
            // (`Does::Library`: the library's fingerprint against the mounts its store
            // restored), and the request is checked when that is in ([`Jobs::certify_late`]),
            // before any view is opened or plan made. A store that restored no library has
            // no certificate this daemon could find: its config will hold a mount made now.
            if w.mounts.libraries(w.vfs).is_empty() {
                // alloc: a reason, once a store's first build
                self.refused = Some("the standard library was never mounted from this store: this is its first build".to_owned());
            } else {
                self.certify.late.push(id);
            }
            return Some((view, work, None));
        };
        if let Some(why) = stale_local(w) {
            self.refused = Some(why);
            return Some((view, work, None));
        }
        let span = tracing::info_span!(target: "blaze", "request certificate", job = %id);
        let keying = span.in_scope(|| {
            tracing::info_span!(target: "blaze", "request certificate: the request's shape").entered()
        });
        let viewed = Self::viewed(&self.setup, &mut self.roots, w, &work, opened, &[], library);
        let (now, drawn) = (opened.view.root, drawn_id(&work));
        let shaped = shaped(&mut self.certify.tool, id, viewed, request, now, drawn, idle, span);
        drop(keying);
        match shaped {
            Ok(mut checking) => {
                (checking.view, checking.work, checking.late) = (view, Some(work), false);
                self.certify.checking.push(checking);
                self.certify_step(w, id);
                None
            }
            Err(Unchecked::Refused(why)) => {
                self.refused = Some(why);
                Some((view, work, None))
            }
            // No fallback: a request this blaze cannot name itself for is not built unchecked.
            Err(Unchecked::Failed(what)) => {
                tracing::error!(target: "blaze", job = %id, "{what}");
                w.said.push(Said::Finish(id, false, Some(ApiError::new(what, None)), None));
                None
            }
        }
    }

    /// The standard library is in for this daemon's life and job `id` waited for it
    /// ([`Jobs::certify_begin`]): the job's request is checked now, before its view is
    /// opened. True: the check has the job (it waits for looks at its outputs, or it ended:
    /// answered, or failed). False: the job goes on to its build.
    pub(super) fn certify_late(&mut self, w: &mut World<'_, S>, id: JobId) -> bool {
        let Some(at) = self.certify.late.iter().position(|it| *it == id) else { return false };
        self.certify.late.swap_remove(at);
        // clone: the config owns the library's place, once a request
        let Some(library) = self.library.clone() else { return false };
        if let Some(why) = stale_local(w) {
            tracing::info_span!(target: "blaze", "request certificate: not checked", job = %id, why = %why).in_scope(|| {});
            return false;
        }
        let span = tracing::info_span!(target: "blaze", "request certificate", job = %id, late = true);
        let keying = span.in_scope(|| {
            tracing::info_span!(target: "blaze", "request certificate: the request's shape").entered()
        });
        let shaped = {
            let Some(job) = self.jobs.get(key_of(id)) else { return false };
            let (Some(request), [opened]) = (request_id(&job.work), job.views.as_slice()) else { return false };
            let viewed = Self::viewed(&self.setup, &mut self.roots, w, &job.work, opened, &job.relocated, library);
            let (now, drawn) = (opened.view.root, drawn_id(&job.work));
            shaped(&mut self.certify.tool, id, viewed, request, now, drawn, job.idle, span)
        };
        drop(keying);
        match shaped {
            Ok(checking) => {
                if let Some(job) = self.jobs.get_mut(key_of(id)) {
                    job.stage = Stage::Certify;
                }
                self.certify.checking.push(checking);
                self.certify_step(w, id);
                true
            }
            Err(Unchecked::Refused(why)) => {
                tracing::info_span!(target: "blaze", "request certificate: not checked", job = %id, why = %why).in_scope(|| {});
                false
            }
            Err(Unchecked::Failed(what)) => {
                tracing::error!(target: "blaze", job = %id, "{what}");
                self.end(w, id, false, Some(ApiError::new(what, None)));
                true
            }
        }
    }

    /// The check of job `id` stepped until it waits for looks in flight or ends: a hit is
    /// said and finished, a miss becomes the job.
    fn certify_step(&mut self, w: &mut World<'_, S>, id: JobId) {
        for _ in 0..MOST_ROUNDS {
            let Some(at) = self.certify.checking.iter().position(|it| it.id == id) else { return };
            if self.certify.checking[at].open > 0 {
                return;
            }
            let (next, tickets) = {
                let Certify { tally, checking, evidence, .. } = &mut self.certify;
                let checking = &mut checking[at];
                let _step = checking.span.in_scope(|| {
                    tracing::info_span!(target: "blaze", "request certificate: a step", awaits = checking.machine.waiting()).entered()
                });
                let records = Records::new(&mut *w.store, tally);
                let held = evidence_at(evidence, checking.root, checking.shape, w.vfs);
                let next = checking.machine.step(&records, &mut evidence[held].2);
                mark_changed(evidence, held, w.vfs);
                (next, records.tickets.into_inner())
            };
            for ticket in tickets {
                self.put.add(ticket);
            }
            match next {
                Ok(Next::Asks(asks)) => self.certify_ask(w, id, at, asks),
                Ok(Next::Hit(answer)) => {
                    let checking = self.certify.checking.swap_remove(at);
                    self.certify_hit(w, checking, answer);
                    return;
                }
                Ok(Next::Miss(miss)) => {
                    let checking = self.certify.checking.swap_remove(at);
                    self.certify_miss(w, checking, miss);
                    return;
                }
                Ok(other) => {
                    let checking = self.certify.checking.swap_remove(at);
                    // alloc: an error, once
                    let what = format!("the request certificate's check of job {id} was stepped out of turn: {other:?}");
                    self.certify_stopped(w, checking, what);
                    return;
                }
                Err(stopped) => {
                    let checking = self.certify.checking.swap_remove(at);
                    self.certify_stopped(w, checking, stopped.to_string());
                    return;
                }
            }
        }
        if let Some(at) = self.certify.checking.iter().position(|it| it.id == id)
            && self.certify.checking[at].open == 0
        {
            let checking = self.certify.checking.swap_remove(at);
            // alloc: an error, once
            let what = format!("the request certificate's check of job {id} did not end in {MOST_ROUNDS} steps");
            self.certify_stopped(w, checking, what);
        }
    }

    /// Each of `asks` of the check at `at` answered where the loop holds the answer, else
    /// asked of the driver.
    fn certify_ask(&mut self, w: &mut World<'_, S>, id: JobId, at: usize, asks: Vec<Ask>) {
        let asked_ns = w.wall_ns;
        self.certify.checking[at].asked_ns = asked_ns;
        for (n, ask) in asks.iter().enumerate() {
            let held = code_told(&**w.steps, ask, asked_ns);
            let told = match ask {
                _ if held.is_some() => held,
                Ask::Read { read, view } => {
                    let checking = &self.certify.checking[at];
                    // A check reads its request's pinned view alone.
                    debug_assert!(*view == View::Now);
                    Some(Told::Read(match read {
                        Read::Node(path) if path.is_empty() => Digest::Is(checking.now),
                        Read::Node(path) => {
                            w.vfs.reader().dir_id(checking.now, path).map_or(Digest::Nothing, Digest::Is)
                        }
                        Read::Env(name) => env_digest(&checking.env, name),
                        Read::Drawn => Digest::Is(checking.drawn),
                        Read::Outside(_) | Read::Entry(_) | Read::Listing(_) | Read::Tree(_) | Read::Package(_) => {
                            checking.span.in_scope(|| outside_now(read))
                        }
                    }))
                }
                Ask::Stamp { path, .. } | Ask::Content { path, .. } => {
                    let interned = self.paths.intern(path);
                    // clone: the driver's effect owns the file's name, once a look
                    let file = self.paths.files[interned as usize].clone();
                    let tag = file.as_ref().and_then(|_| self.own_tag(Own::Certify { job: id, ask: n as u32 }).ok());
                    match (file, tag, ask) {
                        (Some(file), Some(tag), Ask::Stamp { .. }) => {
                            // A batch of one name: the whole stamp (inode, times, mode), which
                            // the evidence and the marks compare whole.
                            let mut files = blaze_core::Names::with_capacity(1, file.as_str().len() + 1);
                            files.push(&file);
                            // alloc: the effect's batch of one name, once a look
                            w.effects.push(Effect::Stamp { files: std::sync::Arc::new(files), tag });
                            None
                        }
                        (Some(file), Some(tag), _) => {
                            w.effects.push(Effect::ReadAll { file, tag });
                            None
                        }
                        // No file the driver can name, or no tag left for a look: nothing is
                        // seen there, the request misses on the output and its build places it.
                        (_, _, Ask::Stamp { .. }) => Some(Told::Stamp { stamp: None, looked_ns: asked_ns }),
                        _ => Some(Told::Content(None)),
                    }
                }
            };
            let checking = &mut self.certify.checking[at];
            match told {
                Some(told) => {
                    if let (Ask::Read { read, .. }, Told::Read(digest)) = (ask, &told) {
                        checking.told.push((read.clone(), *digest)); // clone: the slot's own read, once a read
                    }
                    if let Err(misused) = checking.machine.told(ask, told) {
                        tracing::error!(target: "blaze", job = %id, "request certificate: {misused}");
                    }
                }
                None => checking.open += 1,
            }
        }
        self.certify.checking[at].asks = asks;
    }

    /// The look `ask` of job `job`'s check is done.
    pub(super) fn certify_told(&mut self, w: &mut World<'_, S>, job: JobId, ask: u32, done: Done) {
        // A check that was cancelled: its looks in flight are let be.
        let Some(at) = self.certify.checking.iter().position(|it| it.id == job) else { return };
        let checking = &mut self.certify.checking[at];
        // clone: one ask, once a look
        let Some(asked) = checking.asks.get(ask as usize).cloned() else { return };
        let told = match (&asked, done) {
            // Nothing there, a stamp the system would not give, or no regular file: nothing a
            // build placed is there.
            (Ask::Stamp { path, .. }, Done::Stamped(stamps)) => match stamps.as_slice() {
                [stamp] => Ok(Told::Stamp {
                    stamp: (!stamp.is_gone()
                        && !stamp.is_unknown()
                        && stamp.mode & blaze_core::S_IFMT == blaze_core::S_IFREG)
                        .then_some(*stamp),
                    looked_ns: checking.asked_ns,
                }),
                // alloc: an error, once
                stamps => Err(format!("the request certificate: looking at the output {path}: {} stamps came back for one name", stamps.len())),
            },
            (Ask::Content { base, path }, Done::Read(bytes)) => {
                let _hash = checking.span.in_scope(|| {
                    tracing::info_span!(
                        target: "blaze",
                        "request certificate: hash an output no stamp vouches for",
                        function = path.as_str(),
                        bytes = bytes.len(),
                    )
                    .entered()
                });
                Ok(Told::Content(Some(content_of(&bytes, checking.machine.wanted(*base, path)))))
            }
            // Gone between its stamp and its read: nothing is there.
            (Ask::Content { path, .. }, Done::Failed(error)) => {
                tracing::info!(target: "blaze", function = path.as_str(), "request certificate: an output could not be read: {error}");
                Ok(Told::Content(None))
            }
            // alloc: an error, once
            (Ask::Stamp { path, .. }, Done::Failed(error)) => Err(format!("the request certificate: looking at the output {path}: {error}")),
            (asked, done) => Err(format!("the request certificate: {asked:?} was answered with {done:?}")), // alloc: an error, once
        };
        checking.open = checking.open.saturating_sub(1);
        let told = told.and_then(|told| checking.machine.told(&asked, told).map_err(|it| it.to_string()));
        match told {
            Ok(()) => self.certify_step(w, job),
            Err(what) => {
                let checking = self.certify.checking.swap_remove(at);
                self.certify_stopped(w, checking, what);
            }
        }
    }

    /// The request is certified: what its build said is said again, and it finishes ok. No
    /// job: no request of the plane, no plan, no task.
    fn certify_hit(&mut self, w: &mut World<'_, S>, checking: Checking, answer: certificate::Answer) {
        let Checking { id, span, root, shape, late, base, drawn, told, ref env, ref work, .. } = checking;
        self.certify_verified(root, shape);
        let asked = match work {
            Some(work) => slot_asked(work),
            None => self.jobs.get(key_of(id)).and_then(|job| slot_asked(&job.work)),
        };
        span.in_scope(|| {
            let traced = traced_by(env);
            // clone: the slot's own environment, once a hit
            self.slot_begin(w, root, asked, base, traced, drawn, env.clone(), &told, &answer.outputs);
        });
        let _hit = span.in_scope(|| {
            tracing::info_span!(
                target: "blaze",
                "request certificate: hit",
                root,
                outputs = answer.outputs.len(),
            )
            .entered()
        });
        match blaze_protocol::cbor::from_slice::<Vec<Response>>(&answer.said) {
            Ok(responses) => {
                for response in responses {
                    w.said.push(Said::Report(id, response));
                }
                match late {
                    // The job that waited for the library ends with nothing more to do:
                    // no view opened, no plan, no unit.
                    true => self.end(w, id, true, None),
                    false => w.said.push(Said::Finish(id, true, None, None)),
                }
            }
            Err(error) => {
                // alloc: an error, once
                let what = format!("the request certificate's answer does not decode as what a build says: {error}");
                match late {
                    true => self.end(w, id, false, Some(ApiError::new(what, None))),
                    false => w.said.push(Said::Finish(id, false, Some(ApiError::new(what, None)), None)),
                }
            }
        }
    }

    /// Each file output the hit of checkout `root`'s shape `shape` verified
    /// ([`Evidence::verified`]) recorded as placed where the jobs hold no record of its path (a
    /// start): by its content's key under the stamp the check saw, as a placement of it would
    /// be. What `placed_key` and the next build's look at the path read, so a build after a
    /// start places no output again that is as it was placed, and a run adopts it (R2).
    fn certify_verified(&mut self, root: u32, shape: ContentId) {
        let Some(held) = self.certify.evidence.iter_mut().find(|it| it.0 == root && it.1 == shape) else { return };
        for verified in std::mem::take(&mut held.2.verified) {
            if verified.base != ABSOLUTE {
                continue;
            }
            let id = self.paths.intern(&verified.path) as usize;
            if self.paths.placed[id].is_some() {
                continue;
            }
            let stamp = verified.stamp;
            self.paths.placed[id] = Some(Placement {
                key: blaze_cas::Key(verified.content),
                executable: stamp.mode & 0o111 != 0,
                meta: Some(Meta { len: stamp.size, dir: false, modified_ns: stamp.mtime_ns }),
                asked_ns: verified.looked_ns,
                stamp: Some(blaze_cas::PlacedStamp { stamp, looked_ns: verified.looked_ns }),
            });
        }
    }

    /// The request is not certified: why is said in a span of the check and again in its
    /// build ([`Jobs::refused`]), and it becomes a job.
    fn certify_miss(&mut self, w: &mut World<'_, S>, checking: Checking, miss: request::Miss) {
        let Checking { id, view, work, idle, late, span, root, base, .. } = checking;
        span.in_scope(|| {
            tracing::info_span!(
                target: "blaze",
                "request certificate: miss",
                root,
                lists = miss.lists,
                why = %miss.why,
            )
            .in_scope(|| {});
            for refused in &miss.refused {
                tracing::warn!(target: "blaze", root, "request certificate: a record was not read: {refused:?}");
            }
        });
        drop(span);
        let why = miss.why.to_string(); // alloc: a reason, once a build
        let walks = match miss.why {
            request::Why::Traced => None,
            _ => Some((format!("request certificate: {why}"), base)), // alloc: once a build
        };
        match (late, work) {
            (false, Some(work)) => {
                self.refused = Some(why);
                self.begin_job(w, id, view, work, idle, walks);
            }
            // The job that waited for the library goes on to its build: its view is opened.
            (true, _) => self.prepare(w, id),
            (false, None) => {
                // alloc: an error, once
                let what = format!("the request certificate's check of job {id} missed and holds no work to build: {why}");
                w.said.push(Said::Finish(id, false, Some(ApiError::new(what, None)), None));
            }
        }
    }

    /// The check stopped with no outcome (the store, a look, the machine out of turn): the
    /// request fails with what stopped it. Never a miss: nothing is built in its place.
    fn certify_stopped(&mut self, w: &mut World<'_, S>, checking: Checking, what: String) {
        let Checking { id, span, late, .. } = checking;
        span.in_scope(|| tracing::error!(target: "blaze", job = %id, "{what}"));
        match late {
            true => self.end(w, id, false, Some(ApiError::new(what, None))),
            false => w.said.push(Said::Finish(id, false, Some(ApiError::new(what, None)), None)),
        }
    }

    /// Job `id` was cancelled: a check of it under way is dropped and the job finishes as a
    /// cancelled one does. Its looks in flight complete to nothing.
    pub(super) fn certify_cancel(&mut self, w: &mut World<'_, S>, id: JobId) {
        self.certify.late.retain(|it| *it != id);
        if let Some(at) = self.certify.checking.iter().position(|it| it.id == id) {
            // A late check's job is cancelled as any job is, by its caller.
            if !self.certify.checking.swap_remove(at).late {
                w.said.push(Said::Finish(id, false, None, None));
            }
        }
    }

    /// Job `id` builds checkout `root`'s request of shape `shape` on `inputs` from now: what
    /// it says and reads is its request's certificate at its end. A job whose view is opened
    /// again (after a fetch, or with the lock file it wrote) is heard anew: a build of what
    /// it left opens once.
    ///
    /// Its looks at the disk are a list of its own from here ([`Building::looks`]), handed to
    /// each task the job starts with its [`JobInput`]: a view opened again starts another,
    /// as what the first opening's tasks looked at is not what this build rests on.
    pub(super) fn certify_building(&mut self, id: JobId, root: u32, shape: ContentId, inputs: Inputs) {
        self.certify.building.retain(|it| it.job != id);
        let looks = blaze_vfs::outside::Looks::new();
        let mut asked = None;
        if let Some(job) = self.jobs.get_mut(key_of(id)) {
            job.looks = looks.clone(); // clone: the list's handle, the build keeps another
            asked = slot_asked(&job.work);
        }
        self.certify.building.push(Building {
            job: id,
            root,
            shape,
            inputs,
            saying: said::Saying::new(),
            looks: looks.clone(), // clone: the list's handle, the job's tasks hold another
            // alloc: a reason, once a build
            named: Err("its steps were not asked which variables they read by name".to_owned()),
            // alloc: as above
            read: Err("its steps were not asked which paths they read".to_owned()),
            // alloc: as above
            seen: Err("its steps were not asked what their records read".to_owned()),
            asked,
        });
    }

    /// What the daemon tells the session of a job, heard for the job's [`Building`]: each
    /// response, and at its `Finish` the build handed over ([`Ended`]).
    pub(super) fn certify_heard(&mut self, said: &Said) {
        if self.certify.building.is_empty() {
            return;
        }
        match said {
            Said::Report(id, response) => {
                if let Some(it) = self.certify.building.iter_mut().find(|it| it.job == *id) {
                    it.saying.hear(response);
                }
            }
            Said::Finish(id, ok, error, _) => {
                let Some(at) = self.certify.building.iter().position(|it| it.job == *id) else { return };
                let building = self.certify.building.swap_remove(at);
                self.certify.ended.push(building.end(*ok && error.is_none()));
            }
        }
    }

    /// Each build that ended since the last call ([`Ended`]) handed to a request machine, and
    /// its certificate written; or why none is, in a span and a warning. On the loop, with
    /// the store in hand.
    pub(super) fn certify_written(&mut self, w: &mut World<'_, S>) {
        if self.certify.ended.is_empty() {
            return;
        }
        for ended in std::mem::take(&mut self.certify.ended) {
            let Ended { root, shape, built } = ended;
            let span = tracing::info_span!(target: "blaze", "request certificate: written at a build's end", root);
            let _entered = span.enter();
            let walked = match built {
                Ok(walked) => walked,
                Err(why) => {
                    tracing::info_span!(target: "blaze", "request certificate: not written", root, why = %why).in_scope(|| {});
                    continue;
                }
            };
            if let Err(why) = self.certify_write(w, root, shape, walked) {
                tracing::info_span!(target: "blaze", "request certificate: not written", root, why = %why).in_scope(|| {});
                tracing::warn!(target: "blaze", root, "{why}");
            }
        }
    }

    /// The certificate of the build `walked` of checkout `root` and shape `shape`, written.
    fn certify_write(
        &mut self,
        w: &mut World<'_, S>,
        root: u32,
        shape: ContentId,
        walked: Walked,
    ) -> Result<(), String> {
        let env = &walked.inputs.config.invocation.env;
        let mut untraced = Vec::new(); // alloc: none unless a step was untraced
        // alloc: once a build, a digest a read
        let mut valued: Vec<(Read, Digest)> = Vec::new();
        match &walked.named {
            Ok(names) => {
                for name in names.iter() {
                    // clone: a read's own name, once a build
                    valued.push((Read::Env(name.clone()), env_digest(env, name)));
                }
            }
            // alloc: a reason, once a build
            Err(why) => untraced.push(format!("a step whose variable reads are not named ({why})")),
        }
        // What the build itself looked at outside the views, each at what its look saw: a
        // file's content, an entry (a link not followed), a listing.
        match &walked.looked {
            Ok(looked) => valued.extend(looked_reads(looked)?),
            // A path seen at two values: no certificate, whatever else the build read.
            Err(why) => return Err(why.clone()), // clone: a reason, once a build
        }
        // What its traced steps' records rest on, wherever on the machine: each at the
        // value its record holds it at, never a look now. A step with no trace: no
        // certificate, and the step is named.
        match &walked.seen {
            Ok(seen) => {
                for Seen { how, path, hash } in seen {
                    let Some(path) = path.to_str() else {
                        return Err(format!("no request certificate is written: {} is no UTF-8 path", path.display()));
                    };
                    let digest = outside_digest(Ok(*hash), path);
                    valued.push((read_of(*how, path.to_owned()), digest)); // alloc: a name
                }
            }
            Err(why) => untraced.push(why.clone()), // clone: a reason, once a build
        }
        // What its units' records read under its outputs (`include!`d files, what expansions
        // read): valued by a look now, at the walk's end (certificate.md, "State": those
        // records' own hashes are not handed over). A path a traced step read is valued
        // above and not looked at again.
        // alloc: once a build, the reads valued so far, sorted
        let mut known: Vec<Read> = valued.iter().map(|(read, _)| read.clone()).collect(); // clone: as above
        known.sort_unstable();
        known.dedup();
        match &walked.read {
            Ok((files, dirs)) => {
                let listed = dirs.iter().map(|it| (it, true));
                for (path, listing) in files.iter().map(|it| (it, false)).chain(listed) {
                    let Some(path) = path.to_str() else {
                        return Err(format!("no request certificate is written: {} is no UTF-8 path", path.display()));
                    };
                    let read = if listing { Read::Listing(path.to_owned()) } else { Read::Outside(path.to_owned()) }; // alloc: a name
                    if known.binary_search(&read).is_ok() {
                        continue;
                    }
                    let digest = outside_now(&read);
                    if digest == Digest::Is(unreadable(path)) {
                        return Err(format!("no request certificate is written: {path}, which the build read, could not be read"));
                    }
                    valued.push((read, digest));
                }
            }
            // alloc: a reason, once a build
            Err(why) => untraced.push(format!("a step whose file reads are not named ({why})")),
        }
        if walked.said.iter().any(|it| matches!(it, Response::Diagnostic(_))) {
            valued.push((Read::Drawn, Digest::Is(walked.inputs.drawn)));
        }
        // The outputs of the shape's plan as they are placed: each by the key of its bytes,
        // with the store's look at the path after its rename and when it was asked for
        // (`blaze_cas::PlacedStamp`, compared whole later, change time included). The machine
        // keeps the stamp only where it is steady (`blaze_vfs::racy`): an output this build
        // wrote inside its window, or chmod'ed after its place (a later change time than the
        // store's look), is hashed once by a later check and kept from then on.
        // And what each place that is no file holds for a program of the plan: the code a run
        // of it executes ([`outputs_of`]).
        let Some(plan) = self.roots.plans[root as usize].iter().find(|it| it.shape == shape) else {
            return Err("no request certificate is written: the checkout keeps no plan of the request's shape to name its outputs by".to_owned());
        };
        let paths = &self.paths;
        let outputs = outputs_of(
            plan.units.plans.iter().flat_map(|it| it.outputs.iter()).map(String::as_str),
            |path| match paths.ids.get(path).and_then(|id| paths.placed[*id as usize].as_ref()) {
                Some(Placement { key, meta: Some(_), stamp, .. }) => AtPath::Placed {
                    content: key.0,
                    stamp: stamp.as_ref().map(|it| (it.stamp, it.looked_ns)),
                },
                Some(_) => AtPath::Placing,
                None => AtPath::Nothing,
            },
            |holder, path| w.steps.held_code(holder, path),
        )?;
        let said = blaze_protocol::cbor::to_vec(&walked.said)
            .map_err(|error| format!("no request certificate is written: what the build said does not encode: {error}"))?;
        let tool = tool_of(&mut self.certify.tool).map_err(|why| format!("no request certificate is written: {why}"))?;
        let base = base_of(shape, walked.inputs.config_id, walked.inputs.request, &tool);
        let mut machine = request::Request::building(base, unasked_of(env));
        // clone: what the request's slot is of, once a build
        let (slot_reads, slot_outputs) = (valued.clone(), outputs.iter().map(|(it, _)| it.clone()).collect::<Vec<_>>());
        let built = request::Built {
            nodes: vec![String::new()], // alloc: the one node, the view's root
            valued,
            outputs,
            units: Vec::new(), // alloc: none
            said,
            untraced,
        };
        machine.built(built).map_err(|it| it.to_string())?;
        for _ in 0..MOST_ROUNDS {
            let (next, tickets) = {
                let Certify { tally, evidence, .. } = &mut self.certify;
                let _step = tracing::info_span!(target: "blaze", "request certificate: a step", awaits = machine.waiting()).entered();
                let records = Records::new(&mut *w.store, tally);
                let held = evidence_at(evidence, root, shape, w.vfs);
                let next = machine.step(&records, &mut evidence[held].2);
                mark_changed(evidence, held, w.vfs);
                (next, records.tickets.into_inner())
            };
            for ticket in tickets {
                self.put.add(ticket);
            }
            match next {
                Ok(Next::Asks(asks)) => {
                    for ask in &asks {
                        let digest = match ask {
                            // The view the build read, its own writes in it: an id of content.
                            Ask::Read { read: Read::Node(path), view: View::Built } if path.is_empty() => Digest::Is(walked.inputs.view),
                            Ask::Read { read: Read::Node(path), view: View::Built } => {
                                w.vfs.reader().dir_id(walked.inputs.view, path).map_or(Digest::Nothing, Digest::Is)
                            }
                            Ask::Read { read: Read::Env(name), .. } => env_digest(env, name),
                            other => return Err(format!("no request certificate is written: the machine asked for {other:?} at a build's end")),
                        };
                        machine.told(ask, Told::Read(digest)).map_err(|it| it.to_string())?;
                    }
                }
                Ok(Next::Written { key, .. }) => {
                    tracing::info_span!(target: "blaze", "request certificate: written", root, key = %format_args!("{key:x}")).in_scope(|| {});
                    let mut reads = slot_reads;
                    reads.push((Read::Node(String::new()), Digest::Is(walked.inputs.view))); // alloc: the root's read
                    // clone: the slot's own, once a build
                    let (asked, env) = (walked.asked.clone(), env.clone());
                    let (traced, drawn) = (traced_by(&env), walked.inputs.drawn);
                    self.slot_begin(w, root, asked, base, traced, drawn, env, &reads, &slot_outputs);
                    return Ok(());
                }
                Ok(Next::Unwritten(why)) => return Err(why.to_string()),
                Ok(other) => return Err(format!("no request certificate is written: the machine was stepped out of turn: {other:?}")),
                Err(stopped) => return Err(format!("no request certificate is written: {stopped}")),
            }
        }
        Err(format!("no request certificate is written: the machine did not end in {MOST_ROUNDS} steps"))
    }
}

/// The premise that the file at `path` (the engine, the library) is as it is now, looked at
/// after `looked_ns`. The driver's one synchronous look, at a slot's start (H9's stamp).
fn slot_premise(path: &std::path::Path, looked_ns: u64) -> Result<blaze_certify::slot::Premise, String> {
    let stamp = crate::serve::local::stamp_now(path)?;
    let path = path.to_str().ok_or_else(|| format!("{} is no UTF-8 path", path.display()))?; // alloc: a reason
    Ok(blaze_certify::slot::Premise { path: path.to_owned(), stamp, looked_ns }) // alloc: the slot's own
}

/// The slots this daemon writes, by request ([`slot`]): begun after a hit and after a written
/// certificate, each stepped through the loop's driver as the check's looks are.
impl<S: Steps> Jobs<S> {
    /// The slot of a request of checkout `root` answered by a certificate of base `base` whose
    /// reads are `reads` and whose outputs are `outputs`, begun; or why none is, in a span.
    #[expect(clippy::too_many_arguments, reason = "the parts of one slot, each of another type")]
    fn slot_begin(
        &mut self,
        w: &mut World<'_, S>,
        root: u32,
        asked: Option<(&'static str, blaze_api::Targets)>,
        base: ContentId,
        traced: bool,
        drawn: ContentId,
        env: Vec<(String, String)>,
        reads: &[(Read, Digest)],
        outputs: &[certificate::Output],
    ) {
        match self.slot_writing(w, root, asked, base, traced, drawn, &env, reads, outputs) {
            Ok(writing) => {
                let n = self.certify.next_slot;
                self.certify.next_slot = (n + 1) % REMOVAL;
                let (names, outside, _) = writing.counts();
                let _write = tracing::info_span!(target: "blaze", "request slot: write", names, outside).entered();
                self.certify.slots.push((n, writing, Waits::Read));
                if self.certify.listed.is_none() {
                    self.slot_list(w);
                }
                self.slot_step(w, n, None);
            }
            Err(why) => {
                tracing::info_span!(target: "blaze", "request slot: not written", why = %why).in_scope(|| {});
            }
        }
    }

    /// The writer of a slot, made from what the daemon holds: the checkout's column, the
    /// premises looked at now, each output's mark.
    #[expect(clippy::too_many_arguments, reason = "the parts of one slot, each of another type")]
    fn slot_writing(
        &mut self,
        w: &mut World<'_, S>,
        root: u32,
        asked: Option<(&'static str, blaze_api::Targets)>,
        base: ContentId,
        traced: bool,
        drawn: ContentId,
        env: &[(String, String)],
        reads: &[(Read, Digest)],
        outputs: &[certificate::Output],
    ) -> Result<slot::Writing, slot::Unslotted> {
        let Some((word, targets)) = asked else { return Err(slot::Unslotted::NoKey) };
        let Some(cargo) = targets.cargo.as_ref() else { return Err(slot::Unslotted::NoKey) };
        let root_dir = cargo.root.as_str();
        // Every path dependency is read where it is on the disk, and none has a column a
        // client can look at yet: no slot while one is mounted.
        if let Some(at) = w.mounts.stale_locals(|_, _| false, |_, _| false, |_| false).into_iter().next() {
            return Err(slot::Unslotted::Local(at));
        }
        let Some(head) = self.roots.ids.iter().find(|(_, id)| **id == root).map(|(head, _)| &**head) else {
            return Err(slot::Unslotted::Moved { read: ContentId([0; 32]), column: None });
        };
        // The premises are stamped on the real disk (`slot_premise`), not through the driver:
        // by the system's clock, which a client's look at them compares with too.
        let looked_ns = blaze_core::system_wall_ns();
        let premises = &self.setup.premises;
        let tool = slot_premise(std::path::Path::new(&premises.engine), looked_ns).map_err(slot::Unslotted::NoTool)?;
        let library = premises
            .library
            .as_deref()
            .and_then(|digest| slot_premise(std::path::Path::new(digest), looked_ns).ok())
            .ok_or(slot::Unslotted::NoLibrary)?;
        let placed = outputs
            .iter()
            .filter(|it| it.base == ABSOLUTE)
            .filter_map(|it| {
                let mark = w.vfs.marked(&placed_mark(&it.path))?;
                let local::Placed { stamp, looked_ns, content } = local::Placed::decode(&placed_mark(&it.path), mark).ok()?;
                slot::placed(&it.path, stamp, looked_ns, content)
            })
            .collect();
        let request = blaze_protocol::cbor::to_vec(&(word, &targets))
            .map_err(|error| slot::Unslotted::Write(format!("the request does not encode: {error}")))?; // alloc: a reason
        let slots = self.setup.slots.clone(); // clone: the slots' directory, once a slot
        let kept = blaze_certify::slot::key_of(word, &targets, root_dir).and_then(|key| {
            self.certify.listed.as_ref()?.iter().find(|it| it.key == key)?.body
        });
        let column = w.vfs.column(head).ok_or(slot::Unslotted::Moved { read: ContentId([0; 32]), column: None })?;
        let of = slot::Of {
            word,
            targets: &targets,
            root_dir,
            slots: &slots,
            base,
            traced,
            drawn,
            env,
            unasked: unasked_of(env),
            reads,
            request,
        };
        let programs = slot::Writing::programs(outputs);
        let column = slot::Saved::of(&column);
        let (files, saved_root) = (w.vfs.reader(), column.root);
        // The id the column's root holds for a name: what a name saved unknown is hashed against.
        let content = |name: &str| files.file(saved_root?, name).ok().flatten().map(|(id, ..)| id);
        let mut writing = slot::Writing::new(of, column, content, Vec::new(), tool, library, placed, kept, w.wall_ns)?;
        writing.set_programs(programs);
        Ok(writing)
    }

    /// `<state>/slots` listed once a daemon's life, before its first slot is kept: the slots
    /// earlier daemons wrote count against the bound.
    fn slot_list(&mut self, w: &mut World<'_, S>) {
        self.certify.listed = Some(Vec::new()); // alloc: none until a slot is kept
        let Some(dir) = FileName::new(self.setup.slots.trim_start_matches('/')) else { return };
        if let Ok(tag) = self.own_tag(Own::Slot { slot: LISTING }) {
            w.effects.push(Effect::List { dir, tag });
        }
    }

    /// The slot numbered `n` stepped with `seen` until it waits at the driver or ends.
    fn slot_step(&mut self, w: &mut World<'_, S>, n: u32, mut seen: Option<slot::Seen>) {
        for _ in 0..MOST_ROUNDS * 4 {
            let Some(at) = self.certify.slots.iter().position(|it| it.0 == n) else { return };
            let step = {
                let Certify { tally, slots, .. } = &mut self.certify;
                let records = Records::new(&mut *w.store, tally);
                slots[at].1.step(&records, seen.take())
            };
            let file = |path: &str| FileName::new(path.trim_start_matches('/'));
            let effect = match step {
                slot::Step::Value(read) => {
                    seen = Some(slot::Seen::Valued(outside_now(&read)));
                    continue;
                }
                slot::Step::Stamps(paths) => {
                    let bytes = paths.iter().map(|it| it.len() + 1).sum();
                    let mut files = blaze_core::Names::with_capacity(paths.len(), bytes);
                    for path in &paths {
                        match file(path) {
                            Some(name) => files.push(&name),
                            None => {
                                seen = Some(slot::Seen::Written(Err(format!("{path} is no file the driver names")))); // alloc: a reason
                                break;
                            }
                        }
                    }
                    if seen.is_some() {
                        continue;
                    }
                    self.certify.slots[at].2 = Waits::Stamps { asked_ns: w.wall_ns };
                    // alloc: the effect's batch, once a look
                    Box::new(move |tag| Effect::Stamp { files: std::sync::Arc::new(files), tag }) as Box<dyn FnOnce(Tag) -> Effect>
                }
                slot::Step::Read(path) => {
                    let Some(name) = file(&path) else {
                        seen = Some(slot::Seen::Read(Err(format!("{path} is no file the driver names")))); // alloc: a reason
                        continue;
                    };
                    self.certify.slots[at].2 = Waits::Read;
                    Box::new(move |tag| Effect::ReadAll { file: name, tag })
                }
                slot::Step::Write { to, bytes } => {
                    let Some(name) = file(&to) else {
                        seen = Some(slot::Seen::Written(Err(format!("{to} is no file the driver names")))); // alloc: a reason
                        continue;
                    };
                    self.certify.slots[at].2 = Waits::Write;
                    Box::new(move |tag| Effect::WriteUnsynced { file: name, bytes, tag })
                }
                slot::Step::Done(outcome) => {
                    let (_, writing, _) = self.certify.slots.swap_remove(at);
                    self.slot_done(w, writing, outcome);
                    return;
                }
            };
            match self.own_tag(Own::Slot { slot: n }) {
                Ok(tag) => w.effects.push(effect(tag)),
                Err(error) => {
                    tracing::info_span!(target: "blaze", "request slot: not written", why = %error.what).in_scope(|| {});
                    self.certify.slots.swap_remove(at);
                }
            }
            return;
        }
        if let Some(at) = self.certify.slots.iter().position(|it| it.0 == n) {
            self.certify.slots.swap_remove(at);
            tracing::info_span!(target: "blaze", "request slot: not written", why = "the writer did not end in its bound of steps").in_scope(|| {});
        }
    }

    /// A slot's writer ended: kept in the list, and each slot past the bound removed.
    fn slot_done(&mut self, w: &mut World<'_, S>, writing: slot::Writing, outcome: Result<slot::Written, slot::Unslotted>) {
        let written = match outcome {
            Ok(written) => written,
            Err(why) => {
                tracing::info_span!(target: "blaze", "request slot: not written", why = %why).in_scope(|| {});
                return;
            }
        };
        let (_, _, records) = writing.counts();
        tracing::info_span!(target: "blaze", "request slot: kept", records, wrote = written.wrote).in_scope(|| {});
        let listed = self.certify.listed.get_or_insert_with(Vec::new);
        let kept = slot::Listed {
            key: written.key,
            written_ns: writing.written_ns(),
            body: Some(written.body),
            programs: writing.programs_held().to_vec(), // alloc: once a slot, a path a program
            request: writing.request().to_vec(),       // alloc: the request's frame, once a slot
        };
        for gone in slot::keep(listed, kept) {
            self.slot_remove(w, gone);
        }
    }

    fn slot_remove(&mut self, w: &mut World<'_, S>, key: ContentId) {
        let path = format!("{}/{key:x}", self.setup.slots); // alloc: a path, once a slot removed
        let Some(file) = FileName::new(path.trim_start_matches('/')) else { return };
        if let Ok(tag) = self.own_tag(Own::Slot { slot: REMOVAL }) {
            w.effects.push(Effect::Remove { file, tag });
        }
    }

    /// The driver's completion of a slot's effect (`Own::Slot`, H8).
    pub(super) fn slot_told(&mut self, w: &mut World<'_, S>, n: u32, done: Done) {
        match (n, done) {
            (LISTING, Done::Listed(entries)) => {
                let listed = self.certify.listed.get_or_insert_with(Vec::new);
                for (name, meta) in entries {
                    let Some(key) = slot::key_named(name.as_str()) else { continue };
                    if listed.iter().all(|it| it.key != key) {
                        // alloc: once a slot listed
                        listed.push(slot::Listed { key, written_ns: meta.modified_ns, body: None, programs: Vec::new(), request: Vec::new() });
                    }
                }
                let mut gone = Vec::new(); // alloc: none unless past the bound
                while listed.len() > slot::MOST_SLOTS {
                    let Some(oldest) = listed.iter().enumerate().min_by_key(|(_, it)| (it.written_ns, it.key)).map(|(at, _)| at) else { break };
                    gone.push(listed.swap_remove(oldest).key);
                }
                for key in gone {
                    self.slot_remove(w, key);
                }
            }
            // No directory yet: no slot was ever written.
            (LISTING, _) => {}
            (REMOVAL, Done::Failed(error)) => tracing::warn!(target: "blaze", "request slot: a slot past the bound was not removed: {error}"),
            (REMOVAL, _) => {}
            (n, done) => {
                let Some(at) = self.certify.slots.iter().position(|it| it.0 == n) else { return };
                let seen = match (self.certify.slots[at].2, done) {
                    (Waits::Stamps { asked_ns }, Done::Stamped(stamps)) => slot::Seen::Stamps { stamps, asked_ns },
                    (Waits::Read, Done::Read(bytes)) => slot::Seen::Read(Ok(bytes)),
                    (Waits::Read, Done::Failed(error)) => slot::Seen::Read(Err(error.to_string())),
                    (Waits::Write, Done::Written) => slot::Seen::Written(Ok(())),
                    (_, Done::Failed(error)) => slot::Seen::Written(Err(error.to_string())),
                    (waits, done) => slot::Seen::Written(Err(format!("{waits:?} was answered with {done:?}"))), // alloc: a reason
                };
                self.slot_step(w, n, Some(seen));
            }
        }
    }
}

#[cfg(test)]
mod marks_tests {
    use blaze_core::{FileName, S_IFREG};

    use super::*;

    const CHANGED: u64 = 1_000 * 1_000_000_000;

    fn stamp(ino: u64) -> Stamp {
        Stamp { ino, size: 9, mtime_ns: CHANGED, ctime_ns: CHANGED, mode: S_IFREG | 0o755 }
    }

    fn output(path: &str) -> certificate::Output {
        // alloc: a test's output
        certificate::Output { base: ABSOLUTE, path: path.to_owned(), content: ContentId::of("certify marks test", path.as_bytes()) }
    }

    /// One answer of `outputs` (sorted) given to the evidence of `shape`, each kept steady,
    /// and its changes written as marks, as a hit or a write does.
    fn answer(held: &mut Vec<(u32, ContentId, Evidence)>, shape: u8, outputs: &[certificate::Output], vfs: &mut Vfs) {
        let at = evidence_at(held, 0, ContentId([shape; 32]), vfs);
        let evidence = &mut held[at].2;
        evidence.only(outputs);
        for (ino, it) in (1..).zip(outputs) {
            evidence.keep(it.base, &it.path, stamp(ino), it.content, CHANGED + 4 * blaze_vfs::COARSE_NS);
        }
        mark_changed(held, at, vfs);
    }

    fn marks(vfs: &Vfs) -> Vec<String> {
        vfs.marks(local::PLACED).map(|(name, _)| name[local::PLACED.len()..].to_owned()).collect()
    }

    /// The marks after `builds` builds of one shape, each of which places the program at the
    /// same path and one output at a path of its own (the old path is an output no more).
    fn after(builds: u32) -> (Vec<String>, usize) {
        let mut vfs = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]).0;
        let mut held = Vec::new();
        let mut outputs = Vec::new();
        for build in 0..builds {
            outputs = vec![output("/w/target/debug/app"), output(&format!("/w/target/debug/deps/app-{build:04}"))];
            outputs.sort_by(|a, b| (a.base, &a.path).cmp(&(b.base, &b.path)));
            answer(&mut held, 1, &outputs, &mut vfs);
        }
        (marks(&vfs), outputs.len())
    }

    /// The invariant: the placed outputs' marks number at most the outputs the evidence held
    /// keeps, whatever the number of builds: a path that is no output of the last answer loses
    /// its mark, unless another evidence held keeps it; an evidence dropped past
    /// [`MOST_EVIDENCE`] takes the marks only it kept. Control: with no removal (the planted
    /// `KEEPS_MARKS`), the marks grow with the builds and the oracle catches it.
    #[test]
    fn the_placed_marks_are_bounded_by_the_outputs_held() {
        for builds in [1, 2, 10, 100] {
            let (marks, outputs) = after(builds);
            assert!(marks.len() <= outputs, "after {builds} builds, {} marks for {outputs} outputs: {marks:?}", marks.len());
            assert!(marks.contains(&format!("/w/target/debug/deps/app-{:04}", builds - 1)), "{marks:?}");
        }
        KEEPS_MARKS.set(true);
        let (planted, outputs) = after(100);
        KEEPS_MARKS.set(false);
        assert!(planted.len() > outputs, "the control: with no removal, {} marks after 100 builds", planted.len());

        // A path another evidence held keeps keeps its mark when one shape drops it.
        let mut vfs = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]).0;
        let mut held = Vec::new();
        let shared = [output("/w/target/debug/shared")];
        answer(&mut held, 1, &shared, &mut vfs);
        answer(&mut held, 2, &shared, &mut vfs);
        answer(&mut held, 1, &[output("/w/target/debug/one")], &mut vfs);
        assert_eq!(marks(&vfs), ["/w/target/debug/one", "/w/target/debug/shared"], "shape 2 keeps the shared path");

        // Past the most held, the evidence dropped takes the marks only it kept.
        let mut vfs = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]).0;
        let mut held = Vec::new();
        for shape in 0..=u8::try_from(MOST_EVIDENCE).unwrap() {
            answer(&mut held, shape, &[output(&format!("/w/out/{shape:03}"))], &mut vfs);
        }
        let marks = marks(&vfs);
        assert_eq!(marks.len(), MOST_EVIDENCE, "{marks:?}");
        assert!(!marks.contains(&"/w/out/000".to_owned()), "the oldest shape's mark went with its evidence");
    }
}

#[cfg(test)]
mod tests;
