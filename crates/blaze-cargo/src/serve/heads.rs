//! The CLI's checkouts as heads (docs/v2/plan.md section 6): a request that names
//! `ViewRef::Head(<absolute path>)` builds the checkout as saved.
//!
//! - The first request on a checkout imports it: a [`Watcher`] of its directory scans it into
//!   the head named by its path and keeps the head current. What a build writes and what is no
//!   source is not taken in: a `target/` at the root (cargo's, or a `--target-dir` named
//!   there; blaze's own build directories are under blaze's root, never in the checkout) and
//!   every `.git`.
//! - The request that starts a checkout's watch asks no flush: it waits for the watcher's own
//!   look (its start sweeps the column, [`Watcher::start`]), and is handed on once that look
//!   has committed. Sound because the look's every stamp is asked after the request arrived
//!   (the watch starts inside [`Heads::arrive`]) and the watch is asked before the look in the
//!   same effects: a change made before the request is seen by the look, one made after the
//!   watch began is told by it. A request that arrives while that look runs flushes as below.
//! - A request on a checkout already watched waits until the watcher has seen every change
//!   made before it: the daemon asks the driver to flush the checkout's watch
//!   ([`Effect::Flush`]) and holds the request until the flush is told and the watcher has
//!   committed everything it was told before it. Only then does the session see it. Nothing
//!   is written into the checkout for
//!   it: FSEvents' flush writes its cookie in a directory of its own outside the checkout,
//!   io_uring's drains the watch's queue, and IOCP's marks where the volume's USN journal ends
//!   as the flush is asked and is told once the watch's reads of the journal have told every
//!   record before it (blaze-core's `journal::barrier`). Where the driver can't flush (a
//!   checkout on a network share or a volume with no USN journal, whose watch can't start at
//!   all), the flush fails and so does every request on the checkout, naming the checkout and
//!   why.
//! - A checkout whose directory or a changed file of it can't be read, or whose watch ended,
//!   fails the requests waiting on it, naming it, and its watch stops: the head never keeps a
//!   file's old bytes as current. The next request watches it again.
//! - A request that runs user code (a build, check, test or run) starts a round of the watches
//!   of directories outside views ([`Outside`]) and is held until the round is in, so what its
//!   checks of user code's inputs trust was heard before it ([`blaze_vfs::unchanged_since`]).
//!   Until a round is in those checks stat everything: the hold is for speed, not soundness.
//!
//! A checkout is a slot, its watcher's tags `watchers + slot * 2^32`, so a completion finds its
//! checkout by one shift; a head's name is looked up once, as a request arrives. A batch of
//! changes costs one pass over its files, filtered in place, nothing allocated per file.

#![forbid(unsafe_code)]

use blaze_api::{ApiError, Arrived, Reply, Request, RequestRef, Response, ViewRef};
use blaze_core::{Completion, Conn, Done, Effect, FileName, Tag};
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_vfs::{Outside, Vfs, Watched, Watcher};
use std::alloc::Global;

/// How many tags one watcher uses ([`Watcher::new`]).
const WATCHER_TAGS: u64 = 1 << 32;

/// What [`Heads`] makes of an arrival or a completion.
#[derive(Debug, PartialEq)]
pub enum Out {
    /// For the session.
    Pass(Arrived),
    /// An answer the daemon sends itself, the session never having seen the request.
    Answer(Reply),
    /// A checkout's watcher committed what the disk changed: `head` is at `root` now.
    Moved { head: String, root: ContentId },
}

/// A request whose checkouts' watchers, or the watches outside views, have not caught up with
/// it.
struct Held {
    from: RequestRef,
    request: Request,
    /// Each checkout it names, by slot, and the flush it waits for there.
    waits: Vec<(u32, u64)>,
    /// It runs user code: it waits for the round of the watches outside views.
    outside: bool,
    /// The span of its wait (every-wait-says-what), closed as it is handed on, fails or is
    /// cancelled: how long the watches took to hear every change made before the request.
    /// The wait ends with each flush's completion, which every driver tells, done or failed.
    _waiting: tracing::Span,
}

struct Checkout {
    head: String,
    watcher: Watcher,
    /// Its directory under the driver's root, then `/`: what its files' paths start with.
    below: String,
    /// The tag of its watcher's watch, which a flush names.
    watch: Tag,
    /// Flushes asked of its watch, by number: whether each has been told. Dropped once told
    /// and no held request waits for it ([`Heads::sweep`]). The wait of the request that
    /// started the watch is one told as it is made: no flush is asked for it, the watcher's
    /// own look stands for it.
    flushes: HashMap<u64, bool, Global>,
    /// The number of the wait that stands for the watcher's own look, for the plant.
    #[cfg(test)]
    first: u64,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// The request that starts a watch is handed on before the watcher's look commits.
    PassesBeforeTheLook,
}

#[cfg(test)]
thread_local! {
    /// The planted bug of the first look's control (`serve::tests::first_look`).
    static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

/// Plants [`Plant::PassesBeforeTheLook`] on this thread, or takes it away: for the
/// simulation's control.
#[cfg(test)]
pub(super) fn plant_passes_before_the_look(on: bool) {
    PLANT.set(if on { Plant::PassesBeforeTheLook } else { Plant::None });
}

pub struct Heads {
    /// The flushes of checkouts' watches: `tags..tags + 2^32`.
    tags: u64,
    /// The first tag of slot 0's watcher.
    watchers: u64,
    /// The last tag of every watcher's range.
    watcher_end: u64,
    next_flush: u64,
    /// By slot; a slot whose watch ended stays empty, its tags never used again.
    checkouts: Vec<Option<Box<Checkout>>>,
    /// The slot of each checkout watched, by its head.
    slots: HashMap<String, u32, Global>,
    /// By flush tag: the checkout's slot and the flush's number.
    flushing: HashMap<Tag, (u32, u64), Global>,
    held: Vec<Held>,
    /// Kept for the daemon's life: dropping it leaves every directory unconfirmed.
    outside: Outside<'static>,
}

/// Whether a head is a checkout's: one the CLI names by the checkout's absolute path.
pub fn is_checkout(head: &str) -> bool {
    head.starts_with('/')
}

/// The checkout heads `view` names.
fn view_heads<'a>(view: &'a ViewRef, out: &mut Vec<&'a str>) {
    match view {
        ViewRef::Head(head) if is_checkout(head) => out.push(head),
        ViewRef::Head(_) | ViewRef::Root(_) => {}
        ViewRef::WhatIf { base, .. } => view_heads(base, out),
    }
}

/// The checkout heads `request` reads. A commit names a head to move, not to read: it waits for
/// nothing.
pub fn named(request: &Request) -> Vec<&str> {
    let mut heads = Vec::new();
    match request {
        Request::Check { view, .. }
        | Request::Build { view, .. }
        | Request::Test { view, .. }
        | Request::Run { view, .. }
        | Request::Why { view, .. }
        | Request::Open { view }
        | Request::Verify { view, .. }
        | Request::CodegenInputs { view, .. }
        | Request::Trace { view, .. }
        | Request::Tables { view }
        | Request::Doc { view, .. }
        | Request::Lock { view, .. }
        | Request::WillTest { view, .. } => view_heads(view, &mut heads),
        Request::WillSave { head, .. } if is_checkout(head) => heads.push(head),
        Request::Diff { from, to } | Request::Reach { from, to, .. } => {
            view_heads(from, &mut heads);
            view_heads(to, &mut heads);
        }
        Request::BestOf { base, .. } => view_heads(base, &mut heads),
        _ => {}
    }
    heads.sort_unstable();
    heads.dedup();
    heads
}

/// Whether `request` runs user code (build scripts, proc macros, tests, a binary), whose checks
/// of their inputs outside views read the watches' round. A reach, an edit and a build ahead
/// build their views' proc macros and run their build scripts before they read them.
pub fn runs_user_code(request: &Request) -> bool {
    matches!(
        request,
        Request::Check { .. }
            | Request::Build { .. }
            | Request::Test { .. }
            | Request::Run { .. }
            | Request::BestOf { .. }
            | Request::Verify { .. }
            | Request::Doc { .. }
            | Request::Reach { .. }
            | Request::Edit { .. }
            | Request::WillTest { .. }
            | Request::WillSave { .. }
    )
}

/// Whether the file at `rel` of a checkout is out of its view: a `target/` at the root, which
/// cargo or a `--target-dir` there writes (blaze's default build directory is never in the
/// checkout), and every `.git`.
pub fn excluded(rel: &str) -> bool {
    rel.strip_prefix("target").is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        || (rel.contains(".git") && rel.split('/').any(|part| part == ".git"))
}

impl Checkout {
    /// Whether `file` is in the view: outside the checkout, or not [`excluded`].
    fn keeps(&self, file: &FileName) -> bool {
        file.as_str().strip_prefix(self.below.as_str()).is_none_or(|rel| !excluded(rel))
    }
}

impl Heads {
    /// Flushes take the tags `tags..tags + 2^32`; watchers take ranges of `2^32` from
    /// `watchers` on, up to `watchers_end`; `outside` its own ([`Outside::owns`]).
    pub fn new(tags: u64, watchers: u64, watchers_end: u64, outside: Outside<'static>) -> Heads {
        Heads {
            tags,
            watchers,
            watcher_end: watchers_end,
            next_flush: 0,
            checkouts: Vec::new(),
            slots: HashMap::new_in(Global),
            flushing: HashMap::new_in(Global),
            held: Vec::new(),
            outside,
        }
    }

    pub fn owns(&self, tag: Tag) -> bool {
        tag.0.wrapping_sub(self.tags) < 1 << 32
            || (tag.0 >= self.watchers && tag.0 < self.watcher_end)
            || self.outside.owns(tag)
    }

    /// The checkout's slot a watcher's tag is of.
    fn slot_of(&self, tag: Tag) -> Option<u32> {
        let slot = tag.0.checked_sub(self.watchers)? / WATCHER_TAGS;
        u32::try_from(slot).ok()
    }

    fn checkout(&self, slot: u32) -> Option<&Checkout> {
        self.checkouts.get(slot as usize)?.as_deref()
    }

    /// The checkout whose head is at `root`, if one is: where an agent's build of it is placed.
    pub fn path_of(&self, vfs: &Vfs, root: ContentId) -> Option<String> {
        self.slots.keys().find(|head| vfs.head(head) == Some(root)).cloned()
    }

    /// The checkouts watched now, by absolute path: the collector's live heads, whose build
    /// dirs it keeps (docs/gc.md).
    pub fn checkouts(&self) -> impl Iterator<Item = &str> + Clone {
        self.slots.keys().map(String::as_str).filter(|head| is_checkout(head))
    }

    /// How many requests wait for a watcher.
    pub fn waiting(&self) -> usize {
        self.held.len()
    }

    /// Takes what arrived: passed on, or held until the watchers of the checkouts it names
    /// have caught up with it.
    pub fn arrive(
        &mut self,
        vfs: &Vfs,
        wall_ns: u64,
        arrived: Arrived,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Out>,
    ) {
        // Each name a watcher saved no stamp of (taken within the racy window of its change)
        // and now steady is looked at again, before the request is held behind the watchers.
        for checkout in self.checkouts.iter_mut().flatten() {
            checkout.watcher.steady(vfs, wall_ns, effects);
        }
        let (from, request) = match arrived {
            Arrived::Gone { conn } => {
                self.held.retain(|held| held.from.conn != conn);
                out.push(Out::Pass(Arrived::Gone { conn }));
                return;
            }
            Arrived::Request { from, request } => (from, request),
        };
        if let Request::Cancel { id } = request
            && let Some(at) =
                self.held.iter().position(|it| it.from == RequestRef { conn: from.conn, id })
        {
            let held = self.held.remove(at);
            answer(out, held.from, None, false);
            answer(out, from, None, true);
            return;
        }
        if self.held.iter().any(|it| it.from == from) {
            let error = ApiError::new(
                format!("request {} is already under way on this connection", from.id),
                Some(format!("request {}", from.id)),
            );
            answer(out, from, Some(error), false);
            return;
        }
        let heads = named(&request);
        let outside = runs_user_code(&request);
        if heads.is_empty() && !outside {
            out.push(Out::Pass(Arrived::Request { from, request }));
            return;
        }
        let mut waits = Vec::with_capacity(heads.len());
        let mut first_looks = 0usize;
        for head in heads {
            match self.flush(vfs, head, wall_ns, effects) {
                Ok((slot, number, first)) => {
                    first_looks += usize::from(first);
                    waits.push((slot, number));
                }
                Err(error) => {
                    answer(out, from, Some(error), false);
                    return;
                }
            }
        }
        if outside {
            self.outside.flush(effects);
        }
        let _waiting = tracing::info_span!(
            target: "blaze",
            "the request waits for the watches to hear every change made before it",
            checkouts = waits.len(),
            first_looks,
            outside,
        );
        self.held.push(Held { from, request, waits, outside, _waiting });
        self.release(out);
    }

    /// A wait on `head`'s checkout's watch: the checkout's slot, the wait's number, and whether
    /// it started the watch. A checkout watched already is flushed; one not watched yet is
    /// watched, and its wait is the watcher's own look, told as it is made, with no flush.
    /// Nothing is written into the checkout.
    fn flush(
        &mut self,
        vfs: &Vfs,
        head: &str,
        wall_ns: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(u32, u64, bool), ApiError> {
        let (slot, first) = match self.slots.get(head) {
            Some(&slot) => (slot, false),
            None => (self.watch(vfs, head, wall_ns, effects)?, true),
        };
        let checkout =
            self.checkouts[slot as usize].as_deref_mut().expect("a slot named is watched");
        let number = self.next_flush;
        self.next_flush += 1;
        if first {
            #[cfg(test)]
            {
                checkout.first = number;
            }
            checkout.flushes.insert(number, true);
            return Ok((slot, number, true));
        }
        let tag = Tag(self.tags + number % (1 << 32));
        self.flushing.insert(tag, (slot, number));
        effects.push(Effect::Flush { watch: checkout.watch, tag });
        checkout.flushes.insert(number, false);
        Ok((slot, number, false))
    }

    /// Starts watching `head`'s checkout in a new slot.
    fn watch(
        &mut self,
        vfs: &Vfs,
        head: &str,
        wall_ns: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<u32, ApiError> {
        let path = head.trim_end_matches('/');
        let Some(dir) = FileName::new(path.trim_start_matches('/')) else {
            return Err(ApiError::new(
                format!("the head `{head}` names no directory a checkout can be in"),
                Some(head.to_owned()),
            ));
        };
        let slot = u32::try_from(self.checkouts.len()).expect("under 2^32 checkouts");
        let tags = self.watchers + u64::from(slot) * WATCHER_TAGS;
        assert!(tags < self.watcher_end, "the daemon has watched every checkout its tags allow");
        let mut watcher = Watcher::new(head, dir.clone(), tags);
        // The journal is asked before any flush of it, in the same effects: a flush names a
        // watch in flight.
        watcher.start(vfs, wall_ns, effects);
        let checkout = Checkout {
            head: head.to_owned(),
            watcher,
            below: format!("{dir}/"),
            // The tag of the watcher's journal: its first ([`Watcher::new`]).
            watch: Tag(tags),
            flushes: HashMap::new_in(Global),
            #[cfg(test)]
            first: u64::MAX,
        };
        self.checkouts.push(Some(Box::new(checkout)));
        self.slots.insert(head.to_owned(), slot);
        Ok(slot)
    }

    /// Takes a completion of its own; what was held and is now caught up goes on.
    pub fn complete(
        &mut self,
        vfs: &mut Vfs,
        wall_ns: u64,
        completion: Completion,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Out>,
    ) {
        if completion.tag.0.wrapping_sub(self.tags) < 1 << 32 {
            self.flushed(completion, out);
        } else if self.outside.owns(completion.tag) {
            self.outside.complete(completion, effects);
        } else {
            self.watched(vfs, wall_ns, completion, effects, out);
        }
        self.release(out);
    }

    fn flushed(&mut self, completion: Completion, out: &mut Vec<Out>) {
        let Some((slot, number)) = self.flushing.remove(&completion.tag) else { return };
        let Some(checkout) = self.checkouts.get_mut(slot as usize).and_then(Option::as_deref_mut)
        else {
            return;
        };
        match completion.done {
            Done::Flushed => {
                if let Some(told) = checkout.flushes.get_mut(&number) {
                    *told = true;
                }
            }
            done => {
                checkout.flushes.remove(&number);
                let why = match done {
                    Done::Failed(error) => error.to_string(),
                    other => format!("the driver answered {other:?}"),
                };
                let what = format!(
                    "flushing the watch of the checkout {} so that a request sees every change made before it: {why}",
                    checkout.head,
                );
                let head = checkout.head.clone();
                self.fail_waiting(|it| *it == (slot, number), &what, &head, out);
            }
        }
    }

    fn watched(
        &mut self,
        vfs: &mut Vfs,
        wall_ns: u64,
        completion: Completion,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Out>,
    ) {
        let Some(slot) = self.slot_of(completion.tag) else { return };
        let Some(checkout) = self.checkouts.get_mut(slot as usize).and_then(Option::as_deref_mut)
        else {
            return;
        };
        let done = match completion.done {
            Done::Journaled(mut batch) => {
                // A lost subtree under `target/` or `.git` is not rescanned, nor is an excluded
                // directory listed into.
                batch.files.retain(|it| checkout.keeps(it));
                batch.lost.retain(|(it, _)| checkout.keeps(it));
                Done::Journaled(batch)
            }
            Done::Entries(mut entries) => {
                entries.retain(|(it, _)| checkout.keeps(it));
                Done::Entries(entries)
            }
            done => done,
        };
        let watched =
            checkout.watcher.complete(
                vfs,
                Completion { tag: completion.tag, done },
                wall_ns,
                effects,
            );
        let head = &checkout.head;
        let mut ended = None;
        for it in watched {
            match it {
                Watched::Committed(commit) => {
                    out.push(Out::Moved { head: head.clone(), root: commit.root })
                }
                Watched::Lost { .. } | Watched::Scanned => {}
                Watched::Conflict { path } => {
                    crate::serve::said::logged!(WARN,
                        "blaze serve: {head}/{path} changed on disk and by a commit since; the head keeps the commit's"
                    );
                }
                Watched::Unreadable { path, error } if path.is_empty() => {
                    ended = Some(format!("the checkout {head} can't be read: {error}"));
                }
                Watched::Unreadable { path, error } => {
                    ended.get_or_insert_with(|| {
                        format!("{head}/{path} changed and can't be read: {error}")
                    });
                }
                Watched::Ended(error) => {
                    ended.get_or_insert_with(|| match error {
                        Some(error) => format!("the watch of the checkout {head} ended: {error}"),
                        None => format!("the watch of the checkout {head} was stopped"),
                    });
                }
            }
        }
        if let Some(what) = ended
            && let Some(mut gone) = self.checkouts[slot as usize].take()
        {
            gone.watcher.stop(effects);
            self.slots.remove(&gone.head);
            self.flushing.retain(|_, it| it.0 != slot);
            self.fail_waiting(|it| it.0 == slot, &what, &gone.head, out);
        }
    }

    /// Fails every held request with a wait `matches` says, naming `what`.
    fn fail_waiting(
        &mut self,
        matches: impl Fn(&(u32, u64)) -> bool,
        what: &str,
        head: &str,
        out: &mut Vec<Out>,
    ) {
        for held in self.held.extract_if(.., |it| it.waits.iter().any(&matches)) {
            answer(out, held.from, Some(ApiError::new(what, Some(head.to_owned()))), false);
        }
    }

    /// Hands on every held request whose every checkout's flush is told and committed what
    /// it was told before, and, if it runs user code, whose round outside views is in, oldest
    /// first. A request held for an older round waits for the newer one too.
    fn release(&mut self, out: &mut Vec<Out>) {
        let (checkouts, flushed) = (&self.checkouts, self.outside.flushed());
        let caught_up = |(slot, number): &(u32, u64)| {
            checkouts.get(*slot as usize).and_then(Option::as_deref).is_some_and(|it| {
                #[cfg(test)]
                if PLANT.get() == Plant::PassesBeforeTheLook && it.first == *number {
                    return true;
                }
                it.watcher.idle() && it.flushes.get(number).is_some_and(|told| *told)
            })
        };
        for held in self
            .held
            .extract_if(.., |it| (flushed || !it.outside) && it.waits.iter().all(&caught_up))
        {
            out.push(Out::Pass(Arrived::Request { from: held.from, request: held.request }));
        }
    }

    /// Drops the flushes told that no held request waits for.
    pub fn sweep(&mut self) {
        let held = &self.held;
        for (slot, checkout) in (0u32..).zip(&mut self.checkouts) {
            let Some(checkout) = checkout.as_deref_mut() else { continue };
            checkout.flushes.retain(|number, told| {
                !*told || held.iter().any(|it| it.waits.contains(&(slot, *number)))
            });
        }
    }

    /// Stops every watch.
    pub fn stop(&mut self, effects: &mut Vec<Effect>) {
        for checkout in self.checkouts.iter_mut().flatten() {
            checkout.watcher.stop(effects);
        }
    }

    /// The requests of `conn` that wait.
    pub fn of(&self, conn: Conn) -> impl Iterator<Item = RequestRef> + '_ {
        self.held.iter().filter(move |it| it.from.conn == conn).map(|it| it.from)
    }
}

fn answer(out: &mut Vec<Out>, to: RequestRef, error: Option<ApiError>, ok: bool) {
    if let Some(error) = error {
        out.push(Out::Answer(Reply { to, response: Response::Error(error) }));
    }
    out.push(Out::Answer(Reply { to, response: Response::Done { ok } }));
}

#[cfg(test)]
mod tests {
    use blaze_vfs::outside::Dirs;

    use super::*;

    /// The wall clock the tests' driver reads: a real epoch's, long after every stamp they
    /// answer with (`mtime_ns: 1`), so none of those is racy.
    const NOW_NS: u64 = 1_700_000_000_000_000_000;

    #[test]
    fn the_build_dir_and_git_are_out_of_the_view() {
        assert!(excluded("target"));
        assert!(excluded("target/debug/foo"));
        assert!(excluded(".git/HEAD"));
        assert!(excluded("vendor/x/.git/config"));
        assert!(!excluded("src/target.rs"));
        assert!(!excluded("targets/a.rs"));
        assert!(!excluded("crates/target/src/lib.rs"));
        assert!(!excluded(".cargo/config.toml"));
        assert!(!excluded("src/.gitignore.rs"));
    }

    #[test]
    fn what_waits_is_what_reads_a_checkout() {
        let head = |it: &str| ViewRef::Head(it.to_owned());
        let targets = blaze_api::Targets {
            packages: Vec::new(),
            profile: String::new(),
            goal: None,
            target: None,
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            cargo: None,
        };
        let build = Request::Build { view: head("/w"), targets: targets.clone() };
        assert_eq!(named(&build), vec!["/w"]);
        let what_if = Request::Check {
            view: ViewRef::WhatIf { base: Box::new(head("/w")), ops: Vec::new() },
            targets: targets.clone(),
        };
        assert_eq!(named(&what_if), vec!["/w"]);
        assert!(named(&Request::Build { view: head("ci/main"), targets }).is_empty());
        assert!(named(&Request::Status).is_empty());
        let diff = Request::Diff { from: head("/w"), to: head("/w") };
        assert_eq!(named(&diff), vec!["/w"]);
    }

    /// A table of its own, so tests that run at once each drive one.
    fn outside(tags: u64) -> (&'static Dirs, Outside<'static>) {
        let dirs: &'static Dirs = Box::leak(Box::new(Dirs::new()));
        (dirs, Outside::new(dirs, "/", tags))
    }

    #[test]
    fn a_request_that_runs_user_code_waits_for_the_round_outside_views() {
        let (dirs, outside) = outside(1 << 40);
        let mut heads = Heads::new(0, 1 << 32, 1 << 40, outside);
        let (mut vfs, _) = Vfs::recover(FileName::new("journal").unwrap(), 1 << 50, &[]);
        let (mut effects, mut out) = (Vec::new(), Vec::new());
        let targets = blaze_api::Targets {
            packages: Vec::new(),
            profile: String::new(),
            goal: None,
            target: None,
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            cargo: None,
        };
        let from = |id| RequestRef { conn: Conn(1), id };
        let build = |id| Arrived::Request {
            from: from(id),
            request: Request::Build {
                view: ViewRef::Head("ci/main".to_owned()),
                targets: targets.clone(),
            },
        };
        let passed = |out: &mut Vec<Out>| -> Vec<u64> {
            out.drain(..)
                .map(|it| match it {
                    Out::Pass(Arrived::Request { from, .. }) => from.id,
                    it => panic!("{it:?}"),
                })
                .collect()
        };

        heads.arrive(&vfs, NOW_NS, build(1), &mut effects, &mut out);
        assert_eq!(passed(&mut out), [1], "nothing is watched: the round is in at once");

        dirs.want(std::path::Path::new("/usr/include"));
        heads.arrive(&vfs, NOW_NS, build(2), &mut effects, &mut out);
        assert_eq!(passed(&mut out), [2], "a watch not yet confirmed has nothing to flush");
        let stat = effects
            .iter()
            .find_map(|it| match it {
                Effect::Stat { tag, .. } => Some(*tag),
                _ => None,
            })
            .expect("the new watch is confirmed by a stat");
        let meta = blaze_core::Meta { len: 0, dir: true, modified_ns: 0 };
        heads.complete(
            &mut vfs,
            NOW_NS,
            Completion { tag: stat, done: Done::Stat(Some(meta)) },
            &mut effects,
            &mut out,
        );
        effects.clear();

        heads.arrive(&vfs, NOW_NS, build(3), &mut effects, &mut out);
        let flush = effects
            .iter()
            .find_map(|it| match it {
                Effect::Flush { tag, .. } => Some(*tag),
                _ => None,
            })
            .expect("the confirmed watch is flushed");
        assert!(out.is_empty());
        assert_eq!(heads.waiting(), 1);
        heads.arrive(
            &vfs,
            NOW_NS,
            Arrived::Request { from: from(4), request: Request::Status },
            &mut effects,
            &mut out,
        );
        assert_eq!(passed(&mut out), [4], "what runs no user code waits for no round");

        heads.complete(
            &mut vfs,
            NOW_NS,
            Completion { tag: flush, done: Done::Flushed },
            &mut effects,
            &mut out,
        );
        assert_eq!(passed(&mut out), [3]);
        assert_eq!(heads.waiting(), 0);
    }

    #[test]
    fn what_runs_user_code_is_what_builds() {
        let view = || ViewRef::Head("ci/main".to_owned());
        let targets = blaze_api::Targets {
            packages: Vec::new(),
            profile: String::new(),
            goal: None,
            target: None,
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            cargo: None,
        };
        assert!(runs_user_code(&Request::Check { view: view(), targets: targets.clone() }));
        assert!(runs_user_code(&Request::Verify { view: view(), targets: targets.clone() }));
        let ahead = Request::WillTest {
            view: view(),
            filter: Vec::new(),
            targets: targets.clone(),
            wait_ms: 0,
        };
        assert!(runs_user_code(&ahead));
        let edit = Request::Edit {
            base: ContentId::of("heads test", b"root"),
            head: "/w".to_owned(),
            edits: Vec::new(),
        };
        assert!(
            runs_user_code(&edit),
            "an edit builds its view's proc macros and runs its build scripts first"
        );
        assert!(!runs_user_code(&Request::Trace { view: view(), targets }));
        assert!(!runs_user_code(&Request::Open { view: view() }));
        assert!(!runs_user_code(&Request::Status));
    }

    #[test]
    fn intent_reads_the_heads_it_builds_ahead_on_and_an_edit_reads_none() {
        let targets = || blaze_api::Targets {
            packages: Vec::new(),
            profile: String::new(),
            goal: None,
            target: None,
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            cargo: None,
        };
        let view = ViewRef::Head("/w".to_owned());
        let ahead = Request::WillTest { view, filter: Vec::new(), targets: targets(), wait_ms: 0 };
        assert_eq!(named(&ahead), ["/w"]);
        let save = Request::WillSave {
            ops: Vec::new(),
            head: "/w".to_owned(),
            test: true,
            targets: targets(),
            wait_ms: 0,
        };
        assert_eq!(named(&save), ["/w"]);
        let edit = Request::Edit {
            base: ContentId::of("heads test", b"root"),
            head: "/w".to_owned(),
            edits: Vec::new(),
        };
        assert!(named(&edit).is_empty());
    }

    /// A request on a checkout waits on a flush of its watch, and nothing is written into the
    /// checkout for it: no file of a cookie, no build directory.
    #[test]
    fn a_checkout_is_caught_up_with_by_a_flush_that_writes_nothing() {
        let mut heads = Heads::new(0, 1 << 32, 1 << 40, outside(1 << 40).1);
        let mut effects = Vec::new();
        let vfs = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]).0;
        let numbers: Vec<u64> =
            (0..40).map(|_| heads.flush(&vfs, "/w", NOW_NS, &mut effects).unwrap().1).collect();
        assert_eq!(numbers, (0..40).collect::<Vec<_>>());
        let watch = heads.checkouts[0].as_deref().unwrap().watch;
        let flushes = effects
            .iter()
            .filter(|it| matches!(it, Effect::Flush { watch: at, .. } if *at == watch))
            .count();
        assert_eq!(flushes, 39, "the first wait is the watcher's own look, no flush");
        assert!(
            !effects.iter().any(|it| matches!(it, Effect::Write { .. } | Effect::Remove { .. })),
            "a flush writes nothing into the checkout: {effects:?}"
        );
        let checkout = heads.checkouts[0].as_deref_mut().unwrap();
        for (number, told) in &mut checkout.flushes {
            *told = number % 3 != 1;
        }
        heads.sweep();
        assert_eq!(heads.checkouts[0].as_deref().unwrap().flushes.len(), 13);
    }

    /// The invariant: the request that starts a checkout's watch asks no flush and is handed on
    /// only once the watcher's own look (its start's sweep) has committed; a request that
    /// arrives while that look runs asks one flush and waits for it. Control: a first request
    /// handed on before the look commits.
    #[test]
    fn the_request_that_starts_a_watch_waits_for_its_look_and_asks_no_flush() {
        for planted in [false, true] {
            plant_passes_before_the_look(planted);
            let mut heads = Heads::new(0, 1 << 32, 1 << 40, outside(1 << 40).1);
            let mut vfs = Vfs::recover(FileName::new("journal").unwrap(), 1 << 50, &[]).0;
            let (mut effects, mut out) = (Vec::new(), Vec::new());
            let open = |id| Arrived::Request {
                from: RequestRef { conn: Conn(1), id },
                request: Request::Open { view: ViewRef::Head("/w".to_owned()) },
            };
            let passed = |out: &mut Vec<Out>| -> Vec<u64> {
                out.drain(..)
                    .filter_map(|it| match it {
                        Out::Pass(Arrived::Request { from, .. }) => Some(from.id),
                        _ => None,
                    })
                    .collect()
            };
            let flushes = |effects: &[Effect]| {
                effects.iter().filter(|it| matches!(it, Effect::Flush { .. })).count()
            };

            heads.arrive(&vfs, NOW_NS, open(1), &mut effects, &mut out);
            assert_eq!(flushes(&effects), 0, "the first request asks no flush: {effects:?}");
            let early = passed(&mut out);
            if planted {
                assert_eq!(early, [1], "the control hands it on before the look");
                plant_passes_before_the_look(false);
                continue;
            }
            assert!(early.is_empty(), "handed on before the watcher looked: {early:?}");
            heads.arrive(&vfs, NOW_NS, open(2), &mut effects, &mut out);
            assert_eq!(flushes(&effects), 1, "a request while the look runs asks one flush");
            assert!(passed(&mut out).is_empty());

            // The watcher's look answered as an empty directory; the journal stays open.
            let mut first = Vec::new();
            let mut flush = None;
            for _ in 0..100 {
                let asked: Vec<Effect> = effects.drain(..).collect();
                let mut answered = false;
                for effect in asked {
                    let (tag, done) = match effect {
                        Effect::Stamp { files, tag } => {
                            let mode = blaze_core::S_IFDIR;
                            let dir = blaze_core::Stamp { ino: 2, size: 0, mtime_ns: 1, ctime_ns: 1, mode };
                            (tag, Done::Stamped(files.iter().map(|_| dir).collect()))
                        }
                        Effect::Entries { tag, .. } => (tag, Done::Entries(Vec::new())),
                        Effect::ReadAll { tag, .. } => (tag, Done::Read(Vec::new())),
                        Effect::Flush { tag, .. } => {
                            flush = Some(tag);
                            continue;
                        }
                        _ => continue,
                    };
                    answered = true;
                    assert!(heads.owns(tag), "{tag:?}");
                    heads.complete(&mut vfs, NOW_NS, Completion { tag, done }, &mut effects, &mut out);
                    let now = passed(&mut out);
                    if !now.is_empty() {
                        let checkout = heads.checkouts[0].as_deref().unwrap();
                        assert!(checkout.watcher.idle(), "handed on with the look in flight");
                    }
                    first.extend(now);
                }
                if !answered {
                    break;
                }
            }
            assert_eq!(first, [1], "the first request goes on once the look commits, alone");
            let flush = flush.expect("the second request's flush");
            heads.complete(&mut vfs, NOW_NS, Completion { tag: flush, done: Done::Flushed }, &mut effects, &mut out);
            assert_eq!(passed(&mut out), [2]);
            assert_eq!(heads.waiting(), 0);
        }
    }
}
