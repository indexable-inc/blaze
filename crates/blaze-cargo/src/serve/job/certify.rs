//! The request machine's driver (docs/v2/plan/design/certificate.md): the thin IO side of
//! [`request::Request`], on the daemon's loop.
//!
//! A build or a check is checked against its certificate before it is a job
//! ([`Jobs::certify_begin`]): the machine's asks are answered here (a node from the request's
//! pinned view, a variable from the request's environment, a file or a listing outside the
//! views through [`blaze_usercode::inputs::Disk`], an output's stamp and bytes through the
//! loop's driver: [`Effect::Stat`], [`Effect::ReadAll`]). A hit says what the build said again
//! and finishes with no job; a miss goes on into the job ([`Jobs::begin_job`]). A build that
//! made nothing hands the machine what it read at its end ([`Jobs::certify_written`]) and the
//! record is written.
//!
//! What this driver does not do yet, each named in certificate.md, "State": the shape is not
//! relocated (the base holds the checkout's directory, an output's path is absolute, base 0);
//! the one node is the view's root; a build that compiled writes none.

use std::{cell::RefCell, sync::Arc};

use blaze_record::Digest;

use super::certificate::{self, Evidence, Read, Stamp};
use super::request::{self, Ask, Next, Told, View};
use super::*;

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

/// A request whose check is under way: no job yet.
struct Checking {
    id: JobId,
    view: Opened,
    work: Work,
    idle: bool,
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

/// A build that ended and may have left a certificate, as [`Jobs::keep_answer`] saw it: kept
/// until the loop has the store in hand ([`Jobs::certify_written`]).
pub(super) struct Ended {
    root: u32,
    shape: ContentId,
    /// What it hands the machine, or why it leaves no certificate.
    built: Result<Walked, String>,
}

/// What a build that made nothing read and said.
struct Walked {
    /// The root of the view it read, its own writes in it: an id of content, never the
    /// checkout's head.
    view: ContentId,
    config: Arc<BuildConfig>,
    config_id: ContentId,
    request: ContentId,
    drawn: ContentId,
    responses: Arc<[Response]>,
    worked: Option<Worked>,
    named: Result<Arc<[String]>, String>,
    read: Result<(Arc<[std::path::PathBuf]>, Arc<[std::path::PathBuf]>), String>,
    looked: Arc<[(std::path::PathBuf, Option<[u8; 32]>)]>,
}

impl Ended {
    /// Of a build of checkout `root` and request shape `shape` whose walk ended as `last`.
    pub(super) fn of(root: u32, shape: ContentId, last: &Last) -> Ended {
        let built = match last {
            Ok(answer) => Ok(Walked {
                view: answer.inputs.view,
                config: Arc::clone(&answer.inputs.config), // clone: a refcount
                config_id: answer.inputs.config_id,
                request: answer.inputs.request,
                drawn: answer.inputs.drawn,
                responses: Arc::clone(&answer.responses), // clone: a refcount
                worked: answer.worked.clone(),            // clone: one `Work`, once a build
                named: answer.named.clone(),              // clone: a refcount or a reason
                read: answer.read.clone(),                // clone: refcounts or a reason
                looked: Arc::clone(&answer.looked),       // clone: a refcount
            }),
            Err(why) => Err(why.to_string()), // alloc: a reason, once a build
        };
        Ended { root, shape, built }
    }
}

/// What the jobs hold for the request machines.
pub(super) struct Certify {
    /// The searches of the certificates' read-lists, counted.
    tally: blaze_record::Tally,
    checking: Vec<Checking>,
    /// One a checkout and request shape, oldest first, at most [`MOST_EVIDENCE`].
    evidence: Vec<(u32, ContentId, Evidence)>,
    pub(super) ended: Vec<Ended>,
}

impl Certify {
    pub(super) fn new() -> Certify {
        Certify {
            tally: blaze_record::Tally::default(),
            checking: Vec::new(), // alloc: none until a request is checked
            evidence: Vec::new(), // alloc: none until a request is checked
            ended: Vec::new(),    // alloc: none until a build ends
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

/// The base of the certificates of a request of plan shape `plan`, config `config` (less its
/// environment) and request `request`, served by this blaze.
fn base_of(plan: ContentId, config: ContentId, request: ContentId) -> ContentId {
    let release = crate::release::get();
    // alloc: the tool's identity, once a request
    let tool = [release.version.as_bytes(), b"\0", release.line.as_bytes()].concat();
    certificate::base(&certificate::Shape { plan, config, request }, &tool)
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
    let (path, listing) = match read {
        Read::Outside(path) => (path, false),
        Read::Listing(path) => (path, true),
        _ => return Digest::Nothing,
    };
    let _look = tracing::info_span!(
        target: "blaze",
        "request certificate: a look at a path outside the views",
        function = path.as_str(),
        listing,
    )
    .entered();
    let disk = blaze_usercode::inputs::Disk::new(Vec::new()); // alloc: none, no variable is asked of it
    let mut now = disk.now();
    let at = std::path::Path::new(path);
    let found = if listing { now.dir(at) } else { now.file(at) };
    outside_digest(found.map_err(|_| ()), path)
}

/// Nanoseconds since the epoch now, by the clock files' change times are of.
fn wall_ns() -> u64 {
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
        let (Some(opened), Some(request)) = (view.as_ref(), request_id(&work)) else {
            return Some((view, work, None));
        };
        // The standard library is mounted by a daemon's first build: the config, and so the
        // base, names where. That build is not checked; it leaves a certificate.
        // clone: the config owns the library's place, once a request
        let Some(library) = self.library.clone() else {
            // alloc: a reason, once a daemon's first build
            self.refused = Some("the standard library is not mounted yet: this is the daemon's first build".to_owned());
            return Some((view, work, None));
        };
        // A path dependency outside the view that no watch vouches for is read again by the
        // build before its config takes its mount: until then its content id is not known.
        let stale = w.mounts.stale_locals(vouched_local, super::super::fetch::local::vouched_file, super::super::fetch::local::absent);
        if let Some(at) = stale.first() {
            // alloc: a reason, once a request
            self.refused = Some(format!(
                "a path outside the view is read again before it can be vouched for: {at} ({}); {} such in all",
                blaze_vfs::outside::why_looked(std::path::Path::new(at)),
                stale.len(),
            ));
            return Some((view, work, None));
        }
        let span = tracing::info_span!(target: "blaze", "request certificate", job = %id);
        let keying = span.in_scope(|| {
            tracing::info_span!(target: "blaze", "request certificate: the request's shape").entered()
        });
        let Viewed { root, config, bare, shape, .. } =
            Self::viewed(&self.setup, &mut self.roots, w, &work, opened, library);
        let Some(root) = root else {
            drop(keying);
            // alloc: a reason, once a request on an agent's view
            self.refused = Some("its view is no checkout's: an agent's view has no certificate".to_owned());
            return Some((view, work, None));
        };
        let env = config.invocation.env;
        let traced = blaze_units::query::env_var(&env, crate::cli::client_rows::PROFILE)
            .is_some_and(|it| !it.is_empty());
        let base = base_of(shape, bare, request);
        let machine = request::Request::new(base, traced, unasked_of(&env));
        drop(keying);
        let (now, drawn) = (opened.view.root, drawn_id(&work));
        let Some(view) = view else { return None };
        self.certify.checking.push(Checking {
            id,
            view,
            work,
            idle,
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
        });
        self.certify_step(w, id);
        None
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
                        Read::Outside(_) | Read::Listing(_) => checking.span.in_scope(|| outside_now(read)),
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
        let Checking { id, span, root, .. } = checking;
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
                w.said.push(Said::Finish(id, true, None, None));
            }
            Err(error) => {
                // alloc: an error, once
                let what = format!("the request certificate's answer does not decode as what a build says: {error}");
                w.said.push(Said::Finish(id, false, Some(ApiError::new(what, None)), None));
            }
        }
    }

    /// The request is not certified: why is said in a span of the check and again in its
    /// build ([`Jobs::refused`]), and it becomes a job.
    fn certify_miss(&mut self, w: &mut World<'_, S>, checking: Checking, miss: request::Miss) {
        let Checking { id, view, work, idle, span, root, base, .. } = checking;
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
        self.refused = Some(why);
        self.begin_job(w, id, Some(view), work, idle, walks);
    }

    /// The check stopped with no outcome (the store, a look, the machine out of turn): the
    /// request fails with what stopped it. Never a miss: nothing is built in its place.
    fn certify_stopped(&mut self, w: &mut World<'_, S>, checking: Checking, what: String) {
        let Checking { id, span, .. } = checking;
        span.in_scope(|| tracing::error!(target: "blaze", job = %id, "{what}"));
        w.said.push(Said::Finish(id, false, Some(ApiError::new(what, None)), None));
    }

    /// Job `id` was cancelled: a check of it under way is dropped and the job finishes as a
    /// cancelled one does. Its looks in flight complete to nothing.
    pub(super) fn certify_cancel(&mut self, w: &mut World<'_, S>, id: JobId) {
        if let Some(at) = self.certify.checking.iter().position(|it| it.id == id) {
            self.certify.checking.swap_remove(at);
            w.said.push(Said::Finish(id, false, None, None));
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
        let env = &walked.config.invocation.env;
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
        // What the walk itself looked at outside the views, at the hash it saw.
        for (path, hash) in walked.looked.iter() {
            let Some(path) = path.to_str() else {
                return Err(format!("no request certificate is written: {} is no UTF-8 path", path.display()));
            };
            valued.push((Read::Outside(path.to_owned()), outside_digest(Ok(*hash), path))); // alloc: a name
        }
        // What its steps' records read under its outputs: valued by a look now, at the
        // walk's end (certificate.md, "State": the records' own hashes are not handed over).
        match &walked.read {
            Ok((files, dirs)) => {
                let listed = dirs.iter().map(|it| (it, true));
                for (path, listing) in files.iter().map(|it| (it, false)).chain(listed) {
                    let Some(path) = path.to_str() else {
                        return Err(format!("no request certificate is written: {} is no UTF-8 path", path.display()));
                    };
                    let read = if listing { Read::Listing(path.to_owned()) } else { Read::Outside(path.to_owned()) }; // alloc: a name
                    if valued.iter().any(|(it, _)| *it == read) {
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
        if walked.responses.iter().any(|it| matches!(it, Response::Diagnostic(_))) {
            valued.push((Read::Drawn, Digest::Is(walked.drawn)));
        }
        // The outputs of the shape's plan as they are placed: each by the key of its bytes.
        // No stamp is handed over: when the placement's look was is not kept, so the first
        // check hashes each once and its own looks vouch from then on.
        let mut outputs = Vec::new(); // alloc: once a build, an entry an output
        let Some(plan) = self.roots.plans[root as usize].iter().find(|it| it.shape == shape) else {
            return Err("no request certificate is written: the checkout keeps no plan of the request's shape to name its outputs by".to_owned());
        };
        for path in plan.units.plans.iter().flat_map(|it| it.outputs.iter()) {
            let placed = self.paths.ids.get(path.as_str()).and_then(|id| self.paths.placed[*id as usize].as_ref());
            match placed {
                Some(Placement { key, meta: Some(_), .. }) => outputs.push((
                    // clone: an output's own path, once a build
                    certificate::Output { base: ABSOLUTE, path: path.clone(), content: key.0 },
                    None,
                )),
                // A placement in flight: what the file holds is not known yet.
                Some(_) => return Err(format!("no request certificate is written: the output {path} is still being placed")),
                // A planned path the build writes no file at (debug info beside an image).
                None => {}
            }
        }
        let mut said: Vec<Response> = walked.responses.to_vec(); // alloc: once a build
        if let Some(worked) = walked.worked {
            // No build stands behind an answer said again: no id, no time of nodes.
            said.push(Response::Work(Worked {
                why: None,
                build_id: None,
                steps_us: Vec::new(), // alloc: none
                critical_path_us: 0,
                placed: blaze_api::PlacedBytes::default(),
                ..worked
            }));
        }
        let said = blaze_protocol::cbor::to_vec(&said)
            .map_err(|error| format!("no request certificate is written: what the build said does not encode: {error}"))?;
        let base = base_of(shape, walked.config_id, walked.request);
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
                            Ask::Read { read: Read::Node(path), view: View::Built } if path.is_empty() => Digest::Is(walked.view),
                            Ask::Read { read: Read::Node(path), view: View::Built } => {
                                w.vfs.reader().dir_id(walked.view, path).map_or(Digest::Nothing, Digest::Is)
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
