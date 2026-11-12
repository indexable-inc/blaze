//! The request machine's driver (docs/v2/plan/design/certificate.md): the thin IO side of
//! [`request::Request`], on the daemon's loop.
//!
//! A build or a check is checked against its certificate before it is a job
//! ([`Jobs::certify_begin`]): the machine's asks are answered here (a node from the request's
//! pinned view, a variable from the request's environment, a file or a listing outside the
//! views through [`blaze_usercode::inputs::Disk`], an output's stamp and bytes through the
//! loop's driver: [`Effect::Stat`], [`Effect::ReadAll`]). A hit says what the build said again
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

use super::certificate::{self, Evidence, Read, Stamp};
use super::request::{self, Ask, Next, Told, View};
use super::said;
use super::*;
use crate::serve::work::{Looked, Seen};

/// The most (checkout, shape) pairs evidence is held of; past it the oldest goes. Evidence is
/// never a condition: without it a check hashes each output once.
const MOST_EVIDENCE: usize = 64;

/// The most states one call steps a machine through before it stops, named: a check has
/// five (find, value, stamp, hash, stamp again), a write two.
const MOST_ROUNDS: u32 = 16;

/// The one base a certificate's outputs are under: the file system's root. Every path is
/// absolute until the shape is relocated (certificate.md, "State").
const ABSOLUTE: u8 = 0;

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
    /// [`blaze_vfs::outside::looks`] as read before it looked at anything.
    looks: u64,
    /// The variables its steps asked for by name ([`Building::steps_read`]); or why they are
    /// not known.
    named: Result<Vec<String>, String>,
    /// The files and the listings its steps read by path outside the views; or why they are
    /// not known.
    read: Result<(Vec<std::path::PathBuf>, Vec<std::path::PathBuf>), String>,
    /// Every read of the machine's files its traced steps' records rest on, each with what
    /// was there when the record held (`Writer::read_seen`); or the step that has no trace.
    seen: Result<Vec<Seen>, String>,
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
        let Building { root, shape, inputs, saying, looks, named, read, seen, .. } = self;
        // Its looks at the disk are taken whatever its outcome: the next build's are its own.
        let files = blaze_vfs::outside::take_file_looks();
        drop(blaze_vfs::outside::take_named_looks());
        let looked_again = blaze_vfs::outside::looks() != looks;
        if !passed {
            // alloc: a reason, once a build
            return Ended { root, shape, built: Err(request::Unwritten::Failed.to_string()) };
        }
        // A file it looked at on the disk that no watch vouched for is a read of its own, at
        // the hash that look saw. A look of another kind is named by no record.
        let looked = match (looked_again, files) {
            (false, _) => Ok(Vec::new()), // alloc: none
            (true, Some(files)) => Ok(files),
            // alloc: a reason, once a build
            (true, None) => Err(match blaze_vfs::outside::last_look() {
                Some(path) => format!("a look at the disk outside every view that is of no one file, the last at {}", path.display()),
                None => "a look at the disk outside every view that is of no one file".to_owned(),
            }),
        };
        let built = match saying.end() {
            Ok(said) => Ok(Walked { inputs, said, named, read, seen, looked }),
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
    /// The files it looked at on the disk itself, each with what was there; or the look
    /// that is of no file.
    looked: Result<Vec<(std::path::PathBuf, Option<[u8; 32]>)>, String>,
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
}

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
        }
    }
}

/// The evidence of checkout `root` and shape `shape`, made empty where none is held.
fn evidence_of(
    held: &mut Vec<(u32, ContentId, Evidence)>,
    root: u32,
    shape: ContentId,
) -> &mut Evidence {
    let at = match held.iter().position(|it| it.0 == root && it.1 == shape) {
        Some(at) => at,
        None => {
            if held.len() >= MOST_EVIDENCE {
                held.remove(0);
            }
            held.push((root, shape, Evidence::default()));
            held.len() - 1
        }
    };
    &mut held[at].2
}

/// The blaze that serves, by content: the BLAKE3 hash of the executable running now
/// ([`blaze_profile::identity::binary_hash`], the one home of it: what `builds.blaze` names
/// each build's blaze by). Never its version: a blaze built again under the same version is
/// another program, and what the old one answered is not its answer (rule 21). Hashed once
/// a process (the daemon's history has it hashed before the first request); `held` keeps it
/// for the loop. A blaze that cannot read itself certifies nothing and says so.
fn tool_of(held: &mut Option<[u8; 32]>) -> Result<[u8; 32], String> {
    if let Some(tool) = held {
        return Ok(*tool);
    }
    let _hashing = tracing::info_span!(target: "blaze", "request certificate: the hash of this blaze's executable").entered();
    let tool = blaze_profile::identity::binary_hash()
        // alloc: an error, once
        .map_err(|error| format!("the request certificate is of the blaze that serves it, by the hash of its executable, which could not be read: {error}"))?;
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

/// The digest of the variable `name` of `env`.
fn env_digest(env: &[(String, String)], name: &str) -> Digest {
    match blaze_units::query::env_var(env, name) {
        Some(value) => Digest::Is(
            Hasher::new("blaze v2 serve request certificate variable").part(value.as_bytes()).finish(),
        ),
        None => Digest::Nothing,
    }
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
fn outside_now(read: &Read) -> Digest {
    use blaze_usercode::trace::Inputs as _;
    let (path, how) = match read {
        Read::Outside(path) => (path, Looked::File),
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
    let disk = blaze_usercode::inputs::Disk::new(Vec::new()); // alloc: none, no variable is asked of it
    let mut now = disk.now();
    let at = std::path::Path::new(path);
    // The question the step's record asked ([`Looked`]), asked again of the same table.
    let found = match how {
        Looked::File => now.file(at),
        Looked::Listing => now.dir(at),
        Looked::Tree { package } => now.tree(at, package),
    };
    outside_digest(found.map_err(|_| ()), path)
}

/// The read a traced step's look `how` at `path` is in a certificate.
fn read_of(how: Looked, path: String) -> Read {
    match how {
        Looked::File => Read::Outside(path),
        Looked::Listing => Read::Listing(path),
        Looked::Tree { package: false } => Read::Tree(path),
        Looked::Tree { package: true } => Read::Package(path),
    }
}

/// Nanoseconds since the epoch now, by the clock files' change times are of.
pub(super) fn wall_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|it| u64::try_from(it.as_nanos()).ok())
        .unwrap_or(0)
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
    let traced = blaze_units::query::env_var(&env, crate::cli::client_rows::PROFILE)
        .is_some_and(|it| !it.is_empty());
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
                let evidence = evidence_of(evidence, checking.root, checking.shape);
                let next = checking.machine.step(&records, evidence);
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
        for (n, ask) in asks.iter().enumerate() {
            let told = match ask {
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
                        Read::Outside(_) | Read::Listing(_) | Read::Tree(_) | Read::Package(_) => {
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
                            w.effects.push(Effect::Stat { file, tag });
                            None
                        }
                        (Some(file), Some(tag), _) => {
                            w.effects.push(Effect::ReadAll { file, tag });
                            None
                        }
                        // No file the driver can name, or no tag left for a look: nothing is
                        // seen there, the request misses on the output and its build places it.
                        (_, _, Ask::Stamp { .. }) => Some(Told::Stamp { stamp: None, looked_ns: wall_ns() }),
                        _ => Some(Told::Content(None)),
                    }
                }
            };
            let checking = &mut self.certify.checking[at];
            match told {
                Some(told) => {
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
            (Ask::Stamp { .. }, Done::Stat(meta)) => Ok(Told::Stamp {
                stamp: meta.filter(|it| !it.dir).map(|it| Stamp { len: it.len, changed_ns: it.modified_ns }),
                looked_ns: wall_ns(),
            }),
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
        let Checking { id, span, root, late, .. } = checking;
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
    pub(super) fn certify_building(&mut self, id: JobId, root: u32, shape: ContentId, inputs: Inputs) {
        self.certify.building.retain(|it| it.job != id);
        self.certify.building.push(Building {
            job: id,
            root,
            shape,
            inputs,
            saying: said::Saying::new(),
            looks: blaze_vfs::outside::looks(),
            // alloc: a reason, once a build
            named: Err("its steps were not asked which variables they read by name".to_owned()),
            // alloc: as above
            read: Err("its steps were not asked which paths they read".to_owned()),
            // alloc: as above
            seen: Err("its steps were not asked what their records read".to_owned()),
        });
        // The path of each look at the disk is kept while a build is heard.
        blaze_vfs::outside::name_looks(true);
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
                blaze_vfs::outside::name_looks(!self.certify.building.is_empty());
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
        // What the build itself looked at outside the views, at the hash it saw.
        match &walked.looked {
            Ok(looked) => {
                for (path, hash) in looked {
                    let Some(path) = path.to_str() else {
                        return Err(format!("no request certificate is written: {} is no UTF-8 path", path.display()));
                    };
                    valued.push((Read::Outside(path.to_owned()), outside_digest(Ok(*hash), path))); // alloc: a name
                }
            }
            Err(why) => untraced.push(why.clone()), // clone: a reason, once a build
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
        // with the stamp its placement's look took and when that look was asked for. The
        // machine keeps the stamp only where it is steady (`RACY_NS` past the change): an
        // output this build wrote is hashed once by a later check, one it found placed is a
        // stat from the first check on.
        let mut outputs = Vec::new(); // alloc: once a build, an entry an output
        let Some(plan) = self.roots.plans[root as usize].iter().find(|it| it.shape == shape) else {
            return Err("no request certificate is written: the checkout keeps no plan of the request's shape to name its outputs by".to_owned());
        };
        for path in plan.units.plans.iter().flat_map(|it| it.outputs.iter()) {
            let placed = self.paths.ids.get(path.as_str()).and_then(|id| self.paths.placed[*id as usize].as_ref());
            match placed {
                Some(Placement { key, meta: Some(meta), asked_ns, .. }) => outputs.push((
                    // clone: an output's own path, once a build
                    certificate::Output { base: ABSOLUTE, path: path.clone(), content: key.0 },
                    Some((Stamp { len: meta.len, changed_ns: meta.modified_ns }, *asked_ns)),
                )),
                // A placement in flight: what the file holds is not known yet.
                Some(_) => return Err(format!("no request certificate is written: the output {path} is still being placed")),
                // A planned path the build writes no file at (debug info beside an image).
                None => {}
            }
        }
        let said = blaze_protocol::cbor::to_vec(&walked.said)
            .map_err(|error| format!("no request certificate is written: what the build said does not encode: {error}"))?;
        let tool = tool_of(&mut self.certify.tool).map_err(|why| format!("no request certificate is written: {why}"))?;
        let base = base_of(shape, walked.inputs.config_id, walked.inputs.request, &tool);
        let mut machine = request::Request::building(base, unasked_of(env));
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
                let next = machine.step(&records, evidence_of(evidence, root, shape));
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
